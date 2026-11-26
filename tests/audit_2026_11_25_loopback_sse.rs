//! Régressions de l'audit du 2026-11-25 (partie 1, P1).
//!
//! * **B2** — l'exception loopback même-origine exigeait un préfixe
//!   `http://` en dur : sous TLS, un navigateur même-origine (`Origin:
//!   https://127.0.0.1:PORT` cohérent avec `Host`) était rejeté en 403
//!   « origin not allowed ». Le préfixe attendu est désormais celui du
//!   schéma **réel** du listener (`http` ou `https` selon `.tls(…)`) ;
//!   le reste de l'exception (cohérence Host/listener, `Sec-Fetch-Site`,
//!   `Origin: null`) est inchangé.
//! * **B9** — un `id` malformé (`null`, booléen, tableau) portant un
//!   `progressToken` ouvrait un flux SSE `200 OK` **avant** le rejet
//!   `-32600` du noyau, alors que la même requête sans jeton recevait
//!   `400 Bad Request`. Le flux ne doit s'ouvrir que si l'`id` est une
//!   chaîne ou un nombre.
//!
//! B2 dialogue avec un vrai serveur TLS (feature `tls`, client `rustls`
//! en dev-dependency) ; B9 reste sur du TCP brut, sans feature.

use litemcp::ServerBuilder;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

// ===========================================================================
// B9 — flux SSE et `id` malformé (TCP brut, sans feature)
// ===========================================================================

/// Serveur strict (profil par défaut : `id: null` rejeté en -32600 + 400)
/// bindé sur le loopback, avec l'outil appelé par les corps de test.
fn spawn_strict() -> SocketAddr {
    let bound = ServerBuilder::new("sse-demo", "1.0.0")
        .tool("x", "Outil de démonstration", |_args| {
            Ok(serde_json::json!({ "ok": true }))
        })
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = bound.local_addr().unwrap();
    std::thread::spawn(move || bound.serve().unwrap());
    addr
}

/// POST /mcp acceptant **uniquement** `text/event-stream` ; renvoie la
/// réponse brute (statut, en-têtes, corps).
fn post_sse(addr: SocketAddr, corps: &str) -> String {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let requete = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAccept: text/event-stream\r\n\
         Content-Type: application/json\r\nMCP-Protocol-Version: 2026-07-28\r\n\
         Mcp-Method: tools/call\r\nMcp-Name: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{corps}",
        addr.port(),
        corps.len()
    );
    stream.write_all(requete.as_bytes()).unwrap();
    let mut brute = String::new();
    stream.read_to_string(&mut brute).unwrap();
    brute
}

/// Un `id` malformé (`null`, booléen, tableau) portant un `progressToken`
/// doit recevoir le `400 Bad Request` du rejet `-32600`, en réponse **bornée**
/// (`Content-Length`) — jamais un flux SSE `200 OK` ouvert avant le verdict.
#[test]
fn b9_id_malforme_avec_progress_token_ne_ouvre_pas_de_flux() {
    let addr = spawn_strict();
    for id in ["null", "true", "[]"] {
        let corps = format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"x","_meta":{{"progressToken":1}}}}}}"#
        );
        let reponse = post_sse(addr, &corps);
        assert!(
            reponse.starts_with("HTTP/1.1 400 Bad Request"),
            "id={id} : le flux SSE ne doit pas s'ouvrir sur un id malformé : {reponse}"
        );
        assert!(
            reponse.contains("-32600"),
            "id={id} : erreur -32600 attendue : {reponse}"
        );
        assert!(
            reponse.contains("Content-Length:"),
            "id={id} : réponse bornée attendue, pas un flux sans longueur : {reponse}"
        );
        assert!(
            !reponse.starts_with("HTTP/1.1 200 OK"),
            "id={id} : un flux 200 ne doit pas être ouvert : {reponse}"
        );
    }
}

/// Garde inverse : une requête **bien formée** (id numérique, jeton de
/// progression) ouvre toujours le flux SSE — tête `200 OK` sans
/// `Content-Length`, verdict porté par l'événement final.
#[test]
fn b9_le_flux_s_ouvre_toujours_pour_un_id_bien_forme() {
    let addr = spawn_strict();
    let corps = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"x","arguments":{},"_meta":{"progressToken":1,"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}"#;
    let reponse = post_sse(addr, corps);
    assert!(
        reponse.starts_with("HTTP/1.1 200 OK"),
        "le flux doit s'ouvrir pour une requête bien formée : {reponse}"
    );
    assert!(
        !reponse.contains("Content-Length:"),
        "un flux ouvert n'annonce pas de Content-Length : {reponse}"
    );
    assert!(
        reponse.contains("Content-Type: text/event-stream"),
        "type de média du flux attendu : {reponse}"
    );
    assert!(
        reponse.contains("data: "),
        "événement final attendu : {reponse}"
    );
    assert!(
        reponse.contains("\"ok\":true"),
        "le verdict doit être porté par l'événement final : {reponse}"
    );
}

// ===========================================================================
// B2 — exception loopback même-origine sous TLS (feature `tls`)
// ===========================================================================

#[cfg(feature = "tls")]
mod b2_tls {
    use litemcp::ServerBuilder;
    use litemcp::http::ServerHandle;
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

    /// Serveur TLS 1.3 (certificat éphémère) bindé sur `127.0.0.1:0`.
    struct BancTls {
        addr: SocketAddr,
        der: Vec<u8>,
        handle: ServerHandle,
    }

    fn banc_tls() -> BancTls {
        let tls = TlsConfig::self_signed_ephemeral("localhost").expect("certificat éphémère");
        let der = tls.certificate_der().to_vec();
        let bound = ServerBuilder::new("tls-origin", "1.0.0")
            .tool("ping", "Ping", |_args| {
                Ok(serde_json::json!({ "ok": true }))
            })
            .tls(tls)
            .bind("127.0.0.1:0")
            .expect("bind TLS");
        let addr = bound.local_addr().unwrap();
        let handle = bound.handle();
        std::thread::spawn(move || {
            let _ = bound.serve();
        });
        BancTls { addr, der, handle }
    }

    impl Drop for BancTls {
        fn drop(&mut self) {
            self.handle.shutdown();
        }
    }

    fn config_client(der: &[u8]) -> Arc<rustls::ClientConfig> {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from(der.to_vec()))
            .expect("certificat racine de test");
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        Arc::new(
            rustls::ClientConfig::builder_with_provider(provider)
                .with_protocol_versions(&[&rustls::version::TLS13])
                .expect("versions de test")
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    }

    /// POST /mcp sur le serveur TLS avec `Host` et `Origin` contrôlés ;
    /// renvoie la réponse brute.
    fn echange(banc: &BancTls, host: &str, origin: &str) -> String {
        let name = ServerName::try_from("localhost").expect("nom de serveur");
        let connexion = rustls::ClientConnection::new(config_client(&banc.der), name)
            .map_err(|error| std::io::Error::other(error.to_string()))
            .expect("connexion TLS");
        let tcp = TcpStream::connect(banc.addr).expect("connexion TCP");
        tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        tcp.set_write_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut flux = rustls::StreamOwned::new(connexion, tcp);
        let corps = corps_appel();
        let requete = format!(
            "POST /mcp HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\n\
             Content-Type: application/json\r\nMCP-Protocol-Version: {PROTO}\r\n\
             Mcp-Method: tools/call\r\nMcp-Name: ping\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n",
            corps.len()
        );
        flux.write_all(requete.as_bytes()).expect("écriture TLS");
        flux.write_all(&corps).expect("écriture du corps");
        flux.flush().expect("flush TLS");
        let mut reponse = Vec::new();
        flux.read_to_end(&mut reponse).expect("lecture TLS");
        String::from_utf8_lossy(&reponse).into_owned()
    }

    /// Positif — un client navigateur même-origine sur TLS porte
    /// `Origin: https://127.0.0.1:PORT` cohérent avec `Host` : la requête
    /// doit être traitée (pas de 403 « origin not allowed ») et l'origine
    /// autorisée échoée en CORS.
    #[test]
    fn b2_origine_https_meme_origine_est_acceptee_sous_tls() {
        let banc = banc_tls();
        let port = banc.addr.port();
        let host = format!("127.0.0.1:{port}");
        let origin = format!("https://127.0.0.1:{port}");
        let reponse = echange(&banc, &host, &origin);
        assert!(
            reponse.starts_with("HTTP/1.1 200 OK"),
            "origine même-origine https:// refusée : {reponse}"
        );
        assert!(
            reponse.contains("\"ok\":true"),
            "la requête doit être traitée : {reponse}"
        );
        assert!(
            reponse.contains(&format!("Access-Control-Allow-Origin: {origin}")),
            "l'origine autorisée doit être échoée : {reponse}"
        );
    }

    /// Négatif — un schéma `http://` sur un listener TLS n'est pas le schéma
    /// du serveur : refus 403 (l'exception suit le schéma **réel**).
    #[test]
    fn b2_origine_http_sur_listener_tls_est_refusee() {
        let banc = banc_tls();
        let port = banc.addr.port();
        let host = format!("127.0.0.1:{port}");
        let reponse = echange(&banc, &host, &format!("http://127.0.0.1:{port}"));
        assert!(
            reponse.starts_with("HTTP/1.1 403 Forbidden"),
            "un schéma http:// sur un listener https doit être refusé : {reponse}"
        );
    }

    /// Négatif — `Origin` https d'un hôte étranger (Host = listener) :
    /// cohérence Host/listener absente, refus 403 (garde DNS-rebinding).
    #[test]
    fn b2_origine_https_cross_host_est_refusee() {
        let banc = banc_tls();
        let port = banc.addr.port();
        let host = format!("127.0.0.1:{port}");
        let reponse = echange(&banc, &host, "https://192.0.2.10:8443");
        assert!(
            reponse.starts_with("HTTP/1.1 403 Forbidden"),
            "une origine étrangère doit rester refusée : {reponse}"
        );
    }
}
