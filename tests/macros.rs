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

// --- #[resource] et #[prompt] ---

use litemcp::macros::{prompt, resource};
use litemcp::protocol::{PromptMessage, ResourceContents};

#[resource(uri = "mem://doc", name = "Doc", description = "La documentation")]
fn doc() -> Result<ResourceContents, String> {
    Ok(ResourceContents {
        uri: "mem://doc".to_string(),
        mime_type: Some("text/markdown".to_string()),
        text: "# Généré par macro".to_string(),
    })
}

#[prompt(description = "Demande une revue de code")]
fn revue(fichier: String, severite: Option<String>) -> Result<Vec<PromptMessage>, String> {
    let severite = severite.unwrap_or_else(|| "normale".to_string());
    Ok(vec![PromptMessage::user(format!(
        "Relis {fichier} (severite {severite})"
    ))])
}

#[test]
fn macro_resource_registers_list_and_read() {
    let core = ServerBuilder::new("m", "0.1.0")
        .with_tool(register_doc)
        .core();

    // Liste : l'entrée porte uri/name/description tels que déclarés.
    let v = call(
        &core,
        r#"{"jsonrpc":"2.0","id":1,"method":"resources/list"}"#,
    );
    let entry = &v["result"]["resources"][0];
    assert_eq!(entry["uri"], "mem://doc", "{v}");
    assert_eq!(entry["name"], "Doc", "{v}");
    assert_eq!(entry["description"], "La documentation", "{v}");

    // Lecture : le contenu vient de la fonction annotée.
    let v = call(
        &core,
        r#"{"jsonrpc":"2.0","id":2,"method":"resources/read","params":{"uri":"mem://doc"}}"#,
    );
    assert_eq!(
        v["result"]["contents"][0]["text"], "# Généré par macro",
        "{v}"
    );
    assert_eq!(
        v["result"]["contents"][0]["mimeType"], "text/markdown",
        "{v}"
    );

    // La capacité resources est annoncée dès qu'une ressource existe.
    let v = call(
        &core,
        r#"{"jsonrpc":"2.0","id":3,"method":"server/discover"}"#,
    );
    assert!(v["result"]["capabilities"]["resources"].is_object(), "{v}");
}

#[test]
fn macro_prompt_validates_and_renders() {
    let core = ServerBuilder::new("m", "0.1.0")
        .with_tool(register_revue)
        .core();

    // `name` omis : défaut = nom de la fonction.
    let v = call(&core, r#"{"jsonrpc":"2.0","id":1,"method":"prompts/list"}"#);
    assert_eq!(v["result"]["prompts"][0]["name"], "revue", "{v}");
    assert_eq!(
        v["result"]["prompts"][0]["description"], "Demande une revue de code",
        "{v}"
    );

    // Requis fourni, optionnel omis.
    let v = call(
        &core,
        r#"{"jsonrpc":"2.0","id":2,"method":"prompts/get","params":{"name":"revue","arguments":{"fichier":"src/main.rs"}}}"#,
    );
    assert_eq!(
        v["result"]["messages"][0]["content"]["text"], "Relis src/main.rs (severite normale)",
        "{v}"
    );

    // Optionnel fourni.
    let v = call(
        &core,
        r#"{"jsonrpc":"2.0","id":3,"method":"prompts/get","params":{"name":"revue","arguments":{"fichier":"lib.rs","severite":"stricte"}}}"#,
    );
    assert_eq!(
        v["result"]["messages"][0]["content"]["text"], "Relis lib.rs (severite stricte)",
        "{v}"
    );

    // Hors schéma (requis manquant, type faux) : -32602, handler jamais exécuté.
    for arguments in [r#"{}"#, r#"{"fichier":42}"#] {
        let v = call(
            &core,
            &format!(
                r#"{{"jsonrpc":"2.0","id":4,"method":"prompts/get","params":{{"name":"revue","arguments":{arguments}}}}}"#
            ),
        );
        assert_eq!(v["error"]["code"], -32602, "arguments={arguments} : {v}");
        assert!(
            v["error"]["data"]["issues"].as_array().is_some(),
            "issues attendues : {v}"
        );
    }
}
