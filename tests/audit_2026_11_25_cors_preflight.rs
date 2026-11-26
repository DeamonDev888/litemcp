//! B3 — audit 2026-11-25 : le préflight CORS doit annoncer **tous** les
//! en-têtes de requête que le serveur accepte réellement.
//!
//! Avant correction, `respond_preflight` renvoyait une liste figée
//! (`Content-Type, Accept, MCP-Protocol-Version, Mcp-Method, Mcp-Name`) :
//! un client navigateur cross-origin utilisant la signature RFC 9421
//! (`Content-Digest`, `Signature-Input`, `Signature`), un jeton
//! `Authorization` (hook `.on_request`) ou un paramètre promu en en-tête
//! (`Mcp-Param-*`, SEP-2243) était bloqué par le navigateur dès le
//! préflight. La liste `Access-Control-Allow-Headers` est désormais
//! calculée **une fois au bind** : liste de base + en-têtes de signature
//! si un vérificateur est configuré + `Mcp-Param-<suffixe>` pour chaque
//! outil du catalogue qui en déclare + en-têtes déclarés via
//! `.allow_preflight_header`.
//!
//! Client TCP brut, même mécanique que `tests/http.rs`.

use litemcp::auth::{AuthContext, ProofMethod, TestVerifier};
use litemcp::{ServerBuilder, zod};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

/// Liste de base, inchangée pour un serveur sans vérificateur ni paramètre
/// promu — non-régression de `tests/http_security_audit.rs` §14.
const BASE: &str = "Content-Type, Accept, MCP-Protocol-Version, Mcp-Method, Mcp-Name";
/// Les trois en-têtes produits par le profil de signature RFC 9421
/// (`SignedHeaders::iter`, `docs/auth.md` §2.1).
const SIGNATURE: &str = "Content-Digest, Signature-Input, Signature";
const ORIGINE: &str = "https://app.exemple.com";

fn servir(builder: ServerBuilder) -> SocketAddr {
    let bound = builder
        .allow_origins(&[ORIGINE])
        .bind("127.0.0.1:0")
        .expect("bind");
    let addr = bound.local_addr().expect("local_addr");
    std::thread::spawn(move || {
        let _ = bound.serve();
    });
    addr
}

fn spawn_base() -> SocketAddr {
    servir(
        ServerBuilder::new("cors", "1.0.0")
            .legacy(true)
            .tool("ok", "Répond ok", |_| {
                Ok(serde_json::json!({ "ok": true }))
            }),
    )
}

fn spawn_verificateur() -> SocketAddr {
    servir(
        ServerBuilder::new("cors-auth", "1.0.0")
            .legacy(true)
            .tool("ok", "Répond ok", |_| {
                Ok(serde_json::json!({ "ok": true }))
            })
            .verifier(Arc::new(TestVerifier::accepting(AuthContext::new(
                "client-web",
                ProofMethod::Application,
            )))),
    )
}

fn spawn_parametre() -> SocketAddr {
    servir(
        ServerBuilder::new("cors-param", "1.0.0")
            .legacy(true)
            .tool_typed(
                "compte",
                "Compte avec paramètre de transport",
                zod::object().field("count", zod::integer().header("count")),
                |_| Ok(serde_json::json!({ "ok": true })),
            ),
    )
}

/// Préflight `OPTIONS /mcp` avec l'origine autorisée — rend (statut, tête).
fn preflight(addr: SocketAddr) -> (u16, String) {
    let req = format!(
        "OPTIONS /mcp HTTP/1.1\r\n\
         Host: t\r\n\
         Origin: {ORIGINE}\r\n\
         Access-Control-Request-Method: POST\r\n\
         Access-Control-Request-Headers: content-type\r\n\
         Connection: close\r\n\r\n"
    );
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout");
    stream.write_all(req.as_bytes()).expect("write");
    let mut raw = String::new();
    stream.read_to_string(&mut raw).expect("lecture réponse");
    let (head, _) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (status, head.to_string())
}

/// Première valeur de l'en-tête `nom` dans la tête de réponse (insensible
/// à la casse), chaîne vide si absent.
fn valeur(head: &str, nom: &str) -> String {
    head.lines()
        .find_map(|ligne| {
            let (cle, valeur) = ligne.split_once(':')?;
            cle.trim()
                .eq_ignore_ascii_case(nom)
                .then(|| valeur.trim().to_string())
        })
        .unwrap_or_default()
}

#[test]
fn la_liste_de_base_est_inchangee_sans_verificateur_ni_parametre() {
    let addr = spawn_base();
    let (status, head) = preflight(addr);
    assert_eq!(status, 204, "préflight autorisé : {head}");
    assert_eq!(
        valeur(&head, "Access-Control-Allow-Headers"),
        BASE,
        "sans vérificateur ni paramètre promu, la liste historique doit rester exactement la même"
    );
}

#[test]
fn un_verificateur_ajoute_les_en_tetes_de_signature_rfc_9421() {
    let addr = spawn_verificateur();
    let (status, head) = preflight(addr);
    assert_eq!(status, 204, "préflight autorisé : {head}");
    assert_eq!(
        valeur(&head, "Access-Control-Allow-Headers"),
        format!("{BASE}, {SIGNATURE}"),
        "Content-Digest, Signature-Input et Signature doivent être annoncés dès qu'un vérificateur est configuré"
    );
}

#[test]
fn un_parametre_promeut_mcp_param_suffixe_dans_la_liste() {
    let addr = spawn_parametre();
    let (status, head) = preflight(addr);
    assert_eq!(status, 204, "préflight autorisé : {head}");
    assert_eq!(
        valeur(&head, "Access-Control-Allow-Headers"),
        format!("{BASE}, Mcp-Param-count"),
        "Mcp-Param-count doit être annoncé pour un outil déclarant x-mcp-header: count"
    );
}
fn spawn_utilisateur() -> SocketAddr {
    servir(
        ServerBuilder::new("cors-user", "1.0.0")
            .legacy(true)
            .tool("ok", "Répond ok", |_| {
                Ok(serde_json::json!({ "ok": true }))
            })
            .allow_preflight_header("Authorization"),
    )
}

fn spawn_combine() -> SocketAddr {
    // Vérificateur + paramètre promu + déclarations utilisateur en double
    // et en collision avec la liste de base : la liste finale doit rester
    // dédupliquée, dans un ordre stable.
    servir(
        ServerBuilder::new("cors-all", "1.0.0")
            .legacy(true)
            .tool_typed(
                "compte",
                "Compte avec paramètre de transport",
                zod::object().field("count", zod::integer().header("count")),
                |_| Ok(serde_json::json!({ "ok": true })),
            )
            .verifier(Arc::new(TestVerifier::accepting(AuthContext::new(
                "client-web",
                ProofMethod::Application,
            ))))
            .allow_preflight_header("Authorization")
            .allow_preflight_header("authorization")
            .allow_preflight_header("Content-Type"),
    )
}

#[test]
fn allow_preflight_header_annonce_les_en_tetes_de_l_utilisateur() {
    let addr = spawn_utilisateur();
    let (status, head) = preflight(addr);
    assert_eq!(status, 204, "préflight autorisé : {head}");
    assert_eq!(
        valeur(&head, "Access-Control-Allow-Headers"),
        format!("{BASE}, Authorization"),
        "l'en-tête déclaré via allow_preflight_header doit rejoindre la liste du préflight"
    );
}

#[test]
fn la_liste_combinee_est_dedoublonnee_et_d_ordre_stable() {
    let addr = spawn_combine();
    let (status, head) = preflight(addr);
    assert_eq!(status, 204, "préflight autorisé : {head}");
    assert_eq!(
        valeur(&head, "Access-Control-Allow-Headers"),
        format!("{BASE}, {SIGNATURE}, Mcp-Param-count, Authorization"),
        "ordre : base, signature, paramètres promus, déclarations utilisateur — chaque nom exactement une fois"
    );
}
