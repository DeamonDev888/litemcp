//! Tests de régression des correctifs Tier 1 de l'audit ergonomique
//! (2026-10-25) : erreurs structurées (`ToolError`), hooks d'observation
//! (`on_call`) et messages de validation en anglais.

use litemcp::prelude::*;
use serde_json::json;
use std::sync::Mutex;

fn call(core: &litemcp::ServerCore, body: &str) -> serde_json::Value {
    let reply = core.dispatch(body.as_bytes()).unwrap();
    serde_json::from_slice(&reply).unwrap()
}

fn call_tool(
    core: &litemcp::ServerCore,
    name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    call(
        core,
        &format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{name}","arguments":{arguments}}}}}"#
        ),
    )
}

// --- Correctif 1 : erreurs structurées ---

#[test]
fn typed_tool_error_reaches_the_wire_as_structured_content() {
    // L'agent client n'a plus à parser la prose : `structuredContent.error`
    // porte code, politique de retente et délai conseillé.
    let core = ServerBuilder::new("t", "0.1.0")
        .tool("quota", "limité", |_args| {
            Err(ToolError::rate_limited("quota exhausted").retry_after_ms(500))
        })
        .core();
    let v = call_tool(&core, "quota", json!({}));
    assert!(
        v.get("error").is_none(),
        "un échec d'outil est un result : {v}"
    );
    assert_eq!(v["result"]["isError"], true);
    // Le texte reste le message exact (compatibilité clients texte).
    assert_eq!(v["result"]["content"][0]["text"], "quota exhausted");
    assert_eq!(
        v["result"]["structuredContent"]["error"],
        json!({
            "code": "rate_limited",
            "message": "quota exhausted",
            "retryable": true,
            "retryAfterMs": 500,
        })
    );
}

#[test]
fn string_error_results_still_work_through_the_question_operator() {
    // `Err(String)` littéral ne compile plus (le type est ToolError), mais
    // la promesse de compatibilité tient là où elle compte : l'opérateur
    // `?` sur un `Result<_, String>` du domaine applicatif convertit via
    // `From<String>` — classée `unclassified`, texte intact.
    fn domaine() -> Result<i64, String> {
        Err("a manquant".to_string())
    }
    let core = ServerBuilder::new("t", "0.1.0")
        .tool("boom", "échec prose", |_args| {
            let a = domaine()?; // Result<i64, String> → ToolError par `?`
            Ok(json!({ "a": a }))
        })
        .core();
    let v = call_tool(&core, "boom", json!({}));
    assert_eq!(v["result"]["isError"], true);
    assert_eq!(v["result"]["content"][0]["text"], "a manquant");
    assert_eq!(
        v["result"]["structuredContent"]["error"]["code"],
        "unclassified"
    );
    assert_eq!(
        v["result"]["structuredContent"]["error"]["retryable"],
        false
    );
    // Pas de `retryAfterMs` conseillé → champ absent (pas `null`).
    assert!(
        v["result"]["structuredContent"]["error"]
            .get("retryAfterMs")
            .is_none()
    );
}

#[test]
fn ok_or_pattern_still_compiles_with_the_typed_error() {
    // Le motif documenté `.ok_or("…")?` doit continuer de compiler via
    // `From<&str>` — c'est la promesse de compatibilité source.
    let core = ServerBuilder::new("t", "0.1.0")
        .tool("add", "additionne", |args| {
            let a = args["a"].as_i64().ok_or("a manquant")?;
            let b = args["b"].as_i64().ok_or("b manquant")?;
            Ok(json!({ "somme": a + b }))
        })
        .core();
    let ok = call_tool(&core, "add", json!({ "a": 1, "b": 2 }));
    assert_eq!(ok["result"]["structuredContent"]["somme"], 3);
    let bad = call_tool(&core, "add", json!({ "a": 1 }));
    assert_eq!(bad["result"]["isError"], true);
    assert_eq!(bad["result"]["content"][0]["text"], "b manquant");
}

#[test]
fn timeout_is_classified_retryable() {
    // Le délai de `tool_timeout` devient `timeout` (retentable) : le client
    // sait qu'il peut rejouer la requête.
    let core = ServerBuilder::new("t", "0.1.0")
        .tool_timeout(std::time::Duration::from_millis(50))
        .tool("lent", "dort", |_args| {
            std::thread::sleep(std::time::Duration::from_millis(2_000));
            Ok(json!({}))
        })
        .core();
    let v = call_tool(&core, "lent", json!({}));
    assert_eq!(v["result"]["isError"], true);
    assert_eq!(v["result"]["structuredContent"]["error"]["code"], "timeout");
    assert_eq!(v["result"]["structuredContent"]["error"]["retryable"], true);
}

#[test]
fn output_schema_violation_is_classified_internal() {
    // Le handler a réussi mais sa sortie viole le contrat publié :
    // échec `internal`, non retentable.
    let core = ServerBuilder::new("t", "0.1.0")
        .output_schema("bad", zod::object().field("wanted", zod::integer()))
        .tool("bad", "mauvaise sortie", |_| Ok(json!({ "autre": 1 })))
        .core();
    let v = call_tool(&core, "bad", json!({}));
    assert_eq!(v["result"]["isError"], true);
    assert_eq!(
        v["result"]["structuredContent"]["error"]["code"],
        "internal"
    );
    assert_eq!(
        v["result"]["structuredContent"]["error"]["retryable"],
        false
    );
    assert!(
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("outputSchema"),
        "le message cite le contrat violé : {v}"
    );
}

// --- Correctif 2 : hooks d'observation ---

#[test]
fn on_call_observes_methods_names_and_statuses() {
    // Un seul callback → compteurs par outil, latence, verdict.
    #[derive(Debug, Clone, PartialEq)]
    struct Observed {
        method: String,
        name: Option<String>,
        status: CallStatus,
    }
    let seen: std::sync::Arc<Mutex<Vec<Observed>>> = std::sync::Arc::new(Mutex::new(Vec::new()));
    let recorder = std::sync::Arc::clone(&seen);
    let core = ServerBuilder::new("t", "0.1.0")
        .tool("ok", "réussit", |_| Ok(json!({ "ok": true })))
        .tool("ko", "échoue", |_| Err(ToolError::upstream("api down")))
        .resource("mem://doc", "doc", "documentation", || {
            Ok(litemcp::protocol::ResourceContents {
                uri: "mem://doc".into(),
                mime_type: None,
                text: " contenu".into(),
            })
        })
        .on_call(move |event| {
            recorder.lock().unwrap().push(Observed {
                method: event.method.to_string(),
                name: event.name.map(str::to_string),
                status: event.status,
            });
        })
        .core();

    let _ = call_tool(&core, "ok", json!({}));
    let _ = call_tool(&core, "ko", json!({}));
    let _ = call_tool(&core, "inconnu", json!({}));
    let _ = call(
        &core,
        r#"{"jsonrpc":"2.0","id":4,"method":"resources/read","params":{"uri":"mem://doc"}}"#,
    );
    let _ = call(
        &core,
        r#"{"jsonrpc":"2.0","id":5,"method":"methode/aucune"}"#,
    );
    // Notification d'action refusée : observée aussi (verdict -32600).
    let _ = core.dispatch(
        br#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"ok","arguments":{}}}"#,
    );
    // Notification de protocole : acceptée et ignorée → observée comme
    // telle.
    let _ = core.dispatch(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);

    let seen = seen.lock().unwrap().clone();
    let of = |method: &str| seen.iter().find(|o| o.method == method).cloned();

    // Deux tools/call réussi + échoué + refus de notification : on vérifie
    // les deux premiers via leur nom.
    assert!(
        seen.iter().any(|o| o.method == "tools/call"
            && o.name.as_deref() == Some("ok")
            && o.status == CallStatus::Success),
        "succès observé : {seen:?}"
    );
    assert!(
        seen.iter().any(|o| o.method == "tools/call"
            && o.name.as_deref() == Some("ko")
            && o.status == CallStatus::ToolFailure(ErrorCode::Upstream)),
        "échec classé observé : {seen:?}"
    );
    assert!(
        seen.iter().any(|o| o.method == "tools/call"
            && o.name.as_deref() == Some("inconnu")
            && o.status == CallStatus::ProtocolError(-32602)),
        "outil inconnu observé : {seen:?}"
    );
    let read = of("resources/read").expect("resources/read observé");
    assert_eq!(read.name.as_deref(), Some("mem://doc"), "{read:?}");
    assert_eq!(read.status, CallStatus::Success);
    let unknown = of("methode/aucune").expect("méthode inconnue observée");
    assert_eq!(unknown.status, CallStatus::ProtocolError(-32601));
    // La notification d'action refusée arrive APRÈS le traitement (donc
    // en 5e position des tools/call) ; la notification de protocole a un
    // verdict Notification.
    assert!(
        seen.iter().any(
            |o| o.method == "notifications/initialized" && o.status == CallStatus::Notification
        ),
        "{seen:?}"
    );
    // Un événement par message, ni plus ni moins.
    assert_eq!(seen.len(), 7, "{seen:?}");
}

#[test]
fn on_call_reports_validation_rejections_before_the_handler_runs() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let ran = std::sync::Arc::new(AtomicUsize::new(0));
    let rejected = std::sync::Arc::new(AtomicUsize::new(0));
    let ran_hook = std::sync::Arc::clone(&ran);
    let rejected_hook = std::sync::Arc::clone(&rejected);
    let core = ServerBuilder::new("t", "0.1.0")
        .tool_typed(
            "typed",
            "arguments typés",
            zod::object().field("direction", zod::enumeration(&["up", "down"])),
            move |_| {
                ran_hook.fetch_add(1, Ordering::SeqCst);
                Ok(json!({}))
            },
        )
        .on_call(move |event| {
            if event.status == CallStatus::ProtocolError(-32602) {
                rejected_hook.fetch_add(1, Ordering::SeqCst);
            }
        })
        .core();
    let _ = call_tool(&core, "typed", json!({ "direction": "sideways" }));
    assert_eq!(ran.load(Ordering::SeqCst), 0, "le handler ne tourne jamais");
    assert_eq!(rejected.load(Ordering::SeqCst), 1);
}

#[test]
fn a_panicking_hook_never_stops_the_server() {
    // L'observation doit rester un détail : un hook qui panique ne casse
    // ni la requête ni les suivantes.
    let core = ServerBuilder::new("t", "0.1.0")
        .tool("ok", "réussit", |_| Ok(json!({ "ok": true })))
        .on_call(|_event| panic!("hook cassé"))
        .core();
    let v = call_tool(&core, "ok", json!({}));
    assert_eq!(v["result"]["structuredContent"]["ok"], true);
    // Requête suivante : toujours servie.
    let v = call(&core, r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
    assert!(v["result"]["tools"].as_array().unwrap().len() == 1);
}

#[test]
fn multiple_on_call_hooks_accumulate() {
    let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
    let first = std::sync::Arc::clone(&hits);
    let second = std::sync::Arc::clone(&hits);
    let core = ServerBuilder::new("t", "0.1.0")
        .tool("ok", "réussit", |_| Ok(json!({})))
        .on_call(move |_| first.lock().unwrap().push(1))
        .on_call(move |_| second.lock().unwrap().push(2))
        .core();
    let _ = call_tool(&core, "ok", json!({}));
    assert_eq!(*hits.lock().unwrap(), vec![1, 2], "chaque hook est appelé");
}

// --- Correctif 3 : messages de validation en anglais ---

#[test]
fn validation_issues_are_english() {
    // Les `-32602` partent chez des clients du monde entier : la prose de
    // validation est en anglais par défaut.
    let core = ServerBuilder::new("t", "0.1.0")
        .tool_typed(
            "move",
            "déplace",
            zod::object()
                .field("direction", zod::enumeration(&["up", "down"]))
                .field("steps", zod::integer())
                .field("label", zod::string().min(2)),
            |_| Ok(json!({})),
        )
        .core();
    let v = call_tool(
        &core,
        "move",
        json!({ "direction": "sideways", "steps": "beaucoup" }),
    );
    assert_eq!(v["error"]["code"], -32602);
    let issues: Vec<String> = v["error"]["data"]["issues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i.as_str().unwrap().to_string())
        .collect();
    assert!(
        issues.iter().any(|i| i.contains("must be one of up, down")),
        "{issues:?}"
    );
    assert!(
        issues.iter().any(|i| i.contains("expected an integer")),
        "{issues:?}"
    );
    // Champ requis manquant, en anglais aussi.
    let v = call_tool(&core, "move", json!({ "direction": "up", "steps": 1 }));
    let issues = v["error"]["data"]["issues"].as_array().unwrap();
    assert!(
        issues
            .iter()
            .any(|i| i.as_str().unwrap().contains("required")),
        "{issues:?}"
    );
}

#[test]
fn protocol_errors_are_english() {
    let core = ServerBuilder::new("t", "0.1.0")
        .tool("existing", "existe", |_| Ok(json!({})))
        .core();
    let v = call(
        &core,
        r#"{"jsonrpc":"2.0","id":1,"method":"aucune/methode"}"#,
    );
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("Unknown method:"),
        "{v}"
    );
    let v = call(
        &core,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"existing","arguments":7}}"#,
    );
    assert_eq!(v["error"]["message"], "params.arguments must be an object");
    let v = call(
        &core,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{}}"#,
    );
    assert_eq!(v["error"]["message"], "missing params.name");
    // JSON illisible : parse error en anglais.
    let reply = core.dispatch(b"not json").unwrap();
    let v: serde_json::Value = serde_json::from_slice(&reply).unwrap();
    assert_eq!(v["error"]["message"], "invalid JSON");
}

// --- Audit complet 2026-10-25 : correctifs P2/P3 ---

#[test]
fn describe_and_nullable_preserve_inner_default_values() {
    let schema = zod::object()
        .field("a", zod::describe("compteur", zod::integer().default(10)))
        .field("b", zod::nullable(zod::string().default("ok")));
    let json_schema = schema.json();
    assert!(
        json_schema.get("required").is_none(),
        "les champs avec défaut interne ne doivent pas être required : {json_schema}"
    );
    assert_eq!(json_schema["properties"]["a"]["default"], json!(10));
    assert_eq!(
        json_schema["properties"]["a"]["description"],
        json!("compteur")
    );
    assert_eq!(json_schema["properties"]["b"]["default"], json!("ok"));

    let core = ServerBuilder::new("t", "0.1.0")
        .tool_typed("cfg", "config", schema, |args| Ok(args.clone()))
        .core();
    let v = call_tool(&core, "cfg", json!({}));
    assert!(v.get("error").is_none(), "{v}");
    assert_eq!(
        v["result"]["structuredContent"],
        json!({ "a": 10, "b": "ok" })
    );
}

#[test]
fn nested_object_default_recursively_applies_inner_defaults() {
    let inner = zod::object().field("retries", zod::integer().default(3));
    let outer = zod::object().field("cfg", inner.default(json!({})));
    let core = ServerBuilder::new("t", "0.1.0")
        .tool_typed("nested", "nested", outer, |args| Ok(args.clone()))
        .core();
    let v = call_tool(&core, "nested", json!({}));
    assert!(v.get("error").is_none(), "{v}");
    assert_eq!(
        v["result"]["structuredContent"],
        json!({ "cfg": { "retries": 3 } })
    );
}

#[test]
fn record_supports_default_value() {
    let schema = zod::object().field(
        "headers",
        zod::record(zod::string()).default(json!({ "x-env": "prod" })),
    );
    let core = ServerBuilder::new("t", "0.1.0")
        .tool_typed("req", "req", schema, |args| Ok(args.clone()))
        .core();
    let v = call_tool(&core, "req", json!({}));
    assert_eq!(
        v["result"]["structuredContent"],
        json!({ "headers": { "x-env": "prod" } })
    );
}

#[test]
fn datetime_rejects_out_of_range_timezone_offsets() {
    let core = ServerBuilder::new("t", "0.1.0")
        .tool_typed(
            "dt",
            "date",
            zod::object().field("at", zod::string().datetime()),
            |args| Ok(args.clone()),
        )
        .core();
    for bad in [
        "2026-10-25T12:00:00+24:00",
        "2026-10-25T12:00:00+02:60",
        "2026-10-25T12:00:00-99:99",
    ] {
        let v = call_tool(&core, "dt", json!({ "at": bad }));
        assert_eq!(
            v["error"]["code"], -32602,
            "offset invalide accepté : {bad}"
        );
    }
    let ok = call_tool(&core, "dt", json!({ "at": "2026-10-25T12:00:00+23:59" }));
    assert!(ok.get("error").is_none(), "{ok}");
}

#[test]
fn prompt_typed_injects_schema_defaults_and_publishes_arguments_in_list() {
    let core = ServerBuilder::new("t", "0.1.0")
        .prompt_typed(
            "review",
            "Code review",
            zod::object()
                .field("file", zod::describe("Target file", zod::string()))
                .field("style", zod::string().default("concise")),
            |args| {
                let file = args["file"].as_str().unwrap_or("");
                let style = args["style"].as_str().unwrap_or("missing");
                Ok(vec![litemcp::protocol::PromptMessage::user(format!(
                    "{file}:{style}"
                ))])
            },
        )
        .core();

    let list = call(&core, r#"{"jsonrpc":"2.0","id":1,"method":"prompts/list"}"#);
    let args = list["result"]["prompts"][0]["arguments"]
        .as_array()
        .expect("arguments publiés");
    assert_eq!(args.len(), 2);
    let file_arg = args.iter().find(|a| a["name"] == "file").unwrap();
    let style_arg = args.iter().find(|a| a["name"] == "style").unwrap();
    assert_eq!(file_arg["description"], json!("Target file"));
    assert_eq!(file_arg["required"], json!(true));
    assert_eq!(style_arg["required"], json!(false));

    let got = call(
        &core,
        r#"{"jsonrpc":"2.0","id":2,"method":"prompts/get","params":{"name":"review","arguments":{"file":"src/lib.rs"}}}"#,
    );
    assert_eq!(
        got["result"]["messages"][0]["content"]["text"],
        "src/lib.rs:concise"
    );
}

#[test]
fn panicking_handlers_without_timeout_return_32603_and_notify_on_call() {
    let statuses = std::sync::Arc::new(Mutex::new(Vec::new()));
    let recorder = std::sync::Arc::clone(&statuses);
    let core = ServerBuilder::new("t", "0.1.0")
        .tool("boom_tool", "panique", |_| panic!("tool panic"))
        .resource("mem://boom", "boom", "panique", || panic!("resource panic"))
        .prompt("boom_prompt", "panique", |_| panic!("prompt panic"))
        .on_call(move |event| {
            recorder.lock().unwrap().push((
                event.method.to_string(),
                event.name.map(str::to_string),
                event.status,
            ));
        })
        .core();

    let v1 = call_tool(&core, "boom_tool", json!({}));
    assert_eq!(v1["error"]["code"], -32603);
    assert_eq!(v1["id"], 1);

    let v2 = call(
        &core,
        r#"{"jsonrpc":"2.0","id":2,"method":"resources/read","params":{"uri":"mem://boom"}}"#,
    );
    assert_eq!(v2["error"]["code"], -32603);
    assert_eq!(v2["id"], 2);

    let v3 = call(
        &core,
        r#"{"jsonrpc":"2.0","id":3,"method":"prompts/get","params":{"name":"boom_prompt"}}"#,
    );
    assert_eq!(v3["error"]["code"], -32603);
    assert_eq!(v3["id"], 3);

    let recorded = statuses.lock().unwrap().clone();
    assert_eq!(
        recorded,
        vec![
            (
                "tools/call".to_string(),
                Some("boom_tool".to_string()),
                CallStatus::ProtocolError(-32603),
            ),
            (
                "resources/read".to_string(),
                Some("mem://boom".to_string()),
                CallStatus::ProtocolError(-32603),
            ),
            (
                "prompts/get".to_string(),
                Some("boom_prompt".to_string()),
                CallStatus::ProtocolError(-32603),
            ),
        ]
    );
}

#[test]
fn mcp_routing_headers_are_enforced_on_notifications_and_missing_body_name() {
    let core = ServerBuilder::new("t", "0.1.0")
        .tool("ok", "ok", |_| Ok(json!({})))
        .core();

    // Mcp-Name présent mais params.name absent sur tools/call -> -32020.
    let headers = litemcp::server::McpHeaders {
        method: Some("tools/call".to_string()),
        name: Some("ok".to_string()),
    };
    let reply = core
        .dispatch_with_headers(
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{}}"#,
            Some(&headers),
        )
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&reply).unwrap();
    assert_eq!(v["error"]["code"], -32020, "{v}");

    // Mcp-Method incohérent sur une notification -> -32020 (pas ignoré en 202).
    let bad_method = litemcp::server::McpHeaders {
        method: Some("ping".to_string()),
        name: None,
    };
    let reply = core
        .dispatch_with_headers(
            br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            Some(&bad_method),
        )
        .expect("erreur -32020 attendue");
    let v: serde_json::Value = serde_json::from_slice(&reply).unwrap();
    assert_eq!(v["error"]["code"], -32020, "{v}");
}

#[test]
fn shutdown_wakes_server_bound_to_unspecified_ipv4_address() {
    let bound = ServerBuilder::new("stop", "0.1.0")
        .tool("ok", "ok", |_| Ok(json!({})))
        .bind("0.0.0.0:0")
        .unwrap();
    let handle = bound.handle();
    let server = std::thread::spawn(move || bound.serve());
    std::thread::sleep(std::time::Duration::from_millis(100));

    handle.shutdown();
    let started = std::time::Instant::now();
    server
        .join()
        .expect("serve ne doit pas paniquer")
        .expect("serve doit rendre Ok(())");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "shutdown sur 0.0.0.0 doit réveiller accept immédiatement"
    );
}
