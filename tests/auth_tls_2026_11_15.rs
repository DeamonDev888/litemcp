//! TLS 1.3 serveur — tests **réels** sur le loopback (feature `tls`).
//!
//! Un client `rustls` (dev-dependency, `tls12` côté client **seulement**)
//! dialogue avec le serveur du crate :
//!
//! * poignée de main TLS 1.3 puis requête MCP `200` ;
//! * un client TLS 1.2 est refusé à la poignée de main, **aucun octet HTTP**
//!   ne sort ;
//! * un certificat substitué est refusé (autorité inconnue du client) ;
//! * l'épinglage refuse explicitement une empreinte changée
//!   ([`litemcp::auth::PinnedServer`]) ;
//! * l'empreinte publiée par le serveur est bien celle du DER servi.
#![cfg(feature = "tls")]

use litemcp::auth::PinnedServer;
use litemcp::prelude::*;
use litemcp::tls::TlsConfig;
use rustls::pki_types::{CertificateDer, ServerName};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

const PROTO: &str = "2026-07-28";

fn corps_appel() -> Vec<u8> {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "ping",
            "arguments": {},
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": PROTO,
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }
    })
    .to_string()
    .into_bytes()
}

fn requete_http(addr: SocketAddr, corps: &[u8]) -> Vec<u8> {
    let mut requete = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         MCP-Protocol-Version: {PROTO}\r\nMcp-Method: tools/call\r\nMcp-Name: ping\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        corps.len()
    )
    .into_bytes();
    requete.extend_from_slice(corps);
    requete
}

/// Serveur TLS 1.3 avec un certificat éphémère, sur un port libre.
struct BancTls {
    addr: SocketAddr,
    der: Vec<u8>,
    fingerprint: litemcp::tls::CertificateFingerprint,
    handle: litemcp::http::ServerHandle,
}

fn banc_tls() -> BancTls {
    let tls = TlsConfig::self_signed_ephemeral("localhost").expect("certificat éphémère");
    let der = tls.certificate_der().to_vec();
    let fingerprint = tls.fingerprint();
    let bound = ServerBuilder::new("tls-demo", "1.0.0")
        .tool("ping", "Ping", |_args| {
            Ok(serde_json::json!({ "ok": true }))
        })
        .tls(tls)
        .bind("127.0.0.1:0")
        .expect("bind TLS");
    let addr = bound.local_addr().unwrap();
    // L'empreinte publiée par le serveur est celle du certificat servi.
    assert_eq!(bound.certificate_fingerprint(), Some(fingerprint));
    let handle = bound.handle();
    std::thread::spawn(move || {
        let _ = bound.serve();
    });
    BancTls {
        addr,
        der,
        fingerprint,
        handle,
    }
}

impl Drop for BancTls {
    fn drop(&mut self) {
        self.handle.shutdown();
    }
}

fn config_client(
    der: &[u8],
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from(der.to_vec()))
        .expect("certificat racine de test");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(versions)
        .expect("versions de test")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(config)
}

fn flux_tls(
    addr: SocketAddr,
    config: Arc<rustls::ClientConfig>,
) -> std::io::Result<rustls::StreamOwned<rustls::ClientConnection, TcpStream>> {
    let name = ServerName::try_from("localhost").expect("nom de serveur");
    let connexion = rustls::ClientConnection::new(config, name)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let tcp = TcpStream::connect(addr)?;
    tcp.set_read_timeout(Some(Duration::from_secs(10)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(10)))?;
    Ok(rustls::StreamOwned::new(connexion, tcp))
}

#[test]
fn tls13_poignee_de_main_et_requete_mcp() {
    let banc = banc_tls();
    let config = config_client(&banc.der, &[&rustls::version::TLS13]);
    let mut flux = flux_tls(banc.addr, config).expect("connexion TCP");
    let requete = requete_http(banc.addr, &corps_appel());
    flux.write_all(&requete).expect("écriture TLS");
    flux.flush().expect("flush TLS");
    let mut reponse = Vec::new();
    flux.read_to_end(&mut reponse).expect("lecture TLS");
    let texte = String::from_utf8_lossy(&reponse);
    assert!(texte.starts_with("HTTP/1.1 200 OK"), "réponse : {texte}");
    assert!(texte.contains("\"ok\":true"), "corps : {texte}");
    // Le certificat servi est bien celui dont l'empreinte est publiée.
    assert_eq!(
        banc.fingerprint,
        litemcp::tls::CertificateFingerprint::of_der(&banc.der)
    );
}

#[test]
fn tls12_est_refuse_sans_aucun_octet_http() {
    let banc = banc_tls();
    // Client volontairement limité à TLS 1.2 (feature `tls12` du test).
    let config = config_client(&banc.der, &[&rustls::version::TLS12]);
    let mut flux = flux_tls(banc.addr, config).expect("connexion TCP");
    let requete = requete_http(banc.addr, &corps_appel());
    let ecriture = flux.write_all(&requete);
    assert!(ecriture.is_err(), "la poignée de main TLS 1.2 doit échouer");
    let mut octets = [0u8; 64];
    let lus = flux.read(&mut octets);
    assert!(
        matches!(lus, Err(_) | Ok(0)),
        "aucun octet HTTP ne doit sortir : {lus:?}"
    );
}

#[test]
fn un_certificat_substitue_est_refuse() {
    let banc = banc_tls();
    // Le client ne fait confiance qu'à un **autre** certificat (le serveur
    // présente celui du banc) : autorité inconnue, poignée de main refusée.
    let autre = TlsConfig::self_signed_ephemeral("localhost").expect("second certificat");
    let config = config_client(autre.certificate_der(), &[&rustls::version::TLS13]);
    let mut flux = flux_tls(banc.addr, config).expect("connexion TCP");
    let requete = requete_http(banc.addr, &corps_appel());
    let ecriture = flux.write_all(&requete);
    assert!(
        ecriture.is_err(),
        "un certificat substitué doit être refusé"
    );
    let mut octets = [0u8; 64];
    let lus = flux.read(&mut octets);
    assert!(matches!(lus, Err(_) | Ok(0)));
}

#[test]
fn l_epinglage_refuse_une_empreinte_changee() {
    let banc = banc_tls();
    // L'empreinte publiée, épinglée : identité acceptée.
    let pin = PinnedServer::new(&banc.fingerprint.to_hex()).expect("empreinte canonique");
    pin.verify(&banc.fingerprint.to_hex())
        .expect("même certificat");
    // Un redémarrage avec un nouveau certificat éphémère : refus explicite.
    let autre = TlsConfig::self_signed_ephemeral("localhost").expect("second certificat");
    let refus = pin.verify(&autre.fingerprint().to_hex());
    assert!(refus.is_err(), "une empreinte changée doit être refusée");
    // Et l'empreinte publiée est dérivée du DER, pas d'autre chose.
    assert_eq!(
        banc.fingerprint.to_hex().len(),
        "sha256:".len() + 64,
        "forme canonique"
    );
    assert_eq!(
        banc.fingerprint,
        litemcp::tls::CertificateFingerprint::of_der(&banc.der)
    );
}

#[test]
fn un_certificat_der_fourni_par_l_application_est_servi() {
    // L'application fournit son certificat (voie `from_der`) : le serveur
    // sert **ce** DER, et l'empreinte publiée le suit.
    let cle = rcgen::KeyPair::generate().expect("clé");
    let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).expect("params");
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "localhost");
    let certificat = params.self_signed(&cle).expect("auto-signé");
    let der = certificat.der().to_vec();
    let fourni = TlsConfig::from_der(der.clone(), cle.serialize_der()).expect("depuis DER");
    assert_eq!(
        fourni.fingerprint(),
        litemcp::tls::CertificateFingerprint::of_der(&der)
    );
    let bound = ServerBuilder::new("der-demo", "1.0.0")
        .tool("ping", "Ping", |_args| {
            Ok(serde_json::json!({ "ok": true }))
        })
        .tls(fourni)
        .bind("127.0.0.1:0")
        .expect("bind");
    let addr = bound.local_addr().unwrap();
    let handle = bound.handle();
    std::thread::spawn(move || {
        let _ = bound.serve();
    });
    let config = config_client(&der, &[&rustls::version::TLS13]);
    let mut flux = flux_tls(addr, config).expect("connexion TCP");
    flux.write_all(&requete_http(addr, &corps_appel()))
        .expect("écriture TLS");
    flux.flush().expect("flush TLS");
    let mut reponse = Vec::new();
    flux.read_to_end(&mut reponse).expect("lecture TLS");
    assert!(
        String::from_utf8_lossy(&reponse).starts_with("HTTP/1.1 200 OK"),
        "réponse : {}",
        String::from_utf8_lossy(&reponse)
    );
    handle.shutdown();
}
