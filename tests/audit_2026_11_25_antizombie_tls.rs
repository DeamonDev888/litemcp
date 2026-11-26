//! Audit 2026-11-25, constat B4 (partie 1, P1) — sonde d'identité du sweep
//! anti-zombie et serveurs TLS.
//!
//! Avant correctif, `probe_identity` ouvrait toujours un `TcpStream` nu et
//! écrivait `GET /health` **en clair** : toute annonce écrite par un serveur
//! `.tls(...)` (elle porte `certificateFingerprint`, écrit par
//! `registry::announce_at_with`) restait à jamais `undecided` — jamais
//! `alive`, et une usurpation n'était jamais prouvée (`reaped = 0`).
//!
//! Le contrat corrigé (feature `tls`) :
//!
//! 1. une annonce TLS avec la **bonne empreinte** et la bonne identité est
//!    sondée par un client rustls qui épingle l'empreinte → vivante
//!    (`alive`), pas `undecided` ;
//! 2. une annonce dont l'empreinte **ne correspond pas** au certificat servi
//!    reste en doute — l'identité qui répond (fût-elle la bonne) ne prouve
//!    plus rien sans le certificat annoncé : jamais de faux vivant ;
//! 3. un port repris par une **autre instance TLS** (autre identité) sous le
//!    certificat annoncé — le cas du redémarrage avec un certificat
//!    persistant — est une usurpation prouvée : l'annonce est purgée
//!    (`reaped`).
//!
//! Un port repris par une instance servant un **autre** certificat reste en
//! doute (échec d'épinglage) : le doute conserve l'annonce, la preuve
//! positive d'usurpation exige l'identité — même philosophie que la sonde
//! claire.

#![cfg(feature = "tls")]

use litemcp::ServerBuilder;
use litemcp::antizombie::sweep_dir;
use litemcp::tls::TlsConfig;
use rustls::pki_types::{CertificateDer, ServerName};
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Dossier de test unique — les sweeps n'y voient que nos fichiers, jamais
/// le registre partagé de la machine.
fn unique_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    let dir = std::env::temp_dir().join(format!(
        "litemcp-aztls-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// PID étranger plausible (jamais le nôtre) : le sweep juge l'annonce comme
/// celle d'un autre process.
fn foreign_pid() -> u64 {
    u64::from(u32::MAX - 1)
}

/// Dossier de registre partagé, prêt pour le verrou du nom : le retrait
/// d'une usurpation (`reaped`) passe par `with_name_lock`, qui valide le
/// dossier réel avant toute section critique — sous Unix, `create_dir_all`
/// seul le créerait en `0755` (umask) : le test pose lui-même le mode exigé.
fn preparer_registre() {
    let registry = litemcp::registry::registry_dir();
    fs::create_dir_all(&registry).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&registry, fs::Permissions::from_mode(0o700)).unwrap();
    }
}

/// Écrit une annonce au **format complet** d'un serveur TLS — adresse
/// effective, identité, `authRequired` et `certificateFingerprint` (le champ
/// écrit par `registry::announce_at_with` quand le serveur est `.tls(...)`)
/// — PID étranger : le sweep la juge comme celle d'un autre process.
fn write_tls_announcement(
    dir: &Path,
    name: &str,
    addr: SocketAddr,
    instance: u64,
    fingerprint: &str,
) -> PathBuf {
    let doc = serde_json::json!({
        "name": name, "version": "1.0.0",
        "addr": addr.to_string(), "port": addr.port(), "instance": instance,
        "authRequired": false, "certificateFingerprint": fingerprint,
        "pid": foreign_pid(),
        "exe": "ztest", "args": [], "cwd": ".", "ts": 0,
    });
    let path = dir.join(format!("{name}.json"));
    fs::write(&path, doc.to_string()).unwrap();
    path
}

/// Ports de ce fichier — tranche fixe sous les plages éphémères, disjointe
/// de celles d'`antizombie.rs` (23100–23149) et de `registre_vie.rs`
/// (23000–23499) : deux fichiers de test ne peuvent pas se voler le port
/// qu'un scénario de reprise doit réutiliser. Le PID décale la tranche pour
/// deux exécutions parallèles ; repli éphémère si elle est occupée.
fn grab_fixed_port() -> SocketAddr {
    let base = 23500 + (std::process::id() % 60) as u16 * 8;
    for port in base..base + 8 {
        if let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", port)) {
            let addr = listener.local_addr().unwrap();
            drop(listener);
            return addr;
        }
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

/// Configuration client des tests — TLS 1.3, le certificat du serveur pour
/// seule racine (l'épinglage de la sonde, lui, vit dans le sweep).
fn config_client(der: &[u8]) -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from(der.to_vec()))
        .expect("certificat racine de test");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("versions de test")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(config)
}

/// Lit l'identité d'instance exposée par `GET /health` **sur TLS** —
/// `None` si le serveur ne répond pas encore (démarrage) : sert aussi
/// d'attente de service. Le certificat (`der`) est celui du serveur sondé.
fn tls_health(addr: SocketAddr, der: &[u8]) -> Option<u64> {
    let name = ServerName::try_from("localhost").expect("nom de serveur");
    let connexion = rustls::ClientConnection::new(config_client(der), name)
        .map_err(|error| std::io::Error::other(error.to_string()))
        .ok()?;
    let tcp = TcpStream::connect(addr).ok()?;
    tcp.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    tcp.set_write_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut flux = rustls::StreamOwned::new(connexion, tcp);
    flux.write_all(b"GET /health HTTP/1.1\r\nHost: probe\r\nConnection: close\r\n\r\n")
        .ok()?;
    flux.flush().ok()?;
    // Lecture bornée : le serveur ferme la session TCP **sans** close_notify
    // TLS après `Connection: close` — `read_to_string` rendrait
    // `UnexpectedEof` alors que la réponse est complète. On lit donc jusqu'à
    // fermeture/erreur, en gardant ce qui a été reçu.
    let mut octets = Vec::new();
    let mut tampon = [0u8; 4096];
    loop {
        match flux.read(&mut tampon) {
            Ok(0) | Err(_) => break,
            Ok(n) => octets.extend_from_slice(&tampon[..n]),
        }
    }
    let raw = String::from_utf8_lossy(&octets);
    for line in raw.lines() {
        if let Some(value) = line
            .split_once(':')
            .filter(|(champ, _)| champ.trim().eq_ignore_ascii_case("x-instance-id"))
            .map(|(_, valeur)| valeur.trim())
        {
            return u64::from_str_radix(value, 16).ok();
        }
    }
    None
}

/// Attend (borné) que le serveur TLS serve `/health` et rend son identité.
fn attendre_sante(addr: SocketAddr, der: &[u8]) -> u64 {
    for _ in 0..200 {
        if let Some(id) = tls_health(addr, der) {
            return id;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("le serveur TLS {addr} n'a jamais répondu sur /health");
}

/// Monte un vrai serveur LiteMCP TLS 1.3 sur `addr` et rend sa poignée.
fn spawn_tls_server(
    addr: &str,
    tls: TlsConfig,
    tag: &str,
) -> (
    SocketAddr,
    litemcp::http::ServerHandle,
    std::thread::JoinHandle<std::io::Result<()>>,
) {
    let bound = ServerBuilder::new(&format!("ztest-aztls-{tag}"), "1.0.0")
        .tool("ping", "Sonner", |_args| {
            Ok(serde_json::json!({ "pong": true }))
        })
        .tls(tls)
        .bind(addr)
        .expect("le bind TLS doit réussir");
    let local = bound.local_addr().unwrap();
    let handle = bound.handle();
    let worker = std::thread::spawn(move || bound.serve());
    (local, handle, worker)
}

// ===========================================================================
// 1. Bonne empreinte + bonne identité : vivante — plus jamais `undecided`
// ===========================================================================

#[test]
fn annonce_tls_avec_bonne_empreinte_est_vivante() {
    let dir = unique_dir("vivant");
    let tls = TlsConfig::self_signed_ephemeral("localhost").expect("certificat éphémère");
    let der = tls.certificate_der().to_vec();
    let empreinte = tls.fingerprint().to_hex();
    let (addr, handle, worker) = spawn_tls_server("127.0.0.1:0", tls, "vivant");
    let instance = attendre_sante(addr, &der);
    let path = write_tls_announcement(&dir, "tls-vivant", addr, instance, &empreinte);

    let report = sweep_dir(&dir);
    assert_eq!(
        report.alive, 1,
        "empreinte épinglée + identité vérifiée : l'annonce TLS est vivante : {report:?}"
    );
    assert_eq!(
        report.undecided, 0,
        "une annonce TLS sondée en TLS ne doit plus rester indécise : {report:?}"
    );
    assert_eq!(
        report.reaped, 0,
        "une annonce vivante n'est jamais retirée : {report:?}"
    );
    assert!(
        path.exists(),
        "l'annonce vivante reste en place : {report:?}"
    );

    handle.shutdown();
    worker.join().unwrap().unwrap();
    let _ = fs::remove_dir_all(&dir);
}

// ===========================================================================
// 2. Empreinte ne correspondant PAS au certificat servi : doute — pas de
//    faux vivant (l'identité qui répond ne prouve rien sans le certificat)
// ===========================================================================

#[test]
fn annonce_tls_avec_empreinte_erronee_reste_en_doute() {
    let dir = unique_dir("doute");
    let tls = TlsConfig::self_signed_ephemeral("localhost").expect("certificat éphémère");
    let der = tls.certificate_der().to_vec();
    // Empreinte d'un AUTRE certificat : l'épinglage doit refuser la poignée
    // de main — même si l'identité qui répond est celle annoncée.
    let autre = TlsConfig::self_signed_ephemeral("localhost").expect("second certificat");
    let mauvaise_empreinte = autre.fingerprint().to_hex();
    let (addr, handle, worker) = spawn_tls_server("127.0.0.1:0", tls, "doute");
    let instance = attendre_sante(addr, &der);
    let path = write_tls_announcement(&dir, "tls-doute", addr, instance, &mauvaise_empreinte);

    let report = sweep_dir(&dir);
    assert_eq!(
        report.alive, 0,
        "une empreinte non servie ne prouve JAMAIS la vie (pas de faux vivant) : {report:?}"
    );
    assert_eq!(
        report.undecided, 1,
        "l'échec d'épinglage est un doute, pas une preuve : {report:?}"
    );
    assert_eq!(
        report.reaped, 0,
        "jamais de retrait sans preuve positive : {report:?}"
    );
    assert!(
        path.exists(),
        "l'annonce douteuse est conservée : {report:?}"
    );

    handle.shutdown();
    worker.join().unwrap().unwrap();
    let _ = fs::remove_dir_all(&dir);
}

// ===========================================================================
// 3. Port repris par une autre instance TLS (autre identité, même
//    certificat annoncé) : usurpation prouvée → l'annonce est purgée
// ===========================================================================

#[test]
fn annonce_tls_usurpee_par_une_autre_instance_est_retiree() {
    // Le retrait passe par le verrou du nom (registre partagé réel) : ce
    // dossier doit exister et être valide.
    preparer_registre();
    let fixed = grab_fixed_port();
    let dir = unique_dir("usurpe");
    // LE MÊME certificat pour les deux instances — le cas du redémarrage
    // avec un certificat persistant (`from_pem`/`from_der`) : seule
    // l'identité d'instance distingue l'annonceur du repreneur.
    let tls = TlsConfig::self_signed_ephemeral("localhost").expect("certificat partagé");
    let der = tls.certificate_der().to_vec();
    let empreinte = tls.fingerprint().to_hex();

    // Instance 1 : vivante, identité lue sur /health (TLS).
    let (addr, handle1, worker1) = spawn_tls_server(&fixed.to_string(), tls.clone(), "usurpe-1");
    let instance1 = attendre_sante(addr, &der);
    let path = write_tls_announcement(&dir, "tls-usurpe", addr, instance1, &empreinte);

    // Vivante et vérifiée (empreinte + identité) : conservée.
    let report = sweep_dir(&dir);
    assert_eq!(
        report.alive, 1,
        "l'instance légitime répond sous le certificat annoncé : {report:?}"
    );
    assert!(
        path.exists(),
        "l'annonce légitime reste en place : {report:?}"
    );

    // L'instance 1 s'arrête ; UNE AUTRE instance (autre identité) reprend
    // le MÊME port avec LE MÊME certificat.
    handle1.shutdown();
    worker1.join().unwrap().unwrap();
    let (addr2, handle2, worker2) = spawn_tls_server(&fixed.to_string(), tls, "usurpe-2");
    assert_eq!(
        addr2, addr,
        "le même port doit être repris : {addr2} vs {addr}"
    );
    let instance2 = attendre_sante(addr2, &der);
    assert_ne!(
        instance1, instance2,
        "deux instances distinctes ne partagent jamais d'identité"
    );

    // Le port publié répond (sous le certificat annoncé) avec UNE AUTRE
    // identité : usurpation prouvée → l'annonce est retirée.
    let report = sweep_dir(&dir);
    assert_eq!(
        report.reaped, 1,
        "l'identité qui répond ({instance2:x}) n'est pas celle annoncée ({instance1:x}) : {report:?}"
    );
    assert_eq!(
        report.undecided, 0,
        "l'usurpation est prouvée, ce n'est pas un doute : {report:?}"
    );
    assert!(!path.exists(), "l'annonce usurpée est retirée : {report:?}");

    handle2.shutdown();
    worker2.join().unwrap().unwrap();
    let _ = fs::remove_dir_all(&dir);
}
