//! Harnais commun de la campagne L4, point 0 (non-regression du chemin SANS
//! verificateur) — **meme fichier** compile dans l'arbre courant (0.12.0) et
//! dans le worktree `b350c16` (0.11.0).
//!
//! Il n'utilise que l'API presente dans les deux versions : `ServerBuilder`,
//! `ServerCore`, `zod`, `.legacy(true)`, `.max_response_bytes(..)`, `.tool_typed`.
//! Les scenarios filtres par appelant (0.12.0 seulement) vivent dans
//! `examples/bench_serialisation.rs`.
//!
//! `cargo run --release --example bench_point0_commun`
//!
//! Mesures : `tools/list` (20 outils a schema zod) et `tools/call` (petite
//! reponse), en mode `dispatch` (coeur seul) et `http-json` (aller-retour
//! HTTP reel keep-alive, en-tetes comprises) — latence mediane et octets
//! alloues par appel, comptes par un allocateur global.

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use litemcp::prelude::*;

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

fn instantane() -> (usize, usize) {
    (
        ALLOCS.load(Ordering::Relaxed),
        OCTETS_ALLOUES.load(Ordering::Relaxed),
    )
}

/// Empreinte FNV-1a 64 bits — compare des charges **octet pour octet** entre
/// deux arbres sans les afficher.
fn empreinte(octets: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for octet in octets {
        hash ^= u64::from(*octet);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// Registre de 20 outils a schema zod (charge de `tools/list` realiste).
fn constructeur() -> ServerBuilder {
    let mut builder = ServerBuilder::new("bench", "1.0").legacy(true);
    for index in 0..20 {
        let nom: &'static str = Box::leak(format!("outil_{index:02}").into_boxed_str());
        let description: &'static str = Box::leak(
            format!("Outil de demonstration numero {index} du catalogue de mesure")
                .into_boxed_str(),
        );
        builder = builder.tool_typed(
            nom,
            description,
            zod::object()
                .field("query", zod::string().min(2).max(64))
                .opt("limit", zod::integer().min(1).max(100))
                .strict(),
            |args| {
                let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
                let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10);
                Ok(serde_json::json!({ "echo": query, "limit": limit }))
            },
        );
    }
    builder.max_response_bytes(1024 * 1024)
}

fn enveloppe(corps: &[u8]) -> Vec<u8> {
    let mut requete = Vec::with_capacity(corps.len() + 160);
    requete.extend_from_slice(
        format!(
            "POST /mcp HTTP/1.1\r\nHost: bench\r\nAccept: application/json\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            corps.len()
        )
        .as_bytes(),
    );
    requete.extend_from_slice(corps);
    requete
}

fn chercher(paille: &[u8], aiguille: &[u8]) -> Option<usize> {
    paille
        .windows(aiguille.len())
        .position(|fenetre| fenetre == aiguille)
}

fn longueur_contenu(tete: &[u8]) -> usize {
    let texte = std::str::from_utf8(tete).expect("tete non UTF-8");
    let pos = texte
        .find("Content-Length: ")
        .expect("Content-Length absent");
    let reste = &texte[pos + "Content-Length: ".len()..];
    let chiffres: &str = reste
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap_or("");
    chiffres.parse().expect("Content-Length illisible")
}

fn echange_json(flux: &mut TcpStream, requete: &[u8], tampon: &mut Vec<u8>) {
    flux.write_all(requete).expect("ecriture requete");
    tampon.clear();
    let mut provisoire = [0u8; 16 * 1024];
    let fin_tete = loop {
        let lu = flux.read(&mut provisoire).expect("lecture tete");
        assert!(lu > 0, "connexion fermee (tete)");
        tampon.extend_from_slice(&provisoire[..lu]);
        if let Some(pos) = chercher(tampon, b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let attendu = fin_tete + longueur_contenu(&tampon[..fin_tete]);
    while tampon.len() < attendu {
        let lu = flux.read(&mut provisoire).expect("lecture corps");
        assert!(lu > 0, "connexion fermee (corps)");
        tampon.extend_from_slice(&provisoire[..lu]);
    }
}

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
        "{nom:<22} {mode:<10} {iters:>6} {moy:>10.2} {med:>10.2} {allocs:>12.2} {octets:>14.0}",
        iters = m.iters,
        moy = m.moyenne_us,
        med = m.mediane_us,
        allocs = m.allocs_appel,
        octets = m.octets_appel,
    );
    println!(
        "RESULT|{nom}|{mode}|{}|{:.3}|{:.3}|{:.2}|{:.0}",
        m.iters, m.moyenne_us, m.mediane_us, m.allocs_appel, m.octets_appel
    );
}

fn main() {
    println!("=== litemcp - point 0 : chemin SANS verificateur (release) ===");
    let coeur = constructeur().core();
    let lie = constructeur().bind("127.0.0.1:0").expect("bind serveur de bench");
    let adresse = lie.local_addr().expect("adresse locale");
    std::thread::spawn(move || {
        let _ = lie.serve();
    });

    let corps_liste = br#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#.to_vec();
    let corps_appel = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"outil_00","arguments":{"query":"abc","limit":3}}}"#.to_vec();
    let requete_liste = enveloppe(&corps_liste);
    let requete_appel = enveloppe(&corps_appel);

    println!(
        "\n{:<22} {:<10} {:>6} {:>10} {:>10} {:>12} {:>14}",
        "scenario", "mode", "appels", "moy us", "med us", "allocs/appel", "octets/appel"
    );

    let mut flux = TcpStream::connect(adresse).expect("connexion bench");
    flux.set_nodelay(true).expect("nodelay");
    let mut tampon = Vec::with_capacity(64 * 1024);

    // Sanitaires : les deux reponses sont conformes avant toute mesure.
    for (corps, requete, marqueur) in [
        (&corps_liste, &requete_liste, &b"\"tools\""[..]),
        (&corps_appel, &requete_appel, &b"\"result\""[..]),
    ] {
        let reponse = coeur.dispatch(corps).expect("dispatch sanitaire");
        assert!(
            chercher(&reponse, marqueur).is_some(),
            "dispatch inattendu : {}",
            String::from_utf8_lossy(&reponse[..120.min(reponse.len())])
        );
        echange_json(&mut flux, requete, &mut tampon);
        assert!(
            tampon.starts_with(b"HTTP/1.1 200") && chercher(&tampon, marqueur).is_some(),
            "reponse HTTP inattendue : {}",
            String::from_utf8_lossy(&tampon[..120.min(tampon.len())])
        );
    }

    // Empreintes des charges servies : elles doivent etre IDENTIQUES entre
    // 0.11.0 et l'arbre courant (aucun octet ne doit changer sur le fil).
    let reponse_liste = coeur.dispatch(&corps_liste).expect("dispatch empreinte liste");
    let reponse_appel = coeur.dispatch(&corps_appel).expect("dispatch empreinte appel");
    println!(
        "EMPREINTE|tools/list|{}|{:016x}",
        reponse_liste.len(),
        empreinte(&reponse_liste)
    );
    println!(
        "EMPREINTE|tools/call|{}|{:016x}",
        reponse_appel.len(),
        empreinte(&reponse_appel)
    );

    let m = mesurer(2_000, 200, || {
        let reponse = coeur.dispatch(&corps_liste).expect("dispatch liste");
        black_box(reponse.len());
        drop(reponse);
    });
    afficher("tools/list (20)", "dispatch", &m);
    let m = mesurer(2_000, 200, || {
        echange_json(&mut flux, &requete_liste, &mut tampon);
        black_box(tampon.len());
    });
    afficher("tools/list (20)", "http-json", &m);

    let m = mesurer(2_000, 200, || {
        let reponse = coeur.dispatch(&corps_appel).expect("dispatch appel");
        black_box(reponse.len());
        drop(reponse);
    });
    afficher("tools/call (petit)", "dispatch", &m);
    let m = mesurer(2_000, 200, || {
        echange_json(&mut flux, &requete_appel, &mut tampon);
        black_box(tampon.len());
    });
    afficher("tools/call (petit)", "http-json", &m);

    println!("\nReproductibilite : iterations figees, build release, machine au repos.");
}
