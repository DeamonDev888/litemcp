//! C4 — négociation de version du handshake legacy (`.legacy(true)`).
//!
//! Règle de la révision `2025-11-25` (Lifecycle, « Version Negotiation ») :
//! « If the server supports the requested protocol version, it **MUST**
//! respond with the same version. Otherwise, the server **MUST** respond with
//! another protocol version it supports. This **SHOULD** be the latest
//! version supported by the server. »
//! <https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle>
//!
//! Le profil legacy ne sert qu'une révision legacy (`2025-11-25`) : toute
//! demande — servie, inconnue ou absente — doit donc recevoir une version
//! que le serveur sert réellement, jamais `2026-07-28` (révision sans
//! `initialize`).
//!
//! Ces tests échouent **avant** correctif : `initialize` répondait
//! `2026-07-28` quelle que soit la version demandée.

use litemcp::{ServerBuilder, ServerCore};
use serde_json::{Value, json};

fn legacy_core() -> ServerCore {
    ServerBuilder::new("legacy", "2.0.0")
        .legacy(true)
        .tool("add", "Additionne", |_| Ok(json!({ "somme": 2 })))
        .core()
}

fn strict_core() -> ServerCore {
    ServerBuilder::new("strict", "2.0.0")
        .tool("add", "Additionne", |_| Ok(json!({ "somme": 2 })))
        .core()
}

fn dispatch(core: &ServerCore, id: i64, method: &str, params: Value) -> Value {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    let reply = core
        .dispatch(serde_json::to_string(&body).unwrap().as_bytes())
        .expect("une requête identifiée reçoit une réponse");
    serde_json::from_slice(&reply).expect("réponse JSON-RPC lisible")
}

fn initialize(core: &ServerCore, protocol_version: Option<&str>) -> Value {
    let mut params = json!({
        "capabilities": {},
        "clientInfo": { "name": "client-2025", "version": "1.0.0" },
    });
    if let Some(version) = protocol_version {
        params["protocolVersion"] = json!(version);
    }
    dispatch(core, 1, "initialize", params)
}

/// La version demandée est servie ⇒ elle est renvoyée telle quelle, avec les
/// trois champs obligatoires de `InitializeResult` (`protocolVersion`,
/// `capabilities`, `serverInfo`).
#[test]
fn initialize_echoes_the_served_legacy_version() {
    let v = initialize(&legacy_core(), Some("2025-11-25"));
    assert!(v.get("error").is_none(), "initialize doit réussir : {v}");
    assert_eq!(v["result"]["protocolVersion"], "2025-11-25", "{v}");
    assert_eq!(v["result"]["serverInfo"]["name"], "legacy", "{v}");
    assert_eq!(v["result"]["serverInfo"]["version"], "2.0.0", "{v}");
    assert!(
        v["result"]["capabilities"]["tools"].is_object(),
        "capacité tools attendue : {v}"
    );
    assert!(
        v["result"]["capabilities"]["resources"].is_object(),
        "capacité resources attendue : {v}"
    );
    assert!(
        v["result"]["capabilities"]["prompts"].is_object(),
        "capacité prompts attendue : {v}"
    );
}

/// Version demandée non servie ⇒ la réponse porte **une version servie**,
/// pas la version demandée ni une révision moderne.
#[test]
fn initialize_answers_a_served_version_for_an_unknown_request() {
    let v = initialize(&legacy_core(), Some("2024-11-05"));
    assert!(v.get("error").is_none(), "initialize doit réussir : {v}");
    assert_eq!(v["result"]["protocolVersion"], "2025-11-25", "{v}");
}

/// Demande sans `protocolVersion` (client non conforme) ⇒ même garantie :
/// la réponse annonce une révision réellement servie.
#[test]
fn initialize_without_a_requested_version_answers_a_served_version() {
    let v = initialize(&legacy_core(), None);
    assert!(v.get("error").is_none(), "initialize doit réussir : {v}");
    assert_eq!(v["result"]["protocolVersion"], "2025-11-25", "{v}");
}

/// Non-régression du profil **strict** (défaut) : `initialize` reste refusé
/// en `-32601` et `server/discover` garde exactement sa forme (une seule
/// version moderne annoncée). Ce test passe avant **et** après le correctif.
#[test]
fn strict_profile_is_unchanged_by_the_legacy_negotiation() {
    let core = strict_core();
    let v = dispatch(
        &core,
        1,
        "initialize",
        json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
            },
        }),
    );
    assert_eq!(v["error"]["code"], -32601, "{v}");

    let discover = dispatch(
        &core,
        2,
        "server/discover",
        json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
            },
        }),
    );
    assert_eq!(
        discover["result"]["supportedVersions"],
        json!(["2026-07-28"]),
        "{discover}"
    );
    assert_eq!(discover["result"]["resultType"], "complete", "{discover}");
}
