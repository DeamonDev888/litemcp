//! A5 (audit de conformance MCP `2026-07-28`, MINEUR 2) — l'erreur
//! `initialize` du profil strict nomme les versions supportées.
//!
//! Page *Versioning* :
//! <https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning>
//! « A server that supports only modern versions SHOULD name the protocol
//! versions it supports in any error it returns to an `initialize` request ».
//!
//! Ce test échoue **avant** correctif : le `-32601` rendu en strict ne
//! portait aucun `data`.

use litemcp::{ServerBuilder, ServerCore};
use serde_json::{Value, json};

fn strict_core() -> ServerCore {
    ServerBuilder::new("init-err", "0.1.0").core()
}

/// Dispatche une requête identifiée en profil strict et rend
/// (corps JSON-RPC, statut HTTP numérique) — le statut fait partie du
/// constat (404 attendu pour une méthode inconnue).
fn dispatch_strict(core: &ServerCore, id: i64, method: &str) -> (Value, u16) {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": {
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
            },
        },
    });
    let reply = core
        .dispatch_request(serde_json::to_string(&body).unwrap().as_bytes(), None)
        .expect("une requête identifiée reçoit une réponse");
    (
        serde_json::from_slice(&reply.body).expect("réponse JSON-RPC lisible"),
        reply.status.code(),
    )
}

/// `initialize` en profil strict → HTTP 404 + `-32601` + `data.supported`
/// nommant les versions modernes servies (`["2026-07-28"]`, contenu de
/// `protocol::SUPPORTED_VERSIONS`). Le message et l'id ne changent pas.
#[test]
fn initialize_error_names_the_supported_versions() {
    let (v, status) = dispatch_strict(&strict_core(), 1, "initialize");
    assert_eq!(status, 404, "initialize en strict → HTTP 404 : {v}");
    assert_eq!(v["error"]["code"], -32601, "{v}");
    assert_eq!(v["id"], 1, "l'id de la requête est repris : {v}");
    assert_eq!(
        v["error"]["data"]["supported"],
        json!(["2026-07-28"]),
        "data.supported doit nommer les versions modernes servies : {v}"
    );
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("Unknown method: initialize"),
        "le message reste celui d'une méthode inconnue : {v}"
    );
}

/// Les **autres** méthodes inconnues restent sans `data` : `supported` n'est
/// posé que là où la spec le demande (uniquement `initialize`), même
/// convention que le `-32022` qui, lui, la porte pour toute version refusée.
#[test]
fn other_unknown_methods_stay_without_data() {
    let core = strict_core();
    for (id, method) in [
        (1, "ping"),
        (2, "logging/setLevel"),
        (3, "resources/subscribe"),
    ] {
        let (v, status) = dispatch_strict(&core, id, method);
        assert_eq!(status, 404, "{method} → HTTP 404 : {v}");
        assert_eq!(v["error"]["code"], -32601, "{method} : {v}");
        assert!(
            v["error"].get("data").is_none(),
            "{method} inconnue ne doit pas porter data : {v}"
        );
    }
}

/// En profil legacy, `initialize` réussit (négociation C4) : le chemin
/// d'erreur n'existe pas hors du profil strict — hypothèse du correctif,
/// vérifiée pour rester vraie.
#[test]
fn legacy_profile_serves_initialize_so_the_error_path_does_not_exist() {
    let core = ServerBuilder::new("init-legacy", "0.1.0")
        .legacy(true)
        .core();
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "protocolVersion": "2025-11-25" },
    });
    let reply = core
        .dispatch_request(serde_json::to_string(&body).unwrap().as_bytes(), None)
        .expect("une requête identifiée reçoit une réponse");
    assert_eq!(reply.status.code(), 200);
    let v: Value = serde_json::from_slice(&reply.body).unwrap();
    assert!(v.get("error").is_none(), "initialize legacy réussit : {v}");
    assert_eq!(v["result"]["protocolVersion"], "2025-11-25", "{v}");
}
