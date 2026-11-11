//! Preuves de vie du registre d'annonce — TODO « Fiabiliser le registre et
//! l'anti-zombie multi-processus ».
//!
//! Avant correctif, le sweep déduisait la mort d'un process d'un **simple
//! échec de connexion sur `127.0.0.1`** : un serveur vivant lié à IPv6 ou à
//! une IP non bouclage perdait son annonce, et un port réutilisé par une
//! autre instance était déclaré vivant au nom de l'annonceur mort. Le
//! contrat corrigé :
//!
//! 1. l'annonce publie l'**adresse effective connectable** (`addr`) et une
//!    **identité d'instance vérifiable** (`instance`, également exposée par
//!    `/health` en en-tête `X-Instance-Id`) ;
//! 2. le sweep sonde l'adresse **publiée** (plus jamais le bouclage figé) ;
//! 3. une identité vérifiée = vivante ; une identité **différente** qui
//!    répond = preuve positive que l'annonceur ne possède plus le port →
//!    retrait ; un port sans réponse ou une identité invérifiable = **doute**
//!    → l'annonce est **conservée** (un refus TCP n'est pas une preuve de
//!    mort) ;
//! 4. `retract` est sérialisé entre process (verrou par nom) : l'annonce
//!    la plus récente ne peut plus être détruite par le retrait d'une
//!    instance plus ancienne ;
//! 5. `handlers_alive` du bilan de fin de vie est lu **après** le drain.

use litemcp::ServerBuilder;
use litemcp::antizombie::sweep_dir;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Dossier de test unique — les sweeps n'y voient que nos fichiers, jamais
/// le registre partagé de la machine.
fn unique_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    let dir = std::env::temp_dir().join(format!(
        "litemcp-regvie-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// PID étranger plausible (jamais le nôtre).
fn foreign_pid() -> u64 {
    u64::from(u32::MAX - 1)
}

/// Écrit une annonce au **format complet** (adresse effective + identité) —
/// PID étranger : le sweep la juge comme celle d'un autre process.
fn write_full_announcement(dir: &Path, name: &str, addr: &str, instance: u64) -> PathBuf {
    let port: u16 = addr.rsplit(':').next().unwrap().parse().unwrap();
    let doc = serde_json::json!({
        "name": name, "version": "1.0.0",
        "addr": addr, "port": port, "instance": instance,
        "pid": foreign_pid(),
        "exe": "ztest", "args": [], "cwd": ".", "ts": 0,
    });
    let path = dir.join(format!("{name}.json"));
    fs::write(&path, doc.to_string()).unwrap();
    path
}

/// Ports de **serveurs** de ce fichier de test — tranche fixe, disjointe des
/// ports de sonde : deux tests parallèles ne peuvent pas se voler le port
/// « mort » que l'autre est en train de sonder (course de harnais observée :
/// un serveur voisin prenait le port lâché → identité différente → usurpation
/// « prouvée » alors que le scénario testé était bien un port vide).
const SERVER_PORTS: std::ops::RangeInclusive<u16> = 23300..=23309;

/// Ports de **sonde** (annonces sans auditeur) — aucun test de ce fichier ne
/// les binde : un refus y reste un refus pendant toute la durée du sweep.
const PROBE_PORTS: std::ops::RangeInclusive<u16> = 23310..=23319;

/// Un port **fixe et non réattribuable** de la tranche serveur (sous les
/// plages éphémères) — pour réutiliser LE MÊME port entre deux serveurs.
fn grab_fixed_port() -> SocketAddr {
    for port in SERVER_PORTS {
        if let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", port)) {
            let addr = listener.local_addr().unwrap();
            drop(listener);
            return addr;
        }
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

/// Un port libre **jamais bindé par les tests de ce fichier** (annonce sans
/// aucun auditeur) — listener lâché immédiatement.
fn dead_port() -> SocketAddr {
    for port in PROBE_PORTS {
        if let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", port)) {
            let addr = listener.local_addr().unwrap();
            drop(listener);
            return addr;
        }
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

/// Lit l'identité d'instance exposée par la sonde `/health` — `None` si
/// l'en-tête `X-Instance-Id` est absent (serveur sans identité vérifiable).
fn health_instance_id(addr: SocketAddr) -> Option<u64> {
    let mut stream = TcpStream::connect(addr).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: probe\r\nConnection: close\r\n\r\n")
        .ok()?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw).ok()?;
    for line in raw.lines() {
        if let Some(value) = line
            .split_once(':')
            .filter(|(name, _)| name.trim().eq_ignore_ascii_case("x-instance-id"))
            .map(|(_, value)| value.trim())
        {
            return u64::from_str_radix(value, 16).ok();
        }
    }
    None
}

/// Monte un vrai serveur LiteMCP sur `port` fixe et attend qu'il serve.
fn spawn_server(
    addr: &str,
    tag: &str,
) -> (
    SocketAddr,
    litemcp::http::ServerHandle,
    std::thread::JoinHandle<std::io::Result<()>>,
) {
    let bound = ServerBuilder::new(&format!("ztest-regvie-{tag}"), "1.0.0")
        .tool("ping", "Sonner", |_args| {
            Ok(serde_json::json!({ "pong": true }))
        })
        .bind(addr)
        .expect("le port de la tranche de test doit être libre");
    let local = bound.local_addr().unwrap();
    let handle = bound.handle();
    let worker = std::thread::spawn(move || bound.serve());
    // Attente de service : la sonde /health doit répondre.
    for _ in 0..100 {
        if health_instance_id(local).is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    (local, handle, worker)
}

// ===========================================================================
// 1. Port sans réponse : DOUTE — l'annonce est conservée (jamais « reaped »)
// ===========================================================================

#[test]
fn annonce_sur_port_sans_reponse_est_conservee_en_doute() {
    let dir = unique_dir("doute");
    let dead = dead_port();
    let path = write_full_announcement(&dir, "sans-reponse", &dead.to_string(), 0x1122334455667788);
    let report = sweep_dir(&dir);
    assert_eq!(
        report.reaped, 0,
        "un refus TCP n'est pas une preuve de mort — rien ne doit être retiré : {report:?}"
    );
    assert!(
        path.exists(),
        "l'annonce d'un port sans réponse est conservée (doute) : {report:?}"
    );
    // Idempotent : le sweep suivant conserve aussi (jamais de retrait sans
    // preuve positive).
    let again = sweep_dir(&dir);
    assert_eq!(again.reaped, 0, "{again:?}");
    assert!(path.exists(), "{again:?}");
    let _ = fs::remove_dir_all(&dir);
}

// ===========================================================================
// 2. Port réutilisé par UNE AUTRE instance : usurpation prouvée → retrait
// ===========================================================================

#[test]
fn annonce_sur_port_reutilise_par_une_autre_instance_est_retiree() {
    let fixed = grab_fixed_port();
    let dir = unique_dir("reuse");

    // Instance 1 : vivante, identité lue sur /health.
    let (addr, handle1, worker1) = spawn_server(&fixed.to_string(), "reuse-1");
    let instance1 =
        health_instance_id(addr).expect("l'identité d'instance doit être exposée par /health");
    let path = write_full_announcement(&dir, "reuse", &addr.to_string(), instance1);

    // Vivante et identité vérifiée : conservée.
    let report = sweep_dir(&dir);
    assert_eq!(report.reaped, 0, "identité vérifiée = vivante : {report:?}");
    assert!(path.exists(), "{report:?}");

    // L'instance 1 s'arrête ; UNE AUTRE instance prend le MÊME port.
    handle1.shutdown();
    worker1.join().unwrap().unwrap();
    let (addr2, handle2, worker2) = spawn_server(&fixed.to_string(), "reuse-2");
    assert_eq!(
        addr2, addr,
        "le même port doit être repris : {addr2} vs {addr}"
    );
    let instance2 =
        health_instance_id(addr2).expect("l'identité de la seconde instance doit être exposée");
    assert_ne!(
        instance1, instance2,
        "deux instances distinctes ne partagent jamais d'identité"
    );

    // L'annonce (instance 1, PID étranger) désigne désormais un port occupé
    // par une AUTRE instance : usurpation prouvée → retirée.
    let report = sweep_dir(&dir);
    assert_eq!(
        report.reaped, 1,
        "l'identité qui répond (instance {instance2:x}) n'est pas celle annoncée ({instance1:x}) : {report:?}"
    );
    assert!(!path.exists(), "l'annonce usurpée est retirée : {report:?}");

    handle2.shutdown();
    worker2.join().unwrap().unwrap();
    let _ = fs::remove_dir_all(&dir);
}

// ===========================================================================
// 3. IPv6 : la sonde utilise l'adresse PUBLIÉE (plus jamais le bouclage figé)
// ===========================================================================

#[test]
fn annonce_ipv6_est_probee_sur_l_adresse_publiee_et_conservee() {
    // Bind IPv6 : indisponible sur certaines machines — test conditionnel.
    let Ok(bound) = ServerBuilder::new("ztest-regvie-v6", "1.0.0")
        .tool("ping", "Sonner", |_args| {
            Ok(serde_json::json!({ "pong": true }))
        })
        .bind("[::1]:0")
    else {
        eprintln!("IPv6 indisponible : test ignoré");
        return;
    };
    let dir = unique_dir("ipv6");
    let addr = bound.local_addr().unwrap();
    let handle = bound.handle();
    let worker = std::thread::spawn(move || bound.serve());
    let instance =
        health_instance_id(addr).expect("l'identité d'instance doit être exposée par /health");

    // Un sweep qui sondait 127.0.0.1 refuserait (le serveur n'écoute que
    // sur [::1]) et détruirait l'annonce d'une instance VIVANTE.
    let path = write_full_announcement(&dir, "v6", &addr.to_string(), instance);
    let report = sweep_dir(&dir);
    assert_eq!(
        report.reaped, 0,
        "un serveur vivant sur [::1] ne doit jamais perdre son annonce : {report:?}"
    );
    assert!(
        path.exists(),
        "l'annonce IPv6 vivante est conservée : {report:?}"
    );

    handle.shutdown();
    worker.join().unwrap().unwrap();
    let _ = fs::remove_dir_all(&dir);
}

// ===========================================================================
// 4. IP spécifique non bouclage : même contrat
// ===========================================================================

#[test]
fn bind_non_loopback_est_probee_sur_l_adresse_publiee_et_conservee() {
    // IP locale non bouclage découverte sans émettre de paquet (routage UDP) ;
    // aucune interface non bouclage (conteneur, sandbox) : test ignoré.
    let Some(local_ip) = std::net::UdpSocket::bind(("0.0.0.0", 0))
        .ok()
        .and_then(|socket| {
            socket
                .connect("192.0.2.1:9")
                .ok()
                .and_then(|_| socket.local_addr().ok())
        })
        .map(|addr| addr.ip())
        .filter(|ip| !ip.is_loopback() && ip.is_ipv4())
    else {
        eprintln!("aucune IP non bouclage : test ignoré");
        return;
    };
    let dir = unique_dir("nonloop");
    let Ok(bound) = ServerBuilder::new("ztest-regvie-ip", "1.0.0")
        .tool("ping", "Sonner", |_args| {
            Ok(serde_json::json!({ "pong": true }))
        })
        .bind(&format!("{local_ip}:0"))
    else {
        eprintln!("bind non bouclage refusé ({local_ip}) : test ignoré");
        return;
    };
    let addr = bound.local_addr().unwrap();
    let handle = bound.handle();
    let worker = std::thread::spawn(move || bound.serve());
    let instance =
        health_instance_id(addr).expect("l'identité d'instance doit être exposée par /health");

    // Une sonde 127.0.0.1 ne joindrait PAS un listener lié à une IP
    // spécifique : l'ancien comportement détruisait l'annonce d'un vivant.
    let path = write_full_announcement(&dir, "nonloop", &addr.to_string(), instance);
    let report = sweep_dir(&dir);
    assert_eq!(
        report.reaped, 0,
        "un serveur vivant sur {addr} ne doit jamais être jugé sur 127.0.0.1 : {report:?}"
    );
    assert!(path.exists(), "{report:?}");

    handle.shutdown();
    worker.join().unwrap().unwrap();
    let _ = fs::remove_dir_all(&dir);
}

// ===========================================================================
// 5. Bout en bout : une annonce RÉELLE publie addr + instance, cohérents
//    avec /health, et le retrait de fin de vie la supprime
// ===========================================================================

#[test]
fn l_annonce_reelle_publie_adresse_et_identite_puis_est_retiree() {
    let registry = litemcp::registry::registry_dir();
    fs::create_dir_all(&registry).unwrap();
    let name = format!(
        "ztest-regvie-e2e-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    );

    let bound = ServerBuilder::new(&name, "1.0.0")
        .tool("ping", "Sonner", |_args| {
            Ok(serde_json::json!({ "pong": true }))
        })
        .announce()
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = bound.local_addr().unwrap();
    let handle = bound.handle();
    let worker = std::thread::spawn(move || bound.serve());

    // Attente de service : la sonde /health doit répondre (le serveur met
    // quelques millisecondes à accepter).
    let mut instance = None;
    for _ in 0..200 {
        if let Some(id) = health_instance_id(addr) {
            instance = Some(id);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let instance =
        instance.expect("l'identité d'instance doit être exposée par /health (annonce active)");
    let path = registry.join(format!("{name}.json"));
    let body = fs::read_to_string(&path).expect("l'annonce réelle doit exister");
    let doc: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        doc["addr"]
            .as_str()
            .and_then(|a| a.parse::<SocketAddr>().ok()),
        Some(addr),
        "l'adresse effective publiée doit être celle du listener : {doc}"
    );
    assert_eq!(
        doc["instance"].as_u64(),
        Some(instance),
        "l'identité publiée doit être celle servie par /health : {doc}"
    );
    assert_eq!(doc["port"].as_u64(), Some(u64::from(addr.port())));

    // Fin de vie : le propriétaire retire SA propre annonce (verrou tenu).
    handle.shutdown();
    worker.join().unwrap().unwrap();
    assert!(
        !path.exists(),
        "le retrait de fin de vie supprime l'annonce"
    );
}

// ===========================================================================
// 6. handlers_alive est lu APRÈS le drain — un handler qui termine pendant
//    le drain n'est plus compté vivant
// ===========================================================================

#[test]
fn handler_terminant_pendant_le_drain_est_compte_zero() {
    let started = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&started);
    let bound = ServerBuilder::new("ztest-regvie-drain", "1.0.0")
        .legacy(true)
        .tool("lent", "Termine pendant le drain", move |_args| {
            signal.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(600));
            Ok(serde_json::json!({ "fini": true }))
        })
        .tool_timeout(Duration::from_secs(5))
        .anti_zombie()
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = bound.local_addr().unwrap();
    let handle = bound.handle();
    let worker = std::thread::spawn(move || bound.serve());

    // Le handler démarre (thread détaché, compteur à 1) et dort 600 ms. Le
    // client réessaie tant que le serveur n'accepte pas (démarrage).
    let client = std::thread::spawn(move || {
        let mut stream = None;
        for _ in 0..200 {
            match TcpStream::connect(addr) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        let mut stream = stream.expect("le serveur doit accepter les connexions");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"lent","arguments":{}}}"#;
        write!(
            stream,
            "POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    });
    // Attente BORNÉE du démarrage du handler : un échec de démarrage est
    // un diagnostic, jamais une boucle infinie.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !started.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "le handler n'a jamais démarré (serveur non servi ?)"
        );
        std::thread::sleep(Duration::from_millis(1));
    }

    // Arrêt pendant le vol : le drain attend la connexion — le handler
    // termine PENDANT le drain et libère son slot avant la lecture.
    handle.shutdown();
    let response = client.join().unwrap();
    assert!(response.contains("fini"), "le handler a servi : {response}");
    worker.join().unwrap().unwrap();

    let report = handle.zombie_report().unwrap();
    assert!(
        report.drained,
        "la connexion est partie avant l'échéance : {report:?}"
    );
    assert_eq!(
        report.handlers_alive, 0,
        "lu APRÈS le drain : le handler qui y a terminé n'est plus vivant : {report:?}"
    );
}

// ===========================================================================
// 7. Annonces/retraits concurrents : la plus récente ne peut pas être
//    détruite par le retrait d'une plus ancienne (sérialisation par verrou —
//    la preuve déterministe de la sérialisation vit dans les tests unitaires
//    de registry ; ici : stress d'interleavings réels, jamais de corruption)
// ===========================================================================

#[test]
fn annonces_et_retraits_concurrents_ne_detruisent_pas_la_plus_recente() {
    let registry = litemcp::registry::registry_dir();
    fs::create_dir_all(&registry).unwrap();
    let name = format!(
        "ztest-regvie-race-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    );
    let path = registry.join(format!("{name}.json"));
    let _ = fs::remove_file(&path);
    let dernier_port_b = *SERVER_PORTS.start();

    for tour in 0..32u32 {
        let port_a = SERVER_PORTS.start() + (tour % 10) as u16;
        let port_b = SERVER_PORTS.start() + ((tour + 7) % 10) as u16;
        // Instance A annonce, puis A se retire pendant que B annonce.
        litemcp::registry::announce(&name, "1.0.0", port_a);
        assert!(path.exists(), "tour {tour} : l'annonce A doit exister");

        let nom = name.clone();
        let retire = std::thread::spawn(move || {
            litemcp::registry::retract(&nom, port_a);
        });
        litemcp::registry::announce(&name, "1.0.0", port_b);
        retire.join().unwrap();

        // Verdict : fichier ABSENT (le retrait a gagné avant l'annonce de B)
        // ou fichier = annonce de B. Jamais une annonce de A qui survivrait
        // à B, jamais un JSON illisible, jamais un blocage.
        if path.exists() {
            let body = fs::read_to_string(&path).unwrap();
            let doc: serde_json::Value = serde_json::from_str(&body).expect("JSON valide");
            assert_eq!(
                doc["port"].as_u64(),
                Some(u64::from(port_b)),
                "tour {tour} : seule l'annonce la plus récente peut survivre : {doc}"
            );
        }
    }
    // Nettoyage (le retrait final de B, puis les résidus de verrou).
    litemcp::registry::retract(&name, dernier_port_b);
    let _ = fs::remove_file(&path);
    let _ = fs::remove_dir_all(registry.join(format!("{name}.lock")));
}

// ===========================================================================
// 8. Retrait : l'identité d'instance compte — le couple (PID, port) seul ne
//    prouve pas que le fichier appartient à l'instance qui s'arrête
// ===========================================================================

#[test]
fn le_retrait_d_une_instance_ne_supprime_pas_l_annonce_d_une_autre_du_meme_port() {
    let registry = litemcp::registry::registry_dir();
    fs::create_dir_all(&registry).unwrap();
    let name = format!(
        "ztest-regvie-retract-id-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    );
    let path = registry.join(format!("{name}.json"));
    let _ = fs::remove_file(&path);

    let bound = ServerBuilder::new(&name, "1.0.0")
        .announce()
        .bind("127.0.0.1:0")
        .expect("le port éphémère doit être libre");
    let addr = bound.local_addr().unwrap();
    let instance_a = {
        let body = fs::read_to_string(&path).expect("l'annonce de A doit exister");
        let doc: serde_json::Value = serde_json::from_str(&body).unwrap();
        doc["instance"]
            .as_u64()
            .expect("l'annonce publie une identité")
    };

    // Une AUTRE instance du même nom et du même port (redémarrage dans le
    // même process) a réécrit l'annonce : (pid, port) sont identiques, seule
    // l'identité d'instance distingue les deux annonceurs.
    let instance_b = instance_a.wrapping_add(1) | 1;
    litemcp::registry::announce_at(&name, "1.0.1", addr, instance_b);

    // Fin de vie de A : son retrait ne doit pas emporter l'annonce de B.
    drop(bound);
    assert!(
        path.exists(),
        "le retrait de A ne doit pas supprimer l'annonce de B (même pid, même port, autre instance)"
    );
    let body = fs::read_to_string(&path).expect("l'annonce de B doit survivre");
    let doc: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        doc["instance"].as_u64(),
        Some(instance_b),
        "l'annonce survivante est bien celle de B : {doc}"
    );

    let _ = fs::remove_file(&path);
}
