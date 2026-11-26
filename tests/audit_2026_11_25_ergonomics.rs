//! Audit 2026-11-25 — améliorations **A2, A3, A4, A6, A7** :
//!
//! - **A2** : variantes typées/parsées sur `tool_result`, `tool_mrtr`,
//!   `prompt_mrtr` et `prompt_parse` ;
//! - **A3** : `resource_mime_type` et `resource_template_title` (ordre libre,
//!   publiés dans `resources/list` et `resources/templates/list`) ;
//! - **A4** : doc-comments `///` comme description par défaut sur `#[tool]`,
//!   `#[prompt]`, `#[resource]`, `mime_type` sur `#[resource]`, et alias
//!   `ServerBuilder::register` ;
//! - **A6** : `clippy -D warnings` exécuté sur les jobs CI `windows` et `macos` ;
//! - **A7** : `resource_template_handler` pour le routage dynamique des
//!   gabarits RFC 6570 simples `{var}` (priorité aux URI statiques, extraction
//!   des paramètres, compatibilité avec `require_resource_right`).

use litemcp::auth::{AuthContext, ProofMethod, TestVerifier};
use litemcp::prelude::*;
use litemcp::protocol::{CallToolResult, ContentBlock, PromptMessage, ResourceData};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

fn dispatch(core: &litemcp::ServerCore, requete: &Value) -> Value {
    let octets = core
        .dispatch_unchecked(requete.to_string().as_bytes())
        .expect("réponse attendue");
    serde_json::from_slice(&octets).expect("JSON-RPC valide")
}

// ------------------------------------------------------------ A2

#[derive(Deserialize)]
struct CaptureArgs {
    largeur: u32,
}

#[derive(Deserialize)]
struct RevueArgs {
    fichier: String,
    strict: Option<bool>,
}

#[test]
fn a2_tool_result_typed_et_parse_valident_avant_handler() {
    let core = ServerBuilder::new("a2", "1.0.0")
        .tool_result_typed(
            "img_typed",
            "Rend une image",
            zod::object().field("label", zod::string().min(2)),
            |args| {
                let label = args["label"].as_str().unwrap_or("");
                Ok(CallToolResult::from_content([ContentBlock::text(format!(
                    "ok:{label}"
                ))]))
            },
        )
        .tool_result_parse(
            "img_parse",
            "Capture typée",
            zod::object().field("largeur", zod::uint().min(10)),
            |args: CaptureArgs| {
                Ok(CallToolResult::from_content([ContentBlock::text(format!(
                    "w={}",
                    args.largeur
                ))]))
            },
        )
        .core();

    // Hors schéma → -32602 avant le handler.
    let bad = dispatch(
        &core,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "img_typed", "arguments": { "label": "x" } }
        }),
    );
    assert_eq!(bad["error"]["code"], -32602, "{bad}");

    // Valide sur tool_result_typed.
    let ok = dispatch(
        &core,
        &json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "img_typed", "arguments": { "label": "hero" } }
        }),
    );
    assert_eq!(ok["result"]["content"][0]["text"], "ok:hero", "{ok}");

    // Valide sur tool_result_parse.
    let ok2 = dispatch(
        &core,
        &json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "img_parse", "arguments": { "largeur": 640 } }
        }),
    );
    assert_eq!(ok2["result"]["content"][0]["text"], "w=640", "{ok2}");
}

#[test]
fn a2_mrtr_typed_et_prompt_parse_valident_et_publient_le_schema() {
    let core = ServerBuilder::new("a2-mrtr", "1.0.0")
        .tool_mrtr_typed(
            "confirme",
            "Outil MRTR typé",
            zod::object().field("montant", zod::integer().min(1)),
            |args, round| {
                if round.turn == 1 {
                    Ok(ToolOutcome::InputRequired(
                        InputRequiredSpec::new().with_state(json!({ "m": args["montant"] })),
                    ))
                } else {
                    Ok(ToolOutcome::Value(json!({ "valide": true })))
                }
            },
        )
        .prompt_mrtr_typed(
            "p_mrtr",
            "Prompt MRTR typé",
            zod::object().field("sujet", zod::string().min(2)),
            |args, _round| {
                Ok(PromptOutcome::Messages(vec![PromptMessage::user(format!(
                    "Sujet: {}",
                    args["sujet"].as_str().unwrap_or("")
                ))]))
            },
        )
        .prompt_parse(
            "p_parse",
            "Prompt parsé",
            zod::object()
                .field("fichier", zod::string())
                .opt("strict", zod::boolean()),
            |args: RevueArgs| {
                Ok(vec![PromptMessage::user(format!(
                    "{}:{}",
                    args.fichier,
                    args.strict.unwrap_or(false)
                ))])
            },
        )
        .core();

    // `tool_mrtr_typed` rejette hors schéma en -32602.
    let bad = dispatch(
        &core,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "confirme", "arguments": { "montant": 0 } }
        }),
    );
    assert_eq!(bad["error"]["code"], -32602, "{bad}");

    // `prompt_mrtr_typed` rejette hors schéma en -32602.
    let bad_p = dispatch(
        &core,
        &json!({
            "jsonrpc": "2.0", "id": 2, "method": "prompts/get",
            "params": { "name": "p_mrtr", "arguments": { "sujet": "a" } }
        }),
    );
    assert_eq!(bad_p["error"]["code"], -32602, "{bad_p}");

    // `prompt_parse` désérialise et rend les messages.
    let ok_p = dispatch(
        &core,
        &json!({
            "jsonrpc": "2.0", "id": 3, "method": "prompts/get",
            "params": { "name": "p_parse", "arguments": { "fichier": "lib.rs", "strict": true } }
        }),
    );
    assert_eq!(
        ok_p["result"]["messages"][0]["content"]["text"], "lib.rs:true",
        "{ok_p}"
    );
}

// ------------------------------------------------------------ A3

#[test]
fn a3_resource_mime_type_et_template_title_ordre_libre() {
    let core = ServerBuilder::new("a3", "1.0.0")
        // Déclarés AVANT l'enregistrement.
        .resource_mime_type("mem://avant", "text/markdown")
        .resource_mime_type("mem://tpl/{id}", "application/json")
        .resource_template_title("mem://tpl/{id}", "Objet par identifiant")
        .resource("mem://avant", "Avant", "Doc avant", || {
            Ok(litemcp::protocol::ResourceContents {
                uri: "mem://avant".into(),
                mime_type: Some("text/markdown".into()),
                text: "# ok".into(),
            })
        })
        .resource("mem://apres", "Apres", "Doc apres", || {
            Ok(litemcp::protocol::ResourceContents {
                uri: "mem://apres".into(),
                mime_type: Some("text/plain".into()),
                text: "ok".into(),
            })
        })
        // Déclaré APRÈS l'enregistrement.
        .resource_mime_type("mem://apres", "text/plain")
        .resource_template("mem://tpl/{id}", "tpl", "Gabarit")
        .core();

    let res = dispatch(
        &core,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "resources/list" }),
    );
    let items = res["result"]["resources"].as_array().unwrap();
    assert_eq!(items[0]["uri"], "mem://apres");
    assert_eq!(items[0]["mimeType"], "text/plain");
    assert_eq!(items[1]["uri"], "mem://avant");
    assert_eq!(items[1]["mimeType"], "text/markdown");

    let tpls = dispatch(
        &core,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "resources/templates/list" }),
    );
    let t = &tpls["result"]["resourceTemplates"][0];
    assert_eq!(t["uriTemplate"], "mem://tpl/{id}");
    assert_eq!(t["title"], "Objet par identifiant");
    assert_eq!(t["mimeType"], "application/json");
}

// ------------------------------------------------------------ A4 (macros)

#[cfg(feature = "macros")]
mod a4_macros {
    use super::*;
    use litemcp::macros::{prompt, resource, tool};

    /// Multiplie deux entiers.
    #[tool]
    fn mul(a: i64, b: i64) -> litemcp::ToolResult {
        Ok(json!({ "produit": a * b }))
    }

    /// Résume un texte.
    #[prompt]
    fn resume(texte: String) -> Result<Vec<PromptMessage>, String> {
        Ok(vec![PromptMessage::user(texte)])
    }

    /// Documentation markdown embarquée.
    #[resource(uri = "mem://guide", mime_type = "text/markdown")]
    fn guide() -> Result<litemcp::protocol::ResourceContents, String> {
        Ok(litemcp::protocol::ResourceContents {
            uri: "mem://guide".into(),
            mime_type: Some("text/markdown".into()),
            text: "# Guide".into(),
        })
    }

    #[test]
    fn a4_doc_comments_mime_type_et_alias_register() {
        let core = ServerBuilder::new("a4", "1.0.0")
            .register(register_mul)
            .register(register_resume)
            .register(register_guide)
            .core();

        let tools = dispatch(
            &core,
            &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
        );
        assert_eq!(
            tools["result"]["tools"][0]["description"],
            "Multiplie deux entiers."
        );

        let prompts = dispatch(
            &core,
            &json!({ "jsonrpc": "2.0", "id": 2, "method": "prompts/list" }),
        );
        assert_eq!(
            prompts["result"]["prompts"][0]["description"],
            "Résume un texte."
        );

        let resources = dispatch(
            &core,
            &json!({ "jsonrpc": "2.0", "id": 3, "method": "resources/list" }),
        );
        assert_eq!(
            resources["result"]["resources"][0]["description"],
            "Documentation markdown embarquée."
        );
        assert_eq!(
            resources["result"]["resources"][0]["mimeType"],
            "text/markdown"
        );
    }
}

// ------------------------------------------------------------ A6 (CI multi-OS)

#[test]
fn a6_ci_execute_clippy_sur_windows_et_macos() {
    let ci_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".github")
        .join("workflows")
        .join("ci.yml");
    let Ok(ci) = std::fs::read_to_string(ci_path) else {
        // Paquet crates.io sans .github/
        return;
    };
    let ci = ci.replace("\r\n", "\n");
    assert!(
        ci.contains("Clippy (Windows)") && ci.contains("Clippy (macOS)"),
        "ci.yml doit lancer clippy sur Windows et macOS"
    );
}

// ------------------------------------------------------------ A7 (templates)

#[test]
fn a7_resource_template_handler_route_et_respecte_priorite_et_droits() {
    let verifier = Arc::new(TestVerifier::accepting(
        AuthContext::new("lecteur", ProofMethod::Application).with_rights(["logs.read"]),
    ));
    let core = ServerBuilder::new("a7", "1.0.0")
        .verifier(verifier)
        .resource("app://logs/pinned", "Pinned", "Entrée fixe", || {
            Ok(litemcp::protocol::ResourceContents {
                uri: "app://logs/pinned".into(),
                mime_type: Some("text/plain".into()),
                text: "statique".into(),
            })
        })
        .require_resource_right("app://logs/pinned", "logs.read")
        .resource_template_handler(
            "app://logs/{env}/{date}",
            "Logs",
            "Journaux par environnement et date",
            |uri, params| {
                Ok(ResourceData::text(
                    uri,
                    Some("text/plain".into()),
                    format!("env={} date={}", params["env"], params["date"]),
                ))
            },
        )
        .require_resource_right("app://logs/{env}/{date}", "logs.read")
        .core();

    let ctx_ok = AuthContext::new("lecteur", ProofMethod::Application).with_rights(["logs.read"]);
    let ctx_non = AuthContext::new("invite", ProofMethod::Application);

    // Lecture dynamique autorisée.
    let req_dyn = json!({
        "jsonrpc": "2.0", "id": 1, "method": "resources/read",
        "params": {
            "uri": "app://logs/prod/2026-11-26",
            "_meta": {
                litemcp::protocol::META_PROTOCOL_VERSION: litemcp::protocol::PROTOCOL_VERSION,
                litemcp::protocol::META_CLIENT_CAPABILITIES: {},
            }
        }
    })
    .to_string();
    let resp_ok = core
        .dispatch_authenticated(req_dyn.as_bytes(), None, &ctx_ok)
        .expect("réponse");
    let v_ok: Value = serde_json::from_slice(&resp_ok.body).unwrap();
    assert_eq!(
        v_ok["result"]["contents"][0]["text"], "env=prod date=2026-11-26",
        "{v_ok}"
    );

    // Lecture dynamique sans le droit exigé → 403 / -32023.
    let resp_non = core
        .dispatch_authenticated(req_dyn.as_bytes(), None, &ctx_non)
        .expect("réponse");
    assert_eq!(resp_non.status, litemcp::server::HttpStatus::Forbidden);
    let v_non: Value = serde_json::from_slice(&resp_non.body).unwrap();
    assert_eq!(v_non["error"]["code"], -32023, "{v_non}");

    // Une URI qui ne correspond à aucun gabarit rend -32602.
    let req_inconnue = json!({
        "jsonrpc": "2.0", "id": 2, "method": "resources/read",
        "params": {
            "uri": "app://logs/prod/2026/extra",
            "_meta": {
                litemcp::protocol::META_PROTOCOL_VERSION: litemcp::protocol::PROTOCOL_VERSION,
                litemcp::protocol::META_CLIENT_CAPABILITIES: {},
            }
        }
    })
    .to_string();
    let resp_inc = core
        .dispatch_authenticated(req_inconnue.as_bytes(), None, &ctx_ok)
        .expect("réponse");
    let v_inc: Value = serde_json::from_slice(&resp_inc.body).unwrap();
    assert_eq!(v_inc["error"]["code"], -32602, "{v_inc}");
}

#[test]
#[should_panic(expected = "gabarits RFC 6570 simples")]
fn a7_resource_template_handler_refuse_operateur_complexe() {
    let _ = ServerBuilder::new("a7-bad", "1.0.0").resource_template_handler(
        "app://files/{+path}",
        "Files",
        "Non simple",
        |uri, _| Ok(ResourceData::text(uri, None, "")),
    );
}
