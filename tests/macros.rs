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

/// Valeurs par défaut déclarées au niveau de la macro :
/// `#[tool(default(limit = 10, mode = "fast"))]`.
#[tool(
    description = "Recherche avec limite par défaut",
    default(limit = 10, mode = "fast")
)]
fn recherche_defaut(query: String, limit: u64, mode: String) -> litemcp::ToolResult {
    Ok(json!({ "query": query, "limit": limit, "mode": mode }))
}

#[test]
fn macro_parameter_default_is_published_and_applied() {
    let core = ServerBuilder::new("m", "0.1.0")
        .with_tool(register_recherche_defaut)
        .core();

    // Le défaut est **publié** dans l'inputSchema.
    let list = call(&core, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let schema = &list["result"]["tools"][0]["inputSchema"];
    assert_eq!(
        schema["properties"]["limit"]["default"],
        json!(10),
        "le défaut doit être publié : {list}"
    );
    assert_eq!(
        schema["properties"]["mode"]["default"],
        json!("fast"),
        "le défaut doit être publié : {list}"
    );
    // Un champ avec défaut n'est pas `required`.
    let required = schema["required"].as_array().cloned().unwrap_or_default();
    assert!(!required.contains(&json!("limit")), "{list}");
    assert!(required.contains(&json!("query")), "{list}");

    // Le défaut est **injecté** : le handler reçoit la valeur.
    let v = call_tool(&core, 2, "recherche_defaut", json!({ "query": "x" }));
    assert!(v.get("error").is_none(), "{v}");
    assert_eq!(v["result"]["structuredContent"]["limit"], json!(10), "{v}");
    assert_eq!(
        v["result"]["structuredContent"]["mode"],
        json!("fast"),
        "{v}"
    );

    // Une valeur explicite écrase le défaut.
    let v2 = call_tool(
        &core,
        3,
        "recherche_defaut",
        json!({ "query": "x", "limit": 99, "mode": "slow" }),
    );
    assert_eq!(
        v2["result"]["structuredContent"]["limit"],
        json!(99),
        "{v2}"
    );
    assert_eq!(
        v2["result"]["structuredContent"]["mode"],
        json!("slow"),
        "{v2}"
    );
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

// --- Audit 2026-10-25 : littéraux négatifs, bornes d'entiers, défauts de prompt ---

#[tool(
    description = "Décalage avec défauts négatifs",
    default(offset = -5, temperature = -2.5)
)]
fn decalage(offset: i32, temperature: f64) -> litemcp::ToolResult {
    Ok(json!({ "offset": offset, "temperature": temperature }))
}

#[tool(description = "Bornes exactes par type entier")]
fn bornes_entiers(octet: u8, court: i16, taille: usize) -> litemcp::ToolResult {
    Ok(json!({ "octet": octet, "court": court, "taille": taille }))
}

#[prompt(
    description = "Résumé avec valeurs par défaut",
    default(ton = "concis", profondeur = -1)
)]
fn resume(sujet: String, ton: String, profondeur: i32) -> Result<Vec<PromptMessage>, String> {
    Ok(vec![PromptMessage::user(format!(
        "{sujet} ({ton}, {profondeur})"
    ))])
}

#[test]
fn macro_negative_default_literals_compile_and_apply() {
    let core = ServerBuilder::new("m", "0.1.0")
        .with_tool(register_decalage)
        .core();

    let list = call(&core, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let schema = &list["result"]["tools"][0]["inputSchema"];
    assert_eq!(schema["properties"]["offset"]["default"], json!(-5));
    assert_eq!(schema["properties"]["temperature"]["default"], json!(-2.5));

    let v = call_tool(&core, 2, "decalage", json!({}));
    assert_eq!(v["result"]["structuredContent"]["offset"], json!(-5));
    assert_eq!(v["result"]["structuredContent"]["temperature"], json!(-2.5));

    let v2 = call_tool(
        &core,
        3,
        "decalage",
        json!({ "offset": -42, "temperature": 1.25 }),
    );
    assert_eq!(v2["result"]["structuredContent"]["offset"], json!(-42));
    assert_eq!(
        v2["result"]["structuredContent"]["temperature"],
        json!(1.25)
    );
}

#[test]
fn macro_integer_parameter_bounds_prevent_silent_truncation_and_wrapping() {
    let core = ServerBuilder::new("m", "0.1.0")
        .with_tool(register_bornes_entiers)
        .core();

    let list = call(&core, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let props = &list["result"]["tools"][0]["inputSchema"]["properties"];
    assert_eq!(props["octet"]["minimum"], json!(0));
    assert_eq!(props["octet"]["maximum"], json!(255));
    assert_eq!(props["court"]["minimum"], json!(i16::MIN));
    assert_eq!(props["court"]["maximum"], json!(i16::MAX));
    assert_eq!(props["taille"]["minimum"], json!(0));

    // Valeurs hors bornes du type Rust cible (ex. -1 sur u8/usize, 300 sur u8,
    // 40000 sur i16) : refusées en -32602, jamais tronquées par `as #ty`.
    for bad in [
        json!({ "octet": -1, "court": 0, "taille": 1 }),
        json!({ "octet": 300, "court": 0, "taille": 1 }),
        json!({ "octet": 1, "court": 40_000, "taille": 1 }),
        json!({ "octet": 1, "court": 0, "taille": -1 }),
    ] {
        let v = call_tool(&core, 2, "bornes_entiers", bad.clone());
        assert_eq!(
            v["error"]["code"], -32602,
            "doit être refusé : {bad} -> {v}"
        );
    }

    // Valeurs dans les bornes : acceptées telles quelles.
    let ok = call_tool(
        &core,
        3,
        "bornes_entiers",
        json!({ "octet": 255, "court": -32_768, "taille": 1024 }),
    );
    assert_eq!(
        ok["result"]["structuredContent"],
        json!({ "octet": 255, "court": -32_768, "taille": 1024 })
    );
}

#[test]
fn macro_prompt_default_is_applied_and_arguments_are_published_in_list() {
    let core = ServerBuilder::new("m", "0.1.0")
        .with_tool(register_resume)
        .core();

    let list = call(&core, r#"{"jsonrpc":"2.0","id":1,"method":"prompts/list"}"#);
    let args = list["result"]["prompts"][0]["arguments"]
        .as_array()
        .expect("arguments publiés dans prompts/list");
    assert_eq!(args.len(), 3, "{list}");
    let sujet = args.iter().find(|a| a["name"] == "sujet").unwrap();
    let ton = args.iter().find(|a| a["name"] == "ton").unwrap();
    assert_eq!(sujet["required"], json!(true));
    assert_eq!(ton["required"], json!(false));

    let v = call(
        &core,
        r#"{"jsonrpc":"2.0","id":2,"method":"prompts/get","params":{"name":"resume","arguments":{"sujet":"MCP"}}}"#,
    );
    assert_eq!(
        v["result"]["messages"][0]["content"]["text"], "MCP (concis, -1)",
        "{v}"
    );
}
