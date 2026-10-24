//! Tests de l'adaptateur schemars (feature `schemars`) : un type
//! `#[derive(JsonSchema)]` devient un schéma exécuteur — publié ET validé.

#![cfg(feature = "schemars")]

use litemcp::ServerBuilder;
use litemcp::schemars::from_schemars;
use schemars::JsonSchema;
use serde_json::{Value, json};

#[derive(JsonSchema)]
#[allow(dead_code)] // les champs servent à la dérivation du schéma, pas à la lecture
struct SearchArgs {
    /// Termes de la recherche.
    query: String,
    #[schemars(range(min = 1, max = 100))]
    limit: Option<u8>,
    tags: Vec<String>,
}

fn call(core: &litemcp::ServerCore, body: &str) -> Value {
    let reply = core.dispatch(body.as_bytes()).unwrap();
    serde_json::from_slice(&reply).unwrap()
}

fn call_tool(core: &litemcp::ServerCore, id: i64, arguments: Value) -> Value {
    call(
        core,
        &format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"search","arguments":{arguments}}}}}"#
        ),
    )
}

#[test]
fn schemars_schema_is_derived_and_published() {
    let core = ServerBuilder::new("m", "0.1.0")
        .tool_typed(
            "search",
            "Recherche",
            from_schemars::<SearchArgs>(),
            |_args| Ok(json!({ "ok": true })),
        )
        .core();
    let v = call(&core, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let schema = &v["result"]["tools"][0]["inputSchema"];
    // Le schéma publié est celui de schemars : propriétés du struct,
    // `query` requis, doc-comment de champ propagé.
    assert_eq!(schema["properties"]["query"]["type"], "string", "{v}");
    assert!(
        schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("query")),
        "{v}"
    );
    // Appel valide : le handler s'exécute.
    let v = call_tool(
        &core,
        2,
        json!({ "query": "rust mcp", "limit": 10, "tags": ["a"] }),
    );
    assert_eq!(v["result"]["structuredContent"]["ok"], true, "{v}");
}

#[test]
fn schemars_validation_rejects_invalid_arguments_with_issues() {
    let core = ServerBuilder::new("m", "0.1.0")
        .tool_typed(
            "search",
            "Recherche",
            from_schemars::<SearchArgs>(),
            |_args| Ok(json!({ "ok": true })),
        )
        .core();

    // Requis manquant et type faux : -32602, handler jamais exécuté, une
    // issue par problème, chemin lisible.
    let v = call_tool(
        &core,
        1,
        json!({ "limit": "beaucoup", "tags": "pas-un-tableau" }),
    );
    assert_eq!(v["error"]["code"], -32602, "{v}");
    let issues = v["error"]["data"]["issues"].as_array().unwrap();
    assert!(
        issues.len() >= 2,
        "au moins query manquant + types faux : {v}"
    );
    assert!(
        issues.iter().any(|i| i.as_str().unwrap().contains("query")),
        "le chemin doit citer query : {v}"
    );
    // Hors bornes (min 1) : refusé aussi.
    let v = call_tool(&core, 2, json!({ "query": "ok", "limit": 0 }));
    assert_eq!(v["error"]["code"], -32602, "{v}");
}

#[test]
fn schemars_adapter_works_for_output_schema_and_prompts() {
    #[derive(JsonSchema)]
    #[allow(dead_code)]
    struct Compte {
        total: i64,
    }
    let core = ServerBuilder::new("m", "0.1.0")
        .tool("compte", "compte", |_args| Ok(json!({ "total": 2 })))
        .output_schema("compte", from_schemars::<Compte>())
        .prompt_typed("salut", "salue", from_schemars::<Compte>(), |_args| {
            Ok(vec![litemcp::protocol::PromptMessage::user("salut")])
        })
        .core();

    // outputSchema : la sortie conforme passe, la non conforme devient
    // isError (schéma exécuteur, ici aussi).
    let v = call(
        &core,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"compte","arguments":{}}}"#,
    );
    assert_eq!(v["result"]["structuredContent"]["total"], 2, "{v}");

    // prompt_typed : arguments validés par le schéma dérivé.
    let v = call(
        &core,
        r#"{"jsonrpc":"2.0","id":2,"method":"prompts/get","params":{"name":"salut","arguments":{"total":3}}}"#,
    );
    assert!(
        v["result"]["messages"][0]["content"]["text"].is_string(),
        "{v}"
    );
    let v = call(
        &core,
        r#"{"jsonrpc":"2.0","id":3,"method":"prompts/get","params":{"name":"salut","arguments":{"total":"x"}}}"#,
    );
    assert_eq!(v["error"]["code"], -32602, "{v}");
}
