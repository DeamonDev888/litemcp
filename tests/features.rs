//! Tests unitaires des fonctionnalités : timeout d'exécution des handlers,
//! `outputSchema`, resources et prompts — via `ServerCore` (sans transport,
//! donc rapides et déterministes).

use litemcp::ServerBuilder;
use litemcp::protocol::PromptMessage;
use litemcp::zod;
use serde_json::{Value, json};
use std::time::Duration;

fn call(core: &litemcp::ServerCore, body: &str) -> Value {
    let reply = core
        .dispatch(body.as_bytes())
        .expect("une réponse est attendue pour une requête");
    serde_json::from_slice(&reply).unwrap()
}

fn call_tool(core: &litemcp::ServerCore, id: i64, name: &str, arguments: Value) -> Value {
    call(
        core,
        &format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{arguments}}}}}"#
        ),
    )
}

// --- Timeout d'exécution des handlers ---

#[test]
fn tool_timeout_cuts_a_slow_handler_into_an_is_error_result() {
    // Le handler dort 10 s ; le délai de 100 ms doit rendre un résultat
    // `isError` (pas une connexion pendante, pas de -32603).
    let core = ServerBuilder::new("t", "0.1.0")
        .tool_timeout(Duration::from_millis(100))
        .tool("lent", "dort longtemps", |_args| {
            std::thread::sleep(Duration::from_secs(10));
            Ok(json!({ "jamais": true }))
        })
        .core();
    let started = std::time::Instant::now();
    let v = call_tool(&core, 1, "lent", json!({}));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "le délai doit couper bien avant la fin du handler"
    );
    assert_eq!(v["result"]["isError"], true, "{v}");
    assert!(
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("délai"),
        "message de timeout attendu : {v}"
    );
    // Le résultat timeout reste un résultat : resultType + _meta serverInfo.
    assert_eq!(v["result"]["resultType"], "complete");
    assert!(
        v["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"].is_string(),
        "{v}"
    );
}

#[test]
fn tool_timeout_leaves_fast_handlers_untouched() {
    let core = ServerBuilder::new("t", "0.1.0")
        .tool_timeout(Duration::from_secs(5))
        .tool("rapide", "répond vite", |args| {
            let n = args["n"].as_i64().unwrap_or_default();
            Ok(json!({ "double": n * 2 }))
        })
        .core();
    let v = call_tool(&core, 1, "rapide", json!({ "n": 21 }));
    assert_eq!(v["result"]["structuredContent"]["double"], 42, "{v}");
    assert!(v["result"].get("isError").is_none(), "{v}");
}

#[test]
fn tool_timeout_maps_a_panicking_handler_to_internal_error() {
    // En mode borné, la panique est rattrapée dans le thread du handler et
    // rendue en -32603 (même verdict que le catch_unwind du transport).
    let core = ServerBuilder::new("t", "0.1.0")
        .tool_timeout(Duration::from_secs(5))
        .tool("boom", "panique exprès", |_args| {
            panic!("volontaire");
        })
        .core();
    let v = call_tool(&core, 1, "boom", json!({}));
    assert_eq!(v["error"]["code"], -32603, "{v}");
    assert_eq!(v["id"], 1, "l'id doit être préservé : {v}");
}

#[test]
fn tool_timeout_cuts_slow_resource_and_prompt_handlers() {
    // Même mécanisme pour resources/read et prompts/get : erreur -32000
    // (plage implementation-defined), pas de thread de connexion retenu.
    let core = ServerBuilder::new("t", "0.1.0")
        .tool_timeout(Duration::from_millis(100))
        .resource("mem://lent", "Lent", "dort", || {
            std::thread::sleep(Duration::from_secs(10));
            unreachable!("le handler doit être abandonné")
        })
        .prompt("lent", "dort", |_args| {
            std::thread::sleep(Duration::from_secs(10));
            unreachable!("le handler doit être abandonné")
        })
        .core();

    let v = call(
        &core,
        r#"{"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":"mem://lent"}}"#,
    );
    assert_eq!(v["error"]["code"], -32000, "{v}");
    assert!(
        v["error"]["message"].as_str().unwrap().contains("délai"),
        "{v}"
    );

    let v = call(
        &core,
        r#"{"jsonrpc":"2.0","id":2,"method":"prompts/get","params":{"name":"lent"}}"#,
    );
    assert_eq!(v["error"]["code"], -32000, "{v}");
    assert!(
        v["error"]["message"].as_str().unwrap().contains("délai"),
        "{v}"
    );
}

// --- outputSchema : publié ET exécuteur ---

#[test]
fn output_schema_is_published_and_enforced() {
    let core = ServerBuilder::new("t", "0.1.0")
        .tool("bon", "sortie conforme", |_args| Ok(json!({ "total": 2 })))
        .output_schema("bon", zod::object().field("total", zod::integer()))
        // Ordre inverse : output_schema AVANT l'enregistrement de l'outil.
        .output_schema("mauvais", zod::object().field("total", zod::integer()))
        .tool("mauvais", "sortie non conforme", |_args| {
            Ok(json!({ "autre": 1 }))
        })
        .core();

    // Publié dans tools/list.
    let v = call(&core, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let tools = v["result"]["tools"].as_array().unwrap();
    let bon = tools.iter().find(|t| t["name"] == "bon").unwrap();
    assert_eq!(
        bon["outputSchema"]["properties"]["total"]["type"], "integer",
        "{}",
        v
    );
    let mauvais = tools.iter().find(|t| t["name"] == "mauvais").unwrap();
    assert!(
        mauvais["outputSchema"]["required"]
            .as_array()
            .unwrap()
            .contains(&json!("total")),
        "{}",
        v
    );

    // Sortie conforme : servie telle quelle.
    let v = call_tool(&core, 1, "bon", json!({}));
    assert_eq!(v["result"]["structuredContent"]["total"], 2, "{v}");
    assert!(v["result"].get("isError").is_none(), "{v}");

    // Sortie non conforme : le handler a exécuté, c'est sa sortie qui
    // viole le contrat publié → résultat isError (pas une erreur JSON-RPC).
    let v = call_tool(&core, 2, "mauvais", json!({}));
    assert_eq!(v["result"]["isError"], true, "{v}");
    assert!(
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("outputSchema"),
        "le message doit citer le outputSchema : {v}"
    );
}

// --- Resources ---

#[test]
fn resources_list_and_read_work() {
    let core = ServerBuilder::new("t", "0.1.0")
        .resource("mem://doc", "Doc", "La documentation", || {
            Ok(litemcp::protocol::ResourceContents {
                uri: "mem://doc".to_string(),
                mime_type: Some("text/markdown".to_string()),
                text: "# Bonjour".to_string(),
            })
        })
        .core();

    // Liste : forme SEP-2549 + identification SEP-2575.
    let v = call(
        &core,
        r#"{"jsonrpc":"2.0","id":1,"method":"resources/list"}"#,
    );
    assert_eq!(v["result"]["resources"][0]["uri"], "mem://doc", "{v}");
    assert_eq!(v["result"]["resources"][0]["name"], "Doc", "{v}");
    assert_eq!(v["result"]["resultType"], "complete", "{v}");
    assert!(v["result"]["ttlMs"].is_u64(), "{v}");
    assert_eq!(v["result"]["cacheScope"], "private", "{v}");
    assert!(
        v["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"].is_string(),
        "{v}"
    );

    // Lecture : le contenu du handler, tel quel.
    let v = call(
        &core,
        r#"{"jsonrpc":"2.0","id":2,"method":"resources/read","params":{"uri":"mem://doc"}}"#,
    );
    assert_eq!(v["result"]["contents"][0]["text"], "# Bonjour", "{v}");
    assert_eq!(
        v["result"]["contents"][0]["mimeType"], "text/markdown",
        "{v}"
    );
    assert_eq!(v["result"]["contents"][0]["uri"], "mem://doc", "{v}");

    // Uri inconnue / manquante : -32602.
    let v = call(
        &core,
        r#"{"jsonrpc":"2.0","id":3,"method":"resources/read","params":{"uri":"mem://inconnu"}}"#,
    );
    assert_eq!(v["error"]["code"], -32602, "{v}");
    let v = call(
        &core,
        r#"{"jsonrpc":"2.0","id":4,"method":"resources/read","params":{}}"#,
    );
    assert_eq!(v["error"]["code"], -32602, "{v}");

    // Handler en échec : -32000 (plage implementation-defined).
    let core = ServerBuilder::new("t", "0.1.0")
        .resource("mem://ko", "Ko", "échoue", || {
            Err("fichier absent".to_string())
        })
        .core();
    let v = call(
        &core,
        r#"{"jsonrpc":"2.0","id":5,"method":"resources/read","params":{"uri":"mem://ko"}}"#,
    );
    assert_eq!(v["error"]["code"], -32000, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("fichier absent"),
        "{v}"
    );
}

#[test]
fn resources_and_prompts_capabilities_track_their_registries() {
    // Une capacité annoncée est une capacité servie : sans registre, pas
    // d'annonce ; avec, l'objet de capacité apparaît.
    let vide = ServerBuilder::new("t", "0.1.0").core();
    let v = call(
        &vide,
        r#"{"jsonrpc":"2.0","id":1,"method":"server/discover"}"#,
    );
    assert!(v["result"]["capabilities"]["tools"].is_object(), "{v}");
    assert!(
        v["result"]["capabilities"].get("resources").is_none(),
        "aucune ressource enregistrée : pas d'annonce : {v}"
    );
    assert!(v["result"]["capabilities"].get("prompts").is_none(), "{v}");

    let plein = ServerBuilder::new("t", "0.1.0")
        .resource("mem://x", "X", "x", || {
            Ok(litemcp::protocol::ResourceContents {
                uri: "mem://x".to_string(),
                mime_type: None,
                text: String::new(),
            })
        })
        .prompt("p", "un prompt", |_args| {
            Ok(vec![PromptMessage::user("salut")])
        })
        .core();
    let v = call(
        &plein,
        r#"{"jsonrpc":"2.0","id":2,"method":"server/discover"}"#,
    );
    assert!(v["result"]["capabilities"]["resources"].is_object(), "{v}");
    assert!(v["result"]["capabilities"]["prompts"].is_object(), "{v}");
}

// --- Prompts ---

#[test]
fn prompts_list_and_get_work() {
    let core = ServerBuilder::new("t", "0.1.0")
        .prompt("salut", "Salue quelqu'un", |args| {
            let nom = args.get("nom").and_then(Value::as_str).unwrap_or("monde");
            Ok(vec![
                PromptMessage::user(format!("Bonjour {nom}.")),
                PromptMessage::assistant("Enchanté !"),
            ])
        })
        .core();

    // Liste.
    let v = call(&core, r#"{"jsonrpc":"2.0","id":1,"method":"prompts/list"}"#);
    assert_eq!(v["result"]["prompts"][0]["name"], "salut", "{v}");
    assert_eq!(
        v["result"]["prompts"][0]["description"], "Salue quelqu'un",
        "{v}"
    );
    assert_eq!(v["result"]["resultType"], "complete", "{v}");
    assert!(v["result"]["ttlMs"].is_u64(), "{v}");
    assert_eq!(v["result"]["cacheScope"], "private", "{v}");

    // Get avec arguments.
    let v = call(
        &core,
        r#"{"jsonrpc":"2.0","id":2,"method":"prompts/get","params":{"name":"salut","arguments":{"nom":"Léa"}}}"#,
    );
    let messages = v["result"]["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2, "{v}");
    assert_eq!(messages[0]["role"], "user", "{v}");
    assert_eq!(messages[0]["content"]["type"], "text", "{v}");
    assert_eq!(messages[0]["content"]["text"], "Bonjour Léa.", "{v}");
    assert_eq!(messages[1]["role"], "assistant", "{v}");

    // Get sans arguments : le défaut du handler s'applique.
    let v = call(
        &core,
        r#"{"jsonrpc":"2.0","id":3,"method":"prompts/get","params":{"name":"salut"}}"#,
    );
    assert_eq!(
        v["result"]["messages"][0]["content"]["text"], "Bonjour monde.",
        "{v}"
    );

    // Nom inconnu → -32602 ; arguments non-objet → -32602 (même contrat que
    // tools/call).
    let v = call(
        &core,
        r#"{"jsonrpc":"2.0","id":4,"method":"prompts/get","params":{"name":"inconnu"}}"#,
    );
    assert_eq!(v["error"]["code"], -32602, "{v}");
    let v = call(
        &core,
        r#"{"jsonrpc":"2.0","id":5,"method":"prompts/get","params":{"name":"salut","arguments":"x"}}"#,
    );
    assert_eq!(v["error"]["code"], -32602, "{v}");

    // Handler en échec → -32000.
    let core = ServerBuilder::new("t", "0.1.0")
        .prompt("ko", "échoue", |_args| Err("contexte manquant".to_string()))
        .core();
    let v = call(
        &core,
        r#"{"jsonrpc":"2.0","id":6,"method":"prompts/get","params":{"name":"ko"}}"#,
    );
    assert_eq!(v["error"]["code"], -32000, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("contexte manquant"),
        "{v}"
    );
}

// --- prompt_typed : validation avant exécution ---

#[test]
fn prompt_typed_validates_arguments_before_running_the_handler() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let ran = std::sync::Arc::new(AtomicBool::new(false));
    let probe = std::sync::Arc::clone(&ran);
    let core = ServerBuilder::new("t", "0.1.0")
        .prompt_typed(
            "salut",
            "Salue",
            zod::object().field("nom", zod::string().min(1)),
            move |_args| {
                probe.store(true, Ordering::Relaxed);
                Ok(vec![PromptMessage::user("salut")])
            },
        )
        .core();

    // Invalide (nom absent) : -32602 avec issues, handler jamais exécuté.
    let v = call(
        &core,
        r#"{"jsonrpc":"2.0","id":1,"method":"prompts/get","params":{"name":"salut","arguments":{}}}"#,
    );
    assert_eq!(v["error"]["code"], -32602, "{v}");
    assert!(v["error"]["data"]["issues"].as_array().is_some(), "{v}");
    assert!(
        !ran.load(Ordering::Relaxed),
        "le handler n'a pas dû tourner"
    );

    // Valide : messages rendus, handler exécuté.
    let v = call(
        &core,
        r#"{"jsonrpc":"2.0","id":2,"method":"prompts/get","params":{"name":"salut","arguments":{"nom":"Léa"}}}"#,
    );
    assert_eq!(
        v["result"]["messages"][0]["content"]["text"], "salut",
        "{v}"
    );
    assert!(ran.load(Ordering::Relaxed), "le handler a dû tourner");
}
