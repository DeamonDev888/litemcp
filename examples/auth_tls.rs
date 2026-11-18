//! Serveur MCP **signé + TLS 1.3** — `cargo run --features tls,auth-sign --example auth_tls`.
//!
//! Le programme, entièrement sur le loopback et sans écrire de clé sur disque :
//!
//! 1. génère une identité au hasard (aléa du système) et un certificat
//!    **éphémère en mémoire** ;
//! 2. démarre un serveur TLS 1.3 qui exige une signature de requête
//!    (RFC 9421 + RFC 9530, `docs/auth.md`) et un droit `db.read` ;
//! 3. publie l'empreinte du certificat et l'**épingle** côté client
//!    (`PinnedServer`) avant la moindre requête ;
//! 4. envoie une requête signée (→ `200`) puis la même sans signature
//!    (→ `401` + `WWW-Authenticate`) sur la même connexion logique ;
//! 5. affiche le motif précis du refus **côté application** : il ne part
//!    jamais au réseau.
//!
//! Aucun secret n'est affiché : seule l'empreinte publique du certificat et
//! la clé publique du client sortent sur la console.

#[cfg(all(feature = "tls", feature = "auth-sign"))]
fn main() -> std::io::Result<()> {
    use litemcp::auth::PinnedServer;
    use litemcp::auth::sign::{
        ClientKey, KeyStore, NonceCache, RequestToSign, SignatureVerifier, Signer, SigningKey,
        TestKeyStore,
    };
    use litemcp::prelude::*;
    use litemcp::protocol::{META_PROTOCOL_VERSION, PROTOCOL_VERSION};
    use litemcp::tls::TlsConfig;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::Arc;
    use std::time::Duration;

    /// Corps d'un `tools/call` conforme au profil strict.
    fn corps(id: u64) -> Vec<u8> {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": "db.query",
                "arguments": { "sql": "select 1" },
                "_meta": {
                    META_PROTOCOL_VERSION: PROTOCOL_VERSION,
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        })
        .to_string()
        .into_bytes()
    }

    // 1. Identité du client (clé privée en mémoire) + certificat éphémère.
    let client = Signer::new(SigningKey::generate()).with_keyid("client-demo");
    let magasin: Arc<TestKeyStore> = Arc::new(TestKeyStore::new());
    magasin.insert(
        "client-demo",
        ClientKey::new("client-demo", client.verifying_key().to_bytes()).with_right("db.read"),
    );
    let tls = TlsConfig::self_signed_ephemeral("localhost").expect("certificat éphémère");
    let empreinte = tls.fingerprint();
    let certificat = tls.certificate_der().to_vec();
    let keys: Arc<dyn KeyStore> = magasin;

    // 2. Serveur : signature + TLS 1.3, un outil qui exige `db.read`.
    let bound = ServerBuilder::new("auth-tls", "1.0.0")
        .tool("db.query", "Lit la base", |_args| {
            Ok(serde_json::json!({ "rows": [{ "id": 1 }] }))
        })
        .require_right("db.query", "db.read")
        .verifier(Arc::new(SignatureVerifier::new(
            keys,
            NonceCache::with_default_policy(),
        )))
        .on_auth_decision(|event| {
            eprintln!(
                "  [journal] {} {:?} → {:?} ({})",
                event.method,
                event.name,
                event.verdict,
                event.reason.unwrap_or("accordé")
            );
        })
        .tls(tls)
        .bind("127.0.0.1:0")?;
    let adresse = bound.local_addr()?;
    println!("litemcp {VERSION} — MCP signé + TLS 1.3 sur https://{adresse}/mcp");
    println!("  empreinte du certificat : {empreinte}");
    let poignee = bound.handle();
    std::thread::spawn(move || {
        let _ = bound.serve();
    });

    // 3. Épinglage : l'empreinte observée doit être celle du serveur, sinon
    //    la connexion est refusée (aucune option de contournement).
    let epingle = PinnedServer::new(&empreinte.to_hex()).expect("empreinte canonique");
    epingle
        .verify(&empreinte.to_hex())
        .expect("identité du serveur épinglée");

    // Client TLS qui ne fait confiance qu'à ce certificat.
    let mut racines = rustls::RootCertStore::empty();
    racines
        .add(rustls::pki_types::CertificateDer::from(certificat))
        .expect("racine de test");
    let config = Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3")
        .with_root_certificates(racines)
        .with_no_client_auth(),
    );

    /// Une requête HTTP/1.1 sur une session TLS neuve, réponse complète.
    fn echanger(
        adresse: std::net::SocketAddr,
        config: Arc<rustls::ClientConfig>,
        entetes: &[(&str, &str)],
        corps: &[u8],
    ) -> std::io::Result<Vec<u8>> {
        let nom = rustls::pki_types::ServerName::try_from("localhost")
            .map_err(|erreur| std::io::Error::other(erreur.to_string()))?;
        let session = rustls::ClientConnection::new(config, nom)
            .map_err(|erreur| std::io::Error::other(erreur.to_string()))?;
        let socket = TcpStream::connect(adresse)?;
        socket.set_read_timeout(Some(Duration::from_secs(10)))?;
        socket.set_write_timeout(Some(Duration::from_secs(10)))?;
        let mut flux = rustls::StreamOwned::new(session, socket);
        let mut requete = format!("POST /mcp HTTP/1.1\r\nHost: {adresse}\r\n");
        for (nom, valeur) in entetes {
            requete.push_str(&format!("{nom}: {valeur}\r\n"));
        }
        requete.push_str(&format!(
            "MCP-Protocol-Version: {PROTOCOL_VERSION}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            corps.len()
        ));
        let mut octets = requete.into_bytes();
        octets.extend_from_slice(corps);
        flux.write_all(&octets)?;
        flux.flush()?;
        let mut reponse = Vec::new();
        flux.read_to_end(&mut reponse)?;
        Ok(reponse)
    }

    fn statut(reponse: &[u8]) -> String {
        String::from_utf8_lossy(reponse)
            .lines()
            .next()
            .unwrap_or_default()
            .to_string()
    }

    // 4. Requête **signée** : 200.
    let corps_signe = corps(1);
    let base = [
        ("Content-Type", "application/json"),
        ("Mcp-Method", "tools/call"),
        ("Mcp-Name", "db.query"),
    ];
    let autorite = adresse.to_string();
    let signes = client.headers(&RequestToSign::new(
        "POST",
        "/mcp",
        &autorite,
        &base,
        &corps_signe,
    ));
    // Les trois en-têtes du profil, dans l'ordre du document.
    let possedes: Vec<(String, String)> = signes
        .iter()
        .map(|(nom, valeur)| (nom.to_string(), valeur.to_string()))
        .collect();
    let mut entetes_signes: Vec<(&str, &str)> = base.to_vec();
    entetes_signes.extend(poignees(&possedes));
    let reponse = echanger(adresse, Arc::clone(&config), &entetes_signes, &corps_signe)?;
    println!("  1. requête signée      : {}", statut(&reponse));

    // 5. Même appel **sans signature** : 401 + WWW-Authenticate, corps constant.
    let sans_signature = echanger(
        adresse,
        Arc::clone(&config),
        &[
            ("Content-Type", "application/json"),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "db.query"),
        ],
        &corps_signe,
    )?;
    let texte = String::from_utf8_lossy(&sans_signature);
    println!("  2. sans signature      : {}", statut(&sans_signature));
    for ligne in texte.lines().take_while(|ligne| !ligne.is_empty()) {
        if ligne.to_ascii_lowercase().starts_with("www-authenticate:") {
            println!("     {ligne}");
        }
    }
    if let Some(index) = sans_signature
        .windows(4)
        .position(|fenetre| fenetre == b"\r\n\r\n")
    {
        println!(
            "     corps                : {}",
            String::from_utf8_lossy(&sans_signature[index + 4..])
        );
    }

    // L'épinglage refuse un serveur qui aurait changé de certificat.
    let autre = TlsConfig::self_signed_ephemeral("localhost").expect("autre certificat");
    match epingle.verify(&autre.fingerprint().to_hex()) {
        Ok(()) => println!("  3. épinglage           : inattendu (empreinte acceptée)"),
        Err(refus) => println!("  3. épinglage           : refusé — {refus}"),
    }

    poignee.shutdown();
    Ok(())
}

/// Projette des paires possédées en références — le vecteur vit plus longtemps.
#[cfg(all(feature = "tls", feature = "auth-sign"))]
fn poignees(paires: &[(String, String)]) -> Vec<(&str, &str)> {
    paires
        .iter()
        .map(|(nom, valeur)| (nom.as_str(), valeur.as_str()))
        .collect()
}

#[cfg(not(all(feature = "tls", feature = "auth-sign")))]
fn main() {
    eprintln!(
        "cet exemple exige deux features : cargo run --features tls,auth-sign --example auth_tls"
    );
}
