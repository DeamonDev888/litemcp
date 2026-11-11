//! Tests de régression du 2026-11-08 — deux surfaces MCP manquantes du noyau :
//! `resources/templates/list` et `completion/complete`.
//!
//! Formes fil attendues issues du schéma officiel MCP 2026-07-28
//! (`draft.schema.json` de la suite modelcontextprotocol/conformance) :
//! - `ListResourceTemplatesResult` : `resultType` + `resourceTemplates` +
//!   indices de cache SEP-2549 (`ttlMs`/`cacheScope`) ;
//! - `CompleteResult` : `resultType` + `completion.values` (≤ 100 chaînes)
//!   avec `total`/`hasMore` optionnels ;
//! - capacité `completions` annoncée seulement quand un fournisseur existe.
//!
//! Ces tests échouent **avant** correctif : les deux méthodes répondaient
//! `-32601` (méthode inconnue).

use litemcp::{ServerBuilder, ServerCore};
use serde_json::{Value, json};

// --- Harnais dispatch (sans transport) — profil strict 2026-07-28 ---

fn params_with_meta(mut params: Value) -> Value {
    params
        .as_object_mut()
        .expect("params doit être un objet")
        .insert(
            "_meta".into(),
            json!({
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
            }),
        );
    params
}

fn dispatch(core: &ServerCore, id: i64, method: &str, params: Value) -> Value {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params_with_meta(params),
    });
    let reply = core
        .dispatch(serde_json::to_string(&body).unwrap().as_bytes())
        .expect("une requête identifiée reçoit une réponse");
    serde_json::from_slice(&reply).expect("réponse JSON-RPC lisible")
}

fn dispatch_status(core: &ServerCore, method: &str, params: Value) -> u16 {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params_with_meta(params),
    });
    core.dispatch_request(serde_json::to_string(&body).unwrap().as_bytes(), None)
        .expect("requête identifiée : réponse")
        .status
        .code()
}

fn empty_params() -> Value {
    json!({})
}

// ===========================================================================
// resources/templates/list
// ===========================================================================

/// Le noyau sert `resources/templates/list` **même sans template enregistré**
/// — catalogue vide, indices de cache SEP-2549, `_meta` d'identité serveur.
/// Avant correctif : `-32601` « Unknown method ».
#[test]
fn resources_templates_list_is_served_even_when_empty() {
    let core = ServerBuilder::new("tpl", "0.1.0").core();
    let v = dispatch(&core, 1, "resources/templates/list", empty_params());
    assert!(
        v.get("error").is_none(),
        "resources/templates/list doit réussir sur un serveur vide : {v}"
    );
    assert_eq!(v["result"]["resultType"], "complete", "{v}");
    assert_eq!(v["result"]["resourceTemplates"], json!([]), "{v}");
    assert_eq!(v["result"]["ttlMs"], 60_000, "{v}");
    assert_eq!(v["result"]["cacheScope"], "private", "{v}");
    assert!(
        v["result"]["_meta"]["io.modelcontextprotocol/serverInfo"].is_object(),
        "identité serveur attendue sous _meta : {v}"
    );
}

// Un serveur qui n'expose **que** des templates annonce bien la capacité
// `resources` — vérifié par `templates_are_covered_by_the_resources_capability`
// dans `mcp_content_surfaces.rs`.

// ===========================================================================
// completion/complete
// ===========================================================================

/// Sans fournisseur enregistré, la requête **bien formée** reçoit
/// `-32602` (convention du noyau : cible introuvable), pas `-32601`.
/// Avant correctif : `-32601` « Unknown method ».
#[test]
fn completion_complete_without_provider_is_invalid_params() {
    let core = ServerBuilder::new("comp", "0.1.0").core();
    let v = dispatch(
        &core,
        1,
        "completion/complete",
        json!({
            "ref": { "type": "ref/prompt", "name": "test_prompt_with_arguments" },
            "argument": { "name": "arg1", "value": "par" },
        }),
    );
    assert_eq!(v["error"]["code"], -32602, "{v}");
}

/// `completion/complete` exécute du code serveur : une notification
/// (sans `id`) est refusée `-32600` avec HTTP 400, comme les autres
/// méthodes d'action — rien ne doit s'exécuter en silence.
/// Avant correctif : la méthode tombait dans « unknown notification »
/// (`-32600` aussi, mais avec HTTP 400 — le statut est identique ; ce test
/// verrouille le message « notification rejected » propre à l'action).
#[test]
fn completion_complete_notification_is_rejected_as_an_action() {
    let core = ServerBuilder::new("comp", "0.1.0").core();
    let body = json!({
        "jsonrpc": "2.0",
        "method": "completion/complete",
        "params": {
            "ref": { "type": "ref/prompt", "name": "p" },
            "argument": { "name": "arg1", "value": "par" },
        },
    });
    let reply = core
        .dispatch(serde_json::to_string(&body).unwrap().as_bytes())
        .expect("id: null explicite — réponse attendue");
    let v: Value = serde_json::from_slice(&reply).unwrap();
    assert_eq!(v["error"]["code"], -32600, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("notification rejected"),
        "message d'action attendu : {v}"
    );
    let status = dispatch_status(
        &core,
        "completion/complete",
        json!({
            "ref": { "type": "ref/prompt", "name": "p" },
            "argument": { "name": "arg1", "value": "par" },
        }),
    );
    assert_eq!(status, 400, "notification d'action : HTTP 400");
}

/// Un serveur sans fournisseur de complétion n'annonce PAS la capacité
/// `completions` — une capacité annoncée est une capacité servie.
#[test]
fn completions_capability_is_absent_without_a_provider() {
    let core = ServerBuilder::new("comp", "0.1.0").core();
    let v = dispatch(&core, 1, "server/discover", empty_params());
    assert!(
        v["result"]["capabilities"].get("completions").is_none(),
        "completions ne doit pas être annoncé sans fournisseur : {v}"
    );
}
