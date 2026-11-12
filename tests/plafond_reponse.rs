//! Plafond de réponse (`max_response_bytes`) — vérification **octet pour
//! octet** des sorties et du verdict du plafond pour les plafonds
//! **0 / 1 / exact / dépassé**, au dispatch **et** au fil HTTP (JSON et
//! événement SSE).
//!
//! Les octets attendus sont **figés** (campagne « writer borné »,
//! `docs/bench/octets_avant.txt` — capture doublement déterministe) : toute
//! divergence est une régression de comportement fil, pas un détail
//! d'implémentation. Notamment :
//!
//! - le message d'erreur du plafond rapporte la **taille réelle** de la
//!   réponse sérialisée (`response size (N bytes) …`) — le writer borné
//!   cesse de stocker au-delà du plafond mais compte le total exact ;
//! - la réponse exactement au plafond part **entière** (jamais tronquée) ;
//! - `Content-Length` reste la longueur du corps effectivement envoyé.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use litemcp::ServerBuilder;
use serde_json::{Value, json};

/// Plafond des scénarios « exact / dépassé ».
const CAP: usize = 300;

/// Calibration figée par les captures : 36 `x` → réponse de 300 o pile.
const N_EXACT: usize = 36;
/// 37 `x` → 302 o (un caractère ajoute un octet dans `content[0].text` **et**
/// dans `structuredContent`).
const N_PLUS_UN: usize = 37;

const SUCCES_20: &str = concat!(
    r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","content":[{"type":"text","text":"{"#,
    r#"\"data\":\"xxxxxxxxxxxxxxxxxxxx\"}"}],"structuredContent":{"data":"xxxxxxxxxxxxxxxxxxxx"},"#,
    r#""_meta":{"io.modelcontextprotocol/serverInfo":{"name":"capture","version":"1.0.0"}}}}"#,
);

const SUCCES_EXACT_300: &str = concat!(
    r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","content":[{"type":"text","text":"{"#,
    r#"\"data\":\""#,
    "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
    r#"\"}"}],"structuredContent":{"data":"#,
    "\"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"",
    r#"},"_meta":{"io.modelcontextprotocol/serverInfo":{"name":"capture","version":"1.0.0"}}}}"#,
);

const ERREUR_CAP_1: &str = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"response size (268 bytes) exceeds maximum (1 bytes)"}}"#;

const ERREUR_CAP_300: &str = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"response size (302 bytes) exceeds maximum (300 bytes)"}}"#;

fn constructeur(plafond: usize) -> ServerBuilder {
    ServerBuilder::new("capture", "1.0.0")
        .legacy(true)
        .tool("blob", "Rend une chaîne de n caractères", |args| {
            let n = args.get("n").and_then(Value::as_u64).unwrap_or(0) as usize;
            Ok(json!({ "data": "x".repeat(n) }))
        })
        .max_response_bytes(plafond)
}

fn corps(n: usize) -> Vec<u8> {
    json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "blob", "arguments": { "n": n } }
    })
    .to_string()
    .into_bytes()
}

fn assert_octets(reponse: &[u8], attendu: &str, contexte: &str) {
    assert_eq!(
        std::str::from_utf8(reponse).expect("réponse UTF-8"),
        attendu,
        "{contexte} : octets différents de la référence figée"
    );
}

// --- dispatch : plafonds 0 / 1 / exact / dépassé -----------------------------

#[test]
fn plafond_0_rend_le_corps_complet_octet_pour_octet() {
    let reponse = constructeur(0)
        .core()
        .dispatch(&corps(20))
        .expect("dispatch");
    assert_octets(&reponse, SUCCES_20, "plafond 0");
}

#[test]
fn plafond_1_produit_l_erreur_octet_pour_octet() {
    let reponse = constructeur(1)
        .core()
        .dispatch(&corps(20))
        .expect("dispatch");
    assert_octets(&reponse, ERREUR_CAP_1, "plafond 1");
}

#[test]
fn plafond_exact_passe_entier_sans_troncature() {
    let reponse = constructeur(CAP)
        .core()
        .dispatch(&corps(N_EXACT))
        .expect("dispatch");
    assert_eq!(reponse.len(), CAP, "réponse exactement au plafond attendue");
    assert_octets(&reponse, SUCCES_EXACT_300, "plafond exact");
}

#[test]
fn plafond_depasse_rapporte_la_taille_reelle_octet_pour_octet() {
    let reponse = constructeur(CAP)
        .core()
        .dispatch(&corps(N_PLUS_UN))
        .expect("dispatch");
    assert_octets(&reponse, ERREUR_CAP_300, "plafond dépassé");
}

// --- fil HTTP : mêmes octets, tête et corps ----------------------------------

fn lie(plafond: usize) -> (SocketAddr, std::thread::JoinHandle<()>) {
    let bound = constructeur(plafond).bind("127.0.0.1:0").expect("bind");
    let addr = bound.local_addr().expect("adresse");
    let handle = std::thread::spawn(move || bound.serve().expect("serve"));
    (addr, handle)
}

fn requete_json(corps_req: &[u8]) -> Vec<u8> {
    format!(
        "POST /mcp HTTP/1.1\r\nHost: capture\r\nAccept: application/json\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        corps_req.len(),
        std::str::from_utf8(corps_req).expect("corps UTF-8"),
    )
    .into_bytes()
}

fn requete_sse(corps_req: &[u8]) -> Vec<u8> {
    format!(
        "POST /mcp HTTP/1.1\r\nHost: capture\r\nAccept: text/event-stream\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        corps_req.len(),
        std::str::from_utf8(corps_req).expect("corps UTF-8"),
    )
    .into_bytes()
}

/// Aller-retour JSON keep-alive cadré sur `Content-Length` — renvoie les
/// octets fil **exactement** reçus (tête + corps, pas un de plus).
fn echange_json(addr: SocketAddr, requete: &[u8]) -> Vec<u8> {
    let mut flux = TcpStream::connect(addr).expect("connexion");
    flux.set_nodelay(true).expect("nodelay");
    flux.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    flux.write_all(requete).expect("écriture");

    let mut tampon = Vec::new();
    let mut provisoire = [0u8; 8 * 1024];
    let fin_tete = loop {
        let lu = flux.read(&mut provisoire).expect("lecture tête");
        assert!(lu > 0, "connexion fermée (tête)");
        tampon.extend_from_slice(&provisoire[..lu]);
        if let Some(pos) = tampon.windows(4).position(|f| f == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let texte = std::str::from_utf8(&tampon[..fin_tete]).expect("tête UTF-8");
    let pos = texte
        .find("Content-Length: ")
        .expect("Content-Length absent");
    let reste = &texte[pos + "Content-Length: ".len()..];
    let attendu = fin_tete
        + reste
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap_or("")
            .parse::<usize>()
            .expect("Content-Length illisible");
    while tampon.len() < attendu {
        let lu = flux.read(&mut provisoire).expect("lecture corps");
        assert!(lu > 0, "connexion fermée (corps)");
        let prendre = lu.min(attendu - tampon.len());
        tampon.extend_from_slice(&provisoire[..prendre]);
    }
    assert_eq!(tampon.len(), attendu, "corps cadré au Content-Length");
    tampon
}

/// Aller-retour SSE : une connexion par événement, lecture jusqu'à la
/// fermeture.
fn echange_sse(addr: SocketAddr, requete: &[u8]) -> Vec<u8> {
    let mut flux = TcpStream::connect(addr).expect("connexion sse");
    flux.set_nodelay(true).expect("nodelay");
    flux.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    flux.write_all(requete).expect("écriture");
    let mut tampon = Vec::new();
    flux.read_to_end(&mut tampon).expect("lecture sse");
    assert!(!tampon.is_empty(), "aucun octet reçu en SSE");
    tampon
}

#[test]
fn fil_json_plafond_0_octets_identiques() {
    let (addr, _serveur) = lie(0);
    let fil = echange_json(addr, &requete_json(&corps(20)));
    let attendu = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 268\r\n\
         Connection: keep-alive\r\n\r\n{SUCCES_20}"
    );
    assert_octets(&fil, &attendu, "fil JSON plafond 0");
}

#[test]
fn fil_json_plafond_exact_octets_identiques() {
    let (addr, _serveur) = lie(CAP);
    let fil = echange_json(addr, &requete_json(&corps(N_EXACT)));
    let attendu = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {CAP}\r\n\
         Connection: keep-alive\r\n\r\n{SUCCES_EXACT_300}"
    );
    assert_octets(&fil, &attendu, "fil JSON plafond exact");
}

#[test]
fn fil_json_plafond_depasse_octets_identiques() {
    let (addr, _serveur) = lie(CAP);
    let fil = echange_json(addr, &requete_json(&corps(N_PLUS_UN)));
    let attendu = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: keep-alive\r\n\r\n{ERREUR_CAP_300}",
        ERREUR_CAP_300.len()
    );
    assert_octets(&fil, &attendu, "fil JSON plafond dépassé");
}

#[test]
fn fil_json_plafond_1_octets_identiques() {
    let (addr, _serveur) = lie(1);
    let fil = echange_json(addr, &requete_json(&corps(20)));
    let attendu = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: keep-alive\r\n\r\n{ERREUR_CAP_1}",
        ERREUR_CAP_1.len()
    );
    assert_octets(&fil, &attendu, "fil JSON plafond 1");
}

#[test]
fn fil_sse_plafond_depasse_octets_identiques() {
    let (addr, _serveur) = lie(CAP);
    let fil = echange_sse(addr, &requete_sse(&corps(N_PLUS_UN)));
    let attendu = format!(
        "HTTP/1.1 200 OK\r\nCache-Control: no-cache, no-transform\r\n\
         X-Accel-Buffering: no\r\nContent-Type: text/event-stream\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\ndata: {ERREUR_CAP_300}\n\n",
        ERREUR_CAP_300.len() + "data: ".len() + 2
    );
    assert_octets(&fil, &attendu, "fil SSE plafond dépassé");
}

#[test]
fn fil_sse_plafond_0_octets_identiques() {
    let (addr, _serveur) = lie(0);
    let fil = echange_sse(addr, &requete_sse(&corps(20)));
    let attendu = format!(
        "HTTP/1.1 200 OK\r\nCache-Control: no-cache, no-transform\r\n\
         X-Accel-Buffering: no\r\nContent-Type: text/event-stream\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\ndata: {SUCCES_20}\n\n",
        SUCCES_20.len() + "data: ".len() + 2
    );
    assert_octets(&fil, &attendu, "fil SSE plafond 0");
}
