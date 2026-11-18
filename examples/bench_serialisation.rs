//! Benchmark de sérialisation et du plafond de réponse — reproductible,
//! zéro dépendance externe.
//!
//! Contexte de la campagne : `max_response_bytes` est vérifié **après**
//! sérialisation complète de la réponse, et la réponse HTTP (comme
//! l'événement SSE) recopie la charge utile dans le tampon d'assemblage.
//! Ce harnais mesure l'existant **avant** modification, puis la même campagne
//! **après**, pour décider en connaissance de cause.
//!
//! `cargo run --release --example bench_serialisation`
//!
//! Méthodologie :
//!
//! - **allocations** : un allocateur global de comptage (nombre d'appels à
//!   l'allocateur et octets alloués, deltas par appel mesuré) — il couvre tout
//!   le processus, harnais compris ; le harnais étant identique avant/après,
//!   les deltas restent attribuables ;
//! - **latence** : moyenne et médiane par appel (`Instant`, release obligatoire) ;
//! - quatre scénarios `tools/call` contre un plafond configuré à **1 Mio** :
//!   petite réponse (~270 o), réponse **exactement au plafond**,
//!   **dépassée d'un octet**, **très grande** (16 Mio sérialisés) ;
//! - trois modes : `dispatch` (le cœur seul — sérialisation + plafond, sans
//!   transport), `http-json` (aller-retour HTTP réel keep-alive — assembleur
//!   en-têtes/corps inclus), `http-sse` (mono-événement `text/event-stream`,
//!   encadrement `data:` inclus, une connexion par appel).
//!
//! Les sanitaires intégrés vérifient à chaque scénario que la réponse reste
//! conforme (plafond respecté, erreur `-32603` quand attendue) : un chiffre
//! mesuré sur un comportement rompu ne serait qu'un mensonge rapide.

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use litemcp::auth::{AuthContext, AuthError, ProofMethod, VerifiedRequest, Verifier};
use litemcp::{ServerBuilder, ServerCore, zod};
use serde_json::{Value, json};

/// Plafond de la campagne : 1 Mio (configurable, le défaut du crate est 16
/// Mio). Un plafond modeste rend les scénarios « exactement au plafond » et
/// « très grande réponse » mesurables en quelques secondes.
const PLAFOND: usize = 1024 * 1024;

/// Taille totale sérialisée du scénario « très grande réponse » : 16 Mio,
/// soit 16× le plafond de la campagne.
const TRES_GRAND: usize = 16 * 1024 * 1024;

// ------------------------------------------------------------------
// Allocateur de comptage — mesure les allocations de tout le processus.
// ------------------------------------------------------------------

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static OCTETS_ALLOUES: AtomicUsize = AtomicUsize::new(0);

struct Compteur;

unsafe impl GlobalAlloc for Compteur {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        OCTETS_ALLOUES.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        OCTETS_ALLOUES.fetch_add(new_size.saturating_sub(layout.size()), Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Compteur = Compteur;

/// Instantané des compteurs d'allocation (appels, octets cumulés).
fn instantane() -> (usize, usize) {
    (
        ALLOCS.load(Ordering::Relaxed),
        OCTETS_ALLOUES.load(Ordering::Relaxed),
    )
}

// ------------------------------------------------------------------
// Serveur et requêtes de la campagne.
// ------------------------------------------------------------------

fn constructeur(plafond: usize) -> ServerBuilder {
    ServerBuilder::new("bench", "1.0")
        // Profil legacy (comme les échanges raw-HTTP de tests/http.rs) : le
        // harnais mesure la sérialisation et l'assemblage de la réponse —
        // identiques dans les deux profils — sans la gramaille d'en-têtes
        // de routage du profil strict.
        .legacy(true)
        .tool("blob", "Rend une chaîne de n caractères", |args| {
            let n = args.get("n").and_then(Value::as_u64).unwrap_or(0) as usize;
            Ok(json!({ "data": "x".repeat(n) }))
        })
        .max_response_bytes(plafond)
}

/// Corps JSON-RPC d'un `tools/call` sur l'outil `blob` avec n caractères.
fn corps(n: usize) -> Vec<u8> {
    json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "blob", "arguments": { "n": n } }
    })
    .to_string()
    .into_bytes()
}

/// Requête HTTP JSON complète (keep-alive implicite HTTP/1.1).
fn enveloppe_json(corps_req: &[u8]) -> Vec<u8> {
    format!(
        "POST /mcp HTTP/1.1\r\nHost: bench\r\nAccept: application/json\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        corps_req.len(),
        std::str::from_utf8(corps_req).expect("corps UTF-8"),
    )
    .into_bytes()
}

/// Requête HTTP demandant **uniquement** `text/event-stream` — déclenche le
/// chemin SSE request-scoped mono-événement (sans jeton de progression).
fn enveloppe_sse(corps_req: &[u8]) -> Vec<u8> {
    format!(
        "POST /mcp HTTP/1.1\r\nHost: bench\r\nAccept: text/event-stream\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        corps_req.len(),
        std::str::from_utf8(corps_req).expect("corps UTF-8"),
    )
    .into_bytes()
}

/// Calibre `n` pour que la réponse sérialisée fasse **exactement** `cible`
/// octets. Le résultat d'un `tools/call` porte la charge deux fois
/// (`content[0].text` + `structuredContent`) : ~2 octets par caractère — la
/// pente est **mesurée** (sécante) plutôt qu'assumée, puis affinée pas à pas.
/// La calibration d'une cible au-delà du plafond passe par un cœur illimité
/// (`plafond = 0`).
fn calibrer(coeur: &ServerCore, cible: usize) -> usize {
    let enveloppe = {
        let reponse = coeur.dispatch(&corps(0)).expect("dispatch de calibration");
        assert!(
            serde_json::from_slice::<Value>(&reponse)
                .expect("JSON de calibration")
                .get("result")
                .is_some(),
            "la calibration suppose une réponse de succès"
        );
        reponse.len()
    };
    let cible = cible as i64;
    // Estimation initiale : enveloppe + ~2 octets par caractère.
    let mut n = ((cible - enveloppe as i64) / 2).max(0) as usize;
    let mut n_precedent = 0usize;
    let mut longueur_precedente = enveloppe as i64;
    for _ in 0..24 {
        let reponse = coeur.dispatch(&corps(n)).expect("dispatch de calibration");
        let longueur = reponse.len() as i64;
        if longueur == cible {
            return n;
        }
        if longueur < enveloppe as i64 + 1024 {
            // Enveloppe d'erreur (plafond dépassé pendant la calibration) :
            // sa taille ne dit rien de la charge — redescendre franchement.
            n = n.saturating_sub(512);
            n_precedent = 0;
            longueur_precedente = enveloppe as i64;
            continue;
        }
        // Sécante : pente locale en octets par caractère.
        let pente = if n != n_precedent {
            (longueur - longueur_precedente) / (n as i64 - n_precedent as i64)
        } else {
            2
        }
        .max(1);
        let correction = (cible - longueur) / pente;
        n_precedent = n;
        longueur_precedente = longueur;
        n = (n as i64 + correction).max(0) as usize;
    }
    panic!("calibration vers {cible} octets impossible");
}

// ------------------------------------------------------------------
// Client HTTP minimal (lecture cadrée, sans dépendance).
// ------------------------------------------------------------------

fn chercher(paille: &[u8], aiguille: &[u8]) -> Option<usize> {
    paille
        .windows(aiguille.len())
        .position(|fenetre| fenetre == aiguille)
}

fn longueur_contenu(tete: &[u8]) -> usize {
    let texte = std::str::from_utf8(tete).expect("tête non UTF-8");
    let pos = texte
        .find("Content-Length: ")
        .expect("Content-Length absent de la tête");
    let reste = &texte[pos + "Content-Length: ".len()..];
    let chiffres: &str = reste
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap_or("");
    chiffres.parse().expect("Content-Length illisible")
}

/// Aller-retour JSON keep-alive sur une connexion réutilisée : écrit la
/// requête, lit la tête puis exactement `Content-Length` octets de corps.
/// Le tampon est réutilisé d'un appel à l'autre (zéro allocation client).
fn echange_json(flux: &mut TcpStream, requete: &[u8], tampon: &mut Vec<u8>) {
    flux.write_all(requete).expect("écriture requête");
    tampon.clear();
    let mut provisoire = [0u8; 16 * 1024];
    let fin_tete = loop {
        let lu = flux.read(&mut provisoire).expect("lecture tête");
        assert!(lu > 0, "connexion fermée (tête)");
        tampon.extend_from_slice(&provisoire[..lu]);
        if let Some(pos) = chercher(tampon, b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let attendu = fin_tete + longueur_contenu(&tampon[..fin_tete]);
    while tampon.len() < attendu {
        let lu = flux.read(&mut provisoire).expect("lecture corps");
        assert!(lu > 0, "connexion fermée (corps)");
        tampon.extend_from_slice(&provisoire[..lu]);
    }
}

/// Aller-retour SSE : une connexion par appel (le serveur répond
/// `Connection: close`), lecture jusqu'à la fermeture.
fn echange_sse(adresse: SocketAddr, requete: &[u8], tampon: &mut Vec<u8>) {
    let mut flux = TcpStream::connect(adresse).expect("connexion sse");
    flux.set_nodelay(true).expect("nodelay");
    flux.write_all(requete).expect("écriture requête");
    tampon.clear();
    flux.read_to_end(tampon).expect("lecture sse");
    assert!(!tampon.is_empty(), "aucun octet reçu en SSE");
}

// ------------------------------------------------------------------
// Mesure.
// ------------------------------------------------------------------

struct Mesure {
    iters: u32,
    moyenne_us: f64,
    mediane_us: f64,
    allocs_appel: f64,
    octets_appel: f64,
}

fn mesurer(iters: u32, echauffement: u32, mut appel: impl FnMut()) -> Mesure {
    for _ in 0..echauffement {
        appel();
    }
    let (allocs_avant, octets_avant) = instantane();
    let mut durees = Vec::with_capacity(iters as usize);
    for _ in 0..iters {
        let depart = Instant::now();
        appel();
        durees.push(depart.elapsed());
    }
    let (allocs_apres, octets_apres) = instantane();
    durees.sort();
    Mesure {
        iters,
        moyenne_us: durees.iter().sum::<Duration>().as_secs_f64() / f64::from(iters) * 1e6,
        mediane_us: durees[iters as usize / 2].as_secs_f64() * 1e6,
        allocs_appel: (allocs_apres - allocs_avant) as f64 / f64::from(iters),
        octets_appel: (octets_apres - octets_avant) as f64 / f64::from(iters),
    }
}

fn afficher(nom: &str, mode: &str, m: &Mesure) {
    println!(
        "{nom:<28} {mode:<10} {iters:>6} {:>10.2} {:>10.2} {:>12.2} {:>14.0}",
        m.moyenne_us,
        m.mediane_us,
        m.allocs_appel,
        m.octets_appel,
        iters = m.iters,
    );
    println!(
        "RESULT|{nom}|{mode}|{}|{:.3}|{:.3}|{:.2}|{:.0}",
        m.iters, m.moyenne_us, m.mediane_us, m.allocs_appel, m.octets_appel,
    );
}

// ------------------------------------------------------------------
// Catalogues : 50 et 200 outils, chemin ouvert et chemin filtré.
// ------------------------------------------------------------------

/// Tailles de catalogue mesurées.
const CATALOGUES: [usize; 2] = [50, 200];

/// Droits du catalogue de mesure : l'outil `i` exige `groupe{i % 2}`.
const GROUPES: [&str; 2] = ["groupe0", "groupe1"];

/// Vérificateur de la campagne : la clé est l'en-tête `x-cle`, les droits
/// sont fixes — aucun coût de signature, la mesure porte sur le catalogue.
struct VerificateurDeBench;

impl Verifier for VerificateurDeBench {
    fn verify(&self, request: &VerifiedRequest<'_>) -> Result<AuthContext, AuthError> {
        let Some((_, cle)) = request
            .headers
            .iter()
            .find(|(nom, _)| nom.eq_ignore_ascii_case("x-cle"))
        else {
            return Err(AuthError::new("cle_absente"));
        };
        match *cle {
            // Deux clés aux droits différents : la large voit les deux
            // groupes, l'étroite un seul (la moitié du catalogue).
            "large" => Ok(AuthContext::new("large", ProofMethod::Application).with_rights(GROUPES)),
            "etroite" => {
                Ok(AuthContext::new("etroite", ProofMethod::Application).with_rights([GROUPES[0]]))
            }
            _ => Err(AuthError::new("cle_inconnue")),
        }
    }
}

/// Catalogue de `n` outils à schéma zod, droits alternés ; le vérificateur
/// n'est installé que pour le chemin filtré (sans lui, `dispatch` refuse).
fn constructeur_catalogue(n: usize, avec_verificateur: bool) -> ServerBuilder {
    let mut builder = ServerBuilder::new("bench-catalogue", "1.0").legacy(true);
    if avec_verificateur {
        builder = builder.verifier(Arc::new(VerificateurDeBench));
    }
    for index in 0..n {
        let nom: &'static str = Box::leak(format!("outil_{index:03}").into_boxed_str());
        let description: &'static str = Box::leak(
            format!("Outil numero {index} du catalogue de mesure des chemins chauds")
                .into_boxed_str(),
        );
        builder = builder
            .tool_typed(
                nom,
                description,
                zod::object()
                    .field("query", zod::string().min(2).max(64))
                    .opt("limit", zod::integer().min(1).max(100))
                    .strict(),
                |args| {
                    let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                    Ok(json!({ "echo": query }))
                },
            )
            .require_right(nom, GROUPES[index % 2]);
    }
    builder.max_response_bytes(16 * 1024 * 1024)
}

/// Nombre d'éléments publiés par un `tools/list` sérialisé.
fn nombre_outils(reponse: &[u8]) -> usize {
    let value: Value = serde_json::from_slice(reponse).expect("JSON de catalogue");
    value
        .get("result")
        .and_then(|result| result.get("tools"))
        .and_then(Value::as_array)
        .map_or(0, Vec::len)
}

/// Mesure les catalogues ouvert et filtré par appelant (mode `dispatch`).
fn catalogues() {
    let corps = br#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#.to_vec();
    for taille in CATALOGUES {
        let coeur_ouvert = constructeur_catalogue(taille, false).core();
        let coeur = constructeur_catalogue(taille, true).core();
        let entetes_large = [("x-cle", "large")];
        let entetes_etroite = [("x-cle", "etroite")];

        // Sanitaires : le nombre d'outils publiés est celui attendu, et les
        // deux clés ne voient pas la même chose.
        let ouverte = coeur_ouvert.dispatch(&corps).expect("dispatch catalogue");
        let large = coeur.dispatch_verified(&VerifiedRequest::new(
            "POST",
            "/mcp",
            "bench",
            &entetes_large,
            &corps,
        ));
        let etroite = coeur.dispatch_verified(&VerifiedRequest::new(
            "POST",
            "/mcp",
            "bench",
            &entetes_etroite,
            &corps,
        ));
        assert_eq!(nombre_outils(&ouverte), taille, "catalogue ouvert");
        assert_eq!(nombre_outils(&large.body), taille, "clé large");
        assert_eq!(
            nombre_outils(&etroite.body),
            taille.div_ceil(2),
            "clé étroite"
        );

        let marqueur = match taille {
            50 => "tools/list 50",
            200 => "tools/list 200",
            _ => "tools/list ?",
        };
        let m = mesurer(1_000, 100, || {
            let reponse = coeur_ouvert.dispatch(&corps).expect("dispatch catalogue");
            black_box(reponse.len());
        });
        afficher(marqueur, "ouvert", &m);
        let m = mesurer(500, 50, || {
            let reponse = coeur.dispatch_verified(&VerifiedRequest::new(
                "POST",
                "/mcp",
                "bench",
                &entetes_large,
                &corps,
            ));
            black_box(reponse.body.len());
        });
        afficher(marqueur, "filtre-large", &m);
        let m = mesurer(500, 50, || {
            let reponse = coeur.dispatch_verified(&VerifiedRequest::new(
                "POST",
                "/mcp",
                "bench",
                &entetes_etroite,
                &corps,
            ));
            black_box(reponse.body.len());
        });
        afficher(marqueur, "filtre-etroit", &m);
    }
}

/// Schéma `schemars` volumineux : le parcours de détection des défauts et
/// des entiers (`has_defaults`/`has_integers`) était refait à chaque appel.
/// 120 propriétés, la moitié avec défaut, quelques-unes entières.
#[cfg(feature = "schemars")]
fn schema_schemars(avec_defauts: bool, etiquette: &str) {
    use litemcp::schemars::from_json_schema;

    let mut proprietes = serde_json::Map::new();
    let mut arguments = serde_json::Map::new();
    for index in 0..120 {
        let nom = format!("champ_{index:03}");
        let mut schema = if index % 2 == 0 {
            json!({ "type": "integer", "minimum": 0, "maximum": 1000 })
        } else {
            json!({ "type": "string", "minLength": 1 })
        };
        if avec_defauts {
            schema["default"] = if index % 2 == 0 {
                json!(index)
            } else {
                json!("v")
            };
        }
        proprietes.insert(nom.clone(), schema);
        if index % 3 == 0 {
            arguments.insert(
                nom,
                if index % 2 == 0 {
                    json!(index)
                } else {
                    json!("v")
                },
            );
        }
    }
    let document = json!({
        "type": "object",
        "properties": proprietes,
        "additionalProperties": false,
    });
    let schema = from_json_schema(document);
    let defauts = {
        use litemcp::zod::Schema;
        schema.has_defaults()
    };
    assert_eq!(defauts, avec_defauts, "défauts publiés : {etiquette}");
    let coeur = ServerBuilder::new("bench-schema", "1.0")
        .legacy(true)
        .tool_typed("doc", "Outil à schéma volumineux", schema, |args| {
            Ok(json!({ "champs": args.as_object().map_or(0, serde_json::Map::len) }))
        })
        .max_response_bytes(1024 * 1024)
        .core();
    let corps = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "doc", "arguments": arguments },
    })
    .to_string()
    .into_bytes();
    let reply = coeur.dispatch(&corps).expect("dispatch schéma");
    let value: Value = serde_json::from_slice(&reply).expect("JSON");
    assert!(
        value.get("result").is_some(),
        "appel conforme attendu : {value}"
    );
    let m = mesurer(2_000, 200, || {
        let reply = coeur.dispatch(&corps).expect("dispatch schéma");
        black_box(reply.len());
    });
    afficher(etiquette, "dispatch", &m);
}

fn main() {
    println!("=== litemcp — bench sérialisation / plafond de réponse (release) ===");
    println!(
        "plafond de la campagne : {PLAFOND} o (1 Mio) · « très grande » : {TRES_GRAND} o (16 Mio) · \
         allocations = processus entier (harnais inclus, constant avant/après)"
    );

    let coeur = constructeur(PLAFOND).core();
    let coeur_illimite = constructeur(0).core();

    // Calibration des tailles exactes.
    let n_exact = calibrer(&coeur, PLAFOND);
    let n_plus_un = n_exact + 1;
    let n_grand = calibrer(&coeur_illimite, TRES_GRAND);
    println!(
        "calibration : exact au plafond n={n_exact} · dépassé n={n_plus_un} · 16 Mio n={n_grand}"
    );

    // Serveur HTTP réel (même plafond), démarrage vérifié hors mesure.
    let lie = constructeur(PLAFOND)
        .bind("127.0.0.1:0")
        .expect("bind serveur de bench");
    let adresse = lie.local_addr().expect("adresse locale");
    std::thread::spawn(move || {
        let _ = lie.serve();
    });
    {
        let mut flux = TcpStream::connect(adresse).expect("connexion démarrage");
        let demarrage = enveloppe_json(
            &json!({"jsonrpc":"2.0","id":0,"method":"tools/list"})
                .to_string()
                .into_bytes(),
        );
        let mut tampon = Vec::new();
        echange_json(&mut flux, &demarrage, &mut tampon);
        assert!(
            tampon.starts_with(b"HTTP/1.1 200"),
            "démarrage : {}",
            String::from_utf8_lossy(&tampon[..300.min(tampon.len())])
        );
    }

    struct Scenario {
        nom: &'static str,
        corps: Vec<u8>,
        iters: u32,
        echauffement: u32,
        attendu: Attendu,
    }

    enum Attendu {
        Succes,
        ErreurPlafond,
    }

    let scenarios = [
        Scenario {
            nom: "petite (~270 o)",
            corps: corps(100),
            iters: 3_000,
            echauffement: 300,
            attendu: Attendu::Succes,
        },
        Scenario {
            nom: "exacte au plafond (1 Mio)",
            corps: corps(n_exact),
            iters: 150,
            echauffement: 10,
            attendu: Attendu::Succes,
        },
        Scenario {
            nom: "dépassée d'un octet (+1)",
            corps: corps(n_plus_un),
            iters: 150,
            echauffement: 10,
            attendu: Attendu::ErreurPlafond,
        },
        Scenario {
            nom: "très grande (16 Mio)",
            corps: corps(n_grand),
            iters: 20,
            echauffement: 3,
            attendu: Attendu::ErreurPlafond,
        },
    ];

    println!(
        "\n{:<28} {:<10} {:>6} {:>10} {:>10} {:>12} {:>14}",
        "scénario", "mode", "appels", "moy µs", "méd µs", "allocs/appel", "octets/appel"
    );

    // Connexion keep-alive partagée par les modes http-json + son tampon.
    let mut flux = TcpStream::connect(adresse).expect("connexion bench");
    flux.set_nodelay(true).expect("nodelay");
    let mut tampon = Vec::with_capacity(PLAFOND + 4096);

    for scenario in &scenarios {
        let corps_req = &scenario.corps;
        let requete_json = enveloppe_json(corps_req);
        let requete_sse = enveloppe_sse(corps_req);

        // --- mode dispatch (cœur seul) ---
        // Sanitaire : la réponse du cœur est conforme au plafond.
        {
            let reponse = coeur.dispatch(corps_req).expect("dispatch");
            match scenario.attendu {
                Attendu::Succes => {
                    let v: Value = serde_json::from_slice(&reponse).expect("JSON");
                    assert!(v.get("result").is_some(), "succès attendu : {v}");
                }
                Attendu::ErreurPlafond => {
                    let v: Value = serde_json::from_slice(&reponse).expect("JSON");
                    assert_eq!(
                        v["error"]["code"],
                        -32603,
                        "erreur plafond attendue pour {} : {}",
                        scenario.nom,
                        String::from_utf8_lossy(&reponse[..100.min(reponse.len())])
                    );
                }
            }
            if scenario.nom.contains("exacte") {
                assert_eq!(reponse.len(), PLAFOND, "réponse exacte au plafond attendue");
            }
        }
        let m = mesurer(scenario.iters, scenario.echauffement, || {
            let reponse = coeur.dispatch(corps_req).expect("dispatch");
            black_box(reponse.len());
            drop(reponse);
        });
        afficher(scenario.nom, "dispatch", &m);

        // --- mode http-json (keep-alive) ---
        echange_json(&mut flux, &requete_json, &mut tampon);
        assert!(
            tampon.starts_with(b"HTTP/1.1 200"),
            "{:?}",
            &tampon[..60.min(tampon.len())]
        );
        if scenario.nom.contains("exacte") {
            let fin_tete = chercher(&tampon, b"\r\n\r\n").expect("tête") + 4;
            assert_eq!(
                longueur_contenu(&tampon[..fin_tete]),
                PLAFOND,
                "corps exactement au plafond attendu"
            );
        }
        if matches!(scenario.attendu, Attendu::ErreurPlafond) {
            assert!(
                chercher(&tampon, b"-32603").is_some(),
                "erreur -32603 attendue dans la réponse HTTP"
            );
        }
        let m = mesurer(scenario.iters, scenario.echauffement, || {
            echange_json(&mut flux, &requete_json, &mut tampon);
            black_box(tampon.len());
        });
        afficher(scenario.nom, "http-json", &m);

        // --- mode http-sse (mono-événement, une connexion par appel) ---
        echange_sse(adresse, &requete_sse, &mut tampon);
        assert!(
            tampon.starts_with(b"HTTP/1.1 200") && chercher(&tampon, b"data: ").is_some(),
            "événement SSE attendu : {:?}",
            &tampon[..80.min(tampon.len())]
        );
        match scenario.attendu {
            Attendu::ErreurPlafond => {
                assert!(
                    chercher(&tampon, b"-32603").is_some(),
                    "erreur -32603 attendue dans l'événement SSE"
                );
            }
            Attendu::Succes => {}
        }
        let m = mesurer(scenario.iters, scenario.echauffement, || {
            echange_sse(adresse, &requete_sse, &mut tampon);
            black_box(tampon.len());
        });
        afficher(scenario.nom, "http-sse", &m);
    }

    println!(
        "\nReproductibilité : itérations/échauffements figés, compilation release identique, \
         machine au repos conseillée — comparer les deltas avant/après sur la même machine."
    );

    // Catalogues (point 1 du lot L4) : mêmes réserves de mesure, mode `dispatch`.
    println!("\nCatalogues — droits alternés par outil, vérificateur sans coût de signature :");
    catalogues();

    // Schéma volumineux (point 3) : détection défauts/entiers hors requête.
    #[cfg(feature = "schemars")]
    {
        println!("\nSchéma `schemars` volumineux — détection des défauts/entiers :");
        schema_schemars(false, "tools/call schéma 120");
        schema_schemars(true, "tools/call schéma+défauts");
    }

    // Attribution du coût de `tool_timeout` (point 0) : le chemin borné passe
    // par un fil de handler ; `.no_tool_timeout()` exécute dans le fil de
    // connexion, comme la 0.11.0. Même outil, même corps, même transport.
    {
        println!("\nCoût de `tool_timeout` — même outil, chemin non borné :");
        let sans_borne = constructeur(PLAFOND).no_tool_timeout().core();
        let corps_appel = corps(100);
        let reponse = sans_borne.dispatch(&corps_appel).expect("dispatch");
        assert!(chercher(&reponse, b"\"result\"").is_some());
        let m = mesurer(3_000, 300, || {
            let reponse = sans_borne.dispatch(&corps_appel).expect("dispatch");
            black_box(reponse.len());
        });
        afficher("tools/call petit", "dispatch", &m);
        let serveur_sans_borne = constructeur(PLAFOND).no_tool_timeout();
        let lie = serveur_sans_borne.bind("127.0.0.1:0").expect("bind");
        let adresse = lie.local_addr().expect("adresse");
        std::thread::spawn(move || {
            let _ = lie.serve();
        });
        let mut flux = TcpStream::connect(adresse).expect("connexion");
        flux.set_nodelay(true).expect("nodelay");
        let mut tampon = Vec::with_capacity(4096);
        let requete = enveloppe_json(&corps_appel);
        echange_json(&mut flux, &requete, &mut tampon);
        assert!(tampon.starts_with(b"HTTP/1.1 200"));
        let m = mesurer(3_000, 300, || {
            echange_json(&mut flux, &requete, &mut tampon);
            black_box(tampon.len());
        });
        afficher("tools/call petit", "http-json-sans-borne", &m);
    }
}
