//! Tests de la macro `#[tool]` (feature `macros`) — schéma dérivé des types
//! des paramètres, enregistrement via `.with_tool`, exécution validée.

#![cfg(feature = "macros")]

use litemcp::ServerBuilder;
use litemcp::macros::tool;
use serde_json::{Value, json};

#[tool(name = "add", description = "Additionne deux entiers")]
fn add(a: i64, b: i64) -> litemcp::ToolResult {
    Ok(json!({ "somme": a + b }))
}

#[tool(description = "Recherche avec options")]
fn recherche(query: String, limit: Option<u64>, tags: Vec<String>) -> litemcp::ToolResult {
    Ok(json!({
        "query": query,
        "limit": limit.unwrap_or(10),
        "tags": tags,
    }))
}

#[tool(description = "Température ambiante")]
fn capteur(celsius: f64, actif: bool) -> litemcp::ToolResult {
    Ok(json!({ "celsius": celsius, "actif": actif }))
}

fn call(core: &litemcp::ServerCore, body: &str) -> Value {
    let reply = core.dispatch(body.as_bytes()).unwrap();
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

#[test]
fn macro_derives_the_input_schema_from_parameter_types() {
    let core = ServerBuilder::new("m", "0.1.0")
        .with_tool(register_add)
        .with_tool(register_recherche)
        .with_tool(register_capteur)
        .core();

    let v = call(&core, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let tools = v["result"]["tools"].as_array().unwrap();

    let add = tools.iter().find(|t| t["name"] == "add").unwrap();
    assert_eq!(add["description"], "Additionne deux entiers");
    let schema = &add["inputSchema"];
    assert_eq!(
        schema["required"],
        json!(["a", "b"]),
        "deux paramètres requis : {schema}"
    );
    assert_eq!(schema["properties"]["a"]["type"], "integer");

    // `name` omis sur `recherche` : le nom par défaut est celui de la
    // fonction — l'outil est trouvé sous ce nom.
    let recherche = tools.iter().find(|t| t["name"] == "recherche").unwrap();
    let schema = &recherche["inputSchema"];
    assert_eq!(
        schema["required"],
        json!(["query", "tags"]),
        "Option<T> est optionnel, Vec<T> requis : {schema}"
    );
    assert_eq!(schema["properties"]["query"]["type"], "string");
    assert_eq!(schema["properties"]["limit"]["type"], "integer");
    assert_eq!(schema["properties"]["tags"]["type"], "array");
    assert_eq!(schema["properties"]["tags"]["items"]["type"], "string");

    let capteur = tools.iter().find(|t| t["name"] == "capteur").unwrap();
    let schema = &capteur["inputSchema"];
    assert_eq!(schema["properties"]["celsius"]["type"], "number");
    assert_eq!(schema["properties"]["actif"]["type"], "boolean");
}

#[test]
fn macro_tool_executes_with_validated_arguments() {
    let core = ServerBuilder::new("m", "0.1.0")
        .with_tool(register_add)
        .with_tool(register_recherche)
        .with_tool(register_capteur)
        .core();

    // Entiers.
    let v = call_tool(&core, 1, "add", json!({ "a": 2, "b": 3 }));
    assert_eq!(v["result"]["structuredContent"]["somme"], 5, "{v}");

    // Optionnel omis → None (défaut du handler) ; Vec fourni.
    let v = call_tool(
        &core,
        2,
        "recherche",
        json!({ "query": "rust mcp", "tags": [] }),
    );
    assert_eq!(v["result"]["structuredContent"]["limit"], 10, "{v}");
    assert_eq!(v["result"]["structuredContent"]["tags"], json!([]), "{v}");

    // Optionnel fourni ; Vec d'éléments.
    let v = call_tool(
        &core,
        3,
        "recherche",
        json!({ "query": "rust mcp", "limit": 5, "tags": ["a", "b"] }),
    );
    assert_eq!(v["result"]["structuredContent"]["limit"], 5, "{v}");
    assert_eq!(
        v["result"]["structuredContent"]["tags"],
        json!(["a", "b"]),
        "{v}"
    );

    // Flottant et booléen.
    let v = call_tool(
        &core,
        4,
        "capteur",
        json!({ "celsius": 21.5, "actif": true }),
    );
    assert_eq!(v["result"]["structuredContent"]["celsius"], 21.5, "{v}");
    assert_eq!(v["result"]["structuredContent"]["actif"], true, "{v}");
}

#[test]
fn macro_tool_never_runs_the_handler_on_schema_violation() {
    let core = ServerBuilder::new("m", "0.1.0")
        .with_tool(register_add)
        .with_tool(register_recherche)
        .core();

    // Type faux.
    let v = call_tool(&core, 1, "add", json!({ "a": "x", "b": 1 }));
    assert_eq!(v["error"]["code"], -32602, "{v}");
    // Requis manquant.
    let v = call_tool(&core, 2, "recherche", json!({ "query": "ok" }));
    assert_eq!(v["error"]["code"], -32602, "{v}");
    // Élément de Vec du mauvais type.
    let v = call_tool(&core, 3, "recherche", json!({ "query": "ok", "tags": [1] }));
    assert_eq!(v["error"]["code"], -32602, "{v}");
}
