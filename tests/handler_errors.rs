//! Même niveau d'erreur structurée sur les quatre surfaces de handler :
//! ressources, gabarits MRTR de ressources, prompts (y compris MRTR) et
//! complétions — comme les outils, et sans casser les handlers qui rendent
//! une `String`.

use litemcp::protocol::PromptMessage;
use litemcp::{PromptOutcome, ResourceOutcome, ServerBuilder, ServerCore, ToolError};
use serde_json::Value;

fn dispatch(core: &ServerCore, body: &str) -> Value {
    let reply = core
        .dispatch_unchecked(body.as_bytes())
        .expect("une requête identifiée reçoit une réponse");
    serde_json::from_slice(&reply).expect("réponse JSON-RPC lisible")
}

const READ: &str =
    r#"{"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":"mem://x"}}"#;
const GET_PROMPT: &str = r#"{"jsonrpc":"2.0","id":2,"method":"prompts/get","params":{"name":"p"}}"#;
const COMPLETE: &str = r#"{"jsonrpc":"2.0","id":3,"method":"completion/complete","params":{"ref":{"type":"ref/prompt","name":"p"},"argument":{"name":"a","value":"x"}}}"#;

fn assert_classified(v: &Value, code: &str, retryable: bool, retry_after: Option<u64>) {
    assert_eq!(v["error"]["code"], -32603, "{v}");
    assert_eq!(v["error"]["data"]["reason"], "handler_failed", "{v}");
    assert_eq!(v["error"]["data"]["code"], code, "{v}");
    assert_eq!(v["error"]["data"]["retryable"], retryable, "{v}");
    match retry_after {
        Some(ms) => assert_eq!(v["error"]["data"]["retryAfterMs"], ms, "{v}"),
        None => assert!(
            v["error"]["data"].get("retryAfterMs").is_none(),
            "retryAfterMs ne doit apparaître que s'il est conseillé : {v}"
        ),
    }
}

#[test]
fn ressource_data_rend_une_erreur_classifiee() {
    let core = ServerBuilder::new("e", "0.1.0")
        .resource_data_with_error("mem://x", "X", "quota", || {
            Err(ToolError::rate_limited("quota exhausted").retry_after_ms(500))
        })
        .core();
    let v = dispatch(&core, READ);
    assert_classified(&v, "rate_limited", true, Some(500));
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("quota exhausted"),
        "{v}"
    );
}

#[test]
fn ressource_texte_rend_une_erreur_classifiee() {
    let core = ServerBuilder::new("e", "0.1.0")
        .resource_with_error("mem://x", "X", "amont", || {
            Err(ToolError::upstream("database down"))
        })
        .core();
    let v = dispatch(&core, READ);
    assert_classified(&v, "upstream", true, None);
}

#[test]
fn ressource_mrtr_rend_une_erreur_classifiee() {
    let core = ServerBuilder::new("e", "0.1.0")
        .resource_mrtr_with_error("mem://x", "X", "mrtr", |_round| {
            Err::<ResourceOutcome, _>(ToolError::timeout("too slow"))
        })
        .core();
    let v = dispatch(&core, READ);
    assert_classified(&v, "timeout", true, None);
}

#[test]
fn prompt_rend_une_erreur_classifiee() {
    let core = ServerBuilder::new("e", "0.1.0")
        .prompt_with_error("p", "prompt", |_args| {
            Err::<Vec<PromptMessage>, _>(ToolError::invalid_input("argument manquant"))
        })
        .core();
    let v = dispatch(&core, GET_PROMPT);
    assert_classified(&v, "invalid_input", false, None);
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("argument manquant"),
        "{v}"
    );
}

#[test]
fn prompt_mrtr_rend_une_erreur_classifiee() {
    let core = ServerBuilder::new("e", "0.1.0")
        .prompt_mrtr_with_error("p", "prompt mrtr", |_args, _round| {
            Err::<PromptOutcome, _>(ToolError::internal("invariant cassé"))
        })
        .core();
    let v = dispatch(&core, GET_PROMPT);
    assert_classified(&v, "internal", false, None);
}

#[test]
fn completion_rend_une_erreur_classifiee() {
    let core = ServerBuilder::new("e", "0.1.0")
        .completions_with_error(|_request| {
            Err::<litemcp::protocol::Completion, _>(
                ToolError::rate_limited("index busy").retry_after_ms(250),
            )
        })
        .core();
    let v = dispatch(&core, COMPLETE);
    assert_classified(&v, "rate_limited", true, Some(250));
}

/// Les handlers existants qui rendent une `String` compilent et gardent leur
/// message ; l'erreur est classée `unclassified`, non retentable.
#[test]
fn handlers_string_restent_compatibles() {
    let core = ServerBuilder::new("e", "0.1.0")
        .resource_data("mem://x", "X", "string", || Err("boom".to_string()))
        .prompt("p", "string", |_args| Err("contexte manquant".to_string()))
        .completions(|_request| Err("index unavailable".to_string()))
        .core();
    let v = dispatch(&core, READ);
    assert_classified(&v, "unclassified", false, None);
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Resource read failed: boom"),
        "{v}"
    );
    let v = dispatch(&core, GET_PROMPT);
    assert_classified(&v, "unclassified", false, None);
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Prompt failed: contexte manquant"),
        "{v}"
    );
    let v = dispatch(&core, COMPLETE);
    assert_classified(&v, "unclassified", false, None);
}
