//! A6 (audit de conformance MCP `2026-07-28`, MINEUR 3) — le
//! `structuredContent` d'un résultat `isError` est **omis** quand un
//! `outputSchema` est publié pour l'outil.
//!
//! Page *Tools* :
//! <https://modelcontextprotocol.io/specification/2026-07-28/server/tools>
//! « If an output schema is provided: Servers MUST provide structured
//! results that conform to this schema » — la forme fil historique
//! `structuredContent: {"error": …}` ne conforme pas au schéma publié.
//!
//! Ces tests échouent **avant** correctif : le champ était toujours posé.

use litemcp::ServerBuilder;
use litemcp::error::ToolError;
use litemcp::zod;
use serde_json::{Value, json};

fn params(name: &str) -> Value {
    json!({
        "_meta": {
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {},
        },
        "name": name,
        "arguments": {},
    })
}

fn call_tool(core: &litemcp::ServerCore, id: i64, name: &str) -> (Value, u16) {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": params(name),
    });
    let reply = core
        .dispatch_request(serde_json::to_string(&body).unwrap().as_bytes(), None)
        .expect("une requête identifiée reçoit une réponse");
    (
        serde_json::from_slice(&reply.body).expect("réponse JSON-RPC lisible"),
        reply.status.code(),
    )
}

/// (1) Outil avec `outputSchema` publié + handler en `Err` → `isError: true`,
/// **pas** de `structuredContent`, `content` présent (message texte).
#[test]
fn tool_error_omits_structured_content_when_output_schema_is_published() {
    let core = ServerBuilder::new("t", "0.1.0")
        .tool("echec", "échec rate-limité", |_args| {
            Err(ToolError::rate_limited("quota épuisé").retry_after_ms(500))
        })
        .output_schema("echec", zod::object().field("total", zod::integer()))
        .core();
    let (v, status) = call_tool(&core, 1, "echec");
    assert_eq!(status, 200, "un échec d'exécution reste un résultat : {v}");
    assert_eq!(v["result"]["isError"], true, "{v}");
    assert!(
        v["result"].get("structuredContent").is_none(),
        "outputSchema publié → structuredContent omis sur isError : {v}"
    );
    assert_eq!(
        v["result"]["content"][0]["text"], "quota épuisé",
        "le message reste servi dans content : {v}"
    );
}

/// (2) Même outil **sans** `outputSchema` publié → la forme historique
/// `structuredContent.error` (`code`, `retryable`, `retryAfterMs`) est
/// conservée à l'identique : aucun contrat de sortie à violer.
#[test]
fn tool_error_keeps_structured_content_without_output_schema() {
    let core = ServerBuilder::new("t", "0.1.0")
        .tool("echec", "échec rate-limité", |_args| {
            Err(ToolError::rate_limited("quota épuisé").retry_after_ms(500))
        })
        .core();
    let (v, _) = call_tool(&core, 1, "echec");
    assert_eq!(v["result"]["isError"], true, "{v}");
    assert_eq!(
        v["result"]["structuredContent"]["error"]["code"], "rate_limited",
        "{v}"
    );
    assert_eq!(
        v["result"]["structuredContent"]["error"]["retryAfterMs"], 500,
        "{v}"
    );
    assert_eq!(v["result"]["content"][0]["text"], "quota épuisé", "{v}");
}

/// (3) Le succès reste validé contre l'`outputSchema` publié : conforme →
/// servi, non conforme → `isError` — et désormais sans `structuredContent`
/// (même règle que le chemin `Err`, l'échec ne porte pas de sortie
/// structurée non conforme).
#[test]
fn success_is_still_validated_against_the_output_schema() {
    let core = ServerBuilder::new("t", "0.1.0")
        .tool("bon", "sortie conforme", |_args| Ok(json!({ "total": 2 })))
        .tool("mauvais", "sortie non conforme", |_args| {
            Ok(json!({ "autre": 1 }))
        })
        .output_schema("bon", zod::object().field("total", zod::integer()))
        .output_schema("mauvais", zod::object().field("total", zod::integer()))
        .core();
    let (ok, _) = call_tool(&core, 1, "bon");
    assert!(ok["result"].get("isError").is_none(), "{ok}");
    assert_eq!(
        ok["result"]["structuredContent"],
        json!({ "total": 2 }),
        "{ok}"
    );
    let (bad, _) = call_tool(&core, 2, "mauvais");
    assert_eq!(bad["result"]["isError"], true, "{bad}");
    assert!(
        bad["result"].get("structuredContent").is_none(),
        "l'échec de validation ne doit pas porter structuredContent : {bad}"
    );
    assert!(
        bad["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("outputSchema"),
        "l'échec cite le contrat violé : {bad}"
    );
}
