//! Tests des trouvailles du DSL Zod (2026-10-24) :
//! `default()`, `record()`, énumération d'entiers, JSON Schema brut.

use litemcp::prelude::*;
use serde_json::json;

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

// --- 1. `default()` : publié ET appliqué ---

#[test]
fn default_is_published_and_applied() {
    // Le défaut est publié dans le JSON Schema (mot-clé `default`).
    let core = ServerBuilder::new("t", "0.1.0")
        .tool_typed(
            "search",
            "recherche",
            zod::object()
                .field("query", zod::string())
                .opt("limit", zod::integer().default(10)),
            |args| Ok(args.clone()),
        )
        .core();

    let list = call(&core, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let published = &list["result"]["tools"][0]["inputSchema"]["properties"]["limit"];
    assert_eq!(published["default"], json!(10), "{list}");

    // Un champ **avec** défaut n'est plus `required` : son absence est
    // couverte par le défaut.
    let required = list["result"]["tools"][0]["inputSchema"]["required"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        !required.contains(&json!("limit")),
        "un champ avec default ne doit pas être required : {list}"
    );
    assert!(required.contains(&json!("query")), "{list}");

    // Le défaut est **injecté** : le handler reçoit l'argument complété.
    let v = call_tool(&core, "search", json!({ "query": "x" }));
    assert!(v.get("error").is_none(), "{v}");
    assert_eq!(
        v["result"]["structuredContent"]["limit"],
        json!(10),
        "le défaut doit être injecté dans les arguments : {v}"
    );

    // Une valeur explicite **écrase** le défaut.
    let v2 = call_tool(&core, "search", json!({ "query": "x", "limit": 42 }));
    assert_eq!(
        v2["result"]["structuredContent"]["limit"],
        json!(42),
        "{v2}"
    );
}

#[test]
fn default_on_a_required_field_makes_it_optional() {
    // `.field()` + `.default()` : le champ reste obligatoire *pour le
    // handler* (il est toujours injecté), mais son absence n'est pas une
    // erreur de validation.
    let core = ServerBuilder::new("t", "0.1.0")
        .tool_typed(
            "t",
            "t",
            zod::object().field("mode", zod::string().default("fast")),
            |args| Ok(args.clone()),
        )
        .core();

    let v = call_tool(&core, "t", json!({}));
    assert!(
        v.get("error").is_none(),
        "l'absence ne doit pas échouer : {v}"
    );
    assert_eq!(
        v["result"]["structuredContent"]["mode"],
        json!("fast"),
        "{v}"
    );
}

#[test]
fn default_is_validated_against_its_own_schema() {
    // Un défaut incohérent avec le schéma (ici : une chaîne trop courte
    // pour la contrainte `min`) est une erreur de programmation : il est
    // refusé à la validation, comme Zod.
    let core = ServerBuilder::new("t", "0.1.0")
        .tool_typed(
            "t",
            "t",
            zod::object().opt("s", zod::string().min(5).default("ab")),
            |args| Ok(args.clone()),
        )
        .core();

    let v = call_tool(&core, "t", json!({}));
    assert!(
        v.get("error").is_some(),
        "le défaut incohérent doit être refusé : {v}"
    );
}

#[test]
fn defaults_work_on_nested_objects() {
    // Un objet imbriqué porte ses propres défauts.
    let core = ServerBuilder::new("t", "0.1.0")
        .tool_typed(
            "t",
            "t",
            zod::object().field(
                "config",
                zod::object()
                    .field("host", zod::string())
                    .opt("port", zod::integer().default(8080)),
            ),
            |args| Ok(args.clone()),
        )
        .core();

    let v = call_tool(&core, "t", json!({ "config": { "host": "h" } }));
    assert!(v.get("error").is_none(), "{v}");
    assert_eq!(
        v["result"]["structuredContent"]["config"]["port"],
        json!(8080),
        "le défaut imbriqué doit être injecté : {v}"
    );
}

#[test]
fn default_on_every_scalar_kind() {
    let core = ServerBuilder::new("t", "0.1.0")
        .tool_typed(
            "t",
            "t",
            zod::object()
                .opt("s", zod::string().default("x"))
                .opt("i", zod::integer().default(1))
                .opt("n", zod::number().default(1.5))
                .opt("b", zod::boolean().default(true))
                .opt("e", zod::enumeration(&["a", "b"]).default("a"))
                .opt("arr", zod::array(zod::integer()).default(json!([1, 2]))),
            |args| Ok(args.clone()),
        )
        .core();

    let v = call_tool(&core, "t", json!({}));
    assert!(v.get("error").is_none(), "{v}");
    let sc = &v["result"]["structuredContent"];
    assert_eq!(sc["s"], json!("x"), "{v}");
    assert_eq!(sc["i"], json!(1), "{v}");
    assert_eq!(sc["n"], json!(1.5), "{v}");
    assert_eq!(sc["b"], json!(true), "{v}");
    assert_eq!(sc["e"], json!("a"), "{v}");
    assert_eq!(sc["arr"], json!([1, 2]), "{v}");
}

// --- 2. `record()` : objet à valeurs typées ---

#[test]
fn record_validates_every_value() {
    let core = ServerBuilder::new("t", "0.1.0")
        .tool_typed(
            "meta",
            "métadonnées",
            zod::object().field("labels", zod::record(zod::string())),
            |args| Ok(args.clone()),
        )
        .core();

    // Publié : `additionalProperties` porte le schéma des valeurs.
    let list = call(&core, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let published = &list["result"]["tools"][0]["inputSchema"]["properties"]["labels"];
    assert_eq!(published["type"], json!("object"), "{list}");
    assert_eq!(
        published["additionalProperties"]["type"],
        json!("string"),
        "{list}"
    );

    // Des valeurs conformes passent.
    let v = call_tool(&core, "meta", json!({ "labels": { "a": "x", "b": "y" } }));
    assert!(v.get("error").is_none(), "{v}");

    // Une valeur non conforme est refusée, avec le chemin de la clé.
    let bad = call_tool(&core, "meta", json!({ "labels": { "a": 1 } }));
    assert!(bad.get("error").is_some(), "{bad}");
    let issues = bad["error"]["data"]["issues"].as_array().cloned().unwrap();
    let rendered = issues
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        rendered.contains("arguments.labels.a"),
        "le chemin doit pointer la clé fautive : {rendered}"
    );
}

#[test]
fn record_rejects_non_object() {
    let core = ServerBuilder::new("t", "0.1.0")
        .tool_typed(
            "meta",
            "m",
            zod::object().field("labels", zod::record(zod::string())),
            |args| Ok(args.clone()),
        )
        .core();
    let bad = call_tool(&core, "meta", json!({ "labels": [1, 2] }));
    assert!(bad.get("error").is_some(), "{bad}");
}

// --- 3. Énumération d'entiers ---

#[test]
fn integer_enumeration_works() {
    let core = ServerBuilder::new("t", "0.1.0")
        .tool_typed(
            "t",
            "t",
            zod::object().field("mode", zod::integer_enumeration(&[1, 2, 3])),
            |args| Ok(args.clone()),
        )
        .core();

    // Publié : `{"type": "integer", "enum": [1, 2, 3]}`.
    let list = call(&core, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let published = &list["result"]["tools"][0]["inputSchema"]["properties"]["mode"];
    assert_eq!(published["type"], json!("integer"), "{list}");
    assert_eq!(published["enum"], json!([1, 2, 3]), "{list}");

    // Une valeur de l'énumération passe.
    let v = call_tool(&core, "t", json!({ "mode": 2 }));
    assert!(v.get("error").is_none(), "{v}");

    // Une valeur hors énumération est refusée.
    let bad = call_tool(&core, "t", json!({ "mode": 9 }));
    assert!(bad.get("error").is_some(), "{bad}");

    // Un type erroné aussi (et le message le dit).
    let bad2 = call_tool(&core, "t", json!({ "mode": "2" }));
    assert!(bad2.get("error").is_some(), "{bad2}");
    let rendered = bad2["error"]["data"]["issues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(rendered.contains("expected an integer"), "{rendered}");
}

#[test]
fn integer_enumeration_rejects_empty_and_duplicates() {
    // Panique de développement (schéma insatisfaisable / meta-schema).
    assert!(std::panic::catch_unwind(|| zod::integer_enumeration(&[])).is_err());
    assert!(std::panic::catch_unwind(|| zod::integer_enumeration(&[1, 1, 2])).is_err());
}

// --- 4. JSON Schema brut ---

#[cfg(feature = "schemars")]
#[test]
fn raw_json_schema_is_published_and_applied() {
    use litemcp::schemars::from_json_schema;

    // Un schéma exotique que le DSL n'exprime pas : `oneOf`.
    let document = json!({
        "type": "object",
        "properties": {
            "value": {
                "oneOf": [
                    { "type": "string" },
                    { "type": "integer" }
                ]
            }
        },
        "required": ["value"]
    });

    let core = ServerBuilder::new("t", "0.1.0")
        .tool_typed("t", "t", from_json_schema(document.clone()), |args| {
            Ok(args.clone())
        })
        .core();

    // Publié **tel quel**.
    let list = call(&core, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let published = &list["result"]["tools"][0]["inputSchema"];
    assert_eq!(
        published, &document,
        "le document doit être publié tel quel : {list}"
    );

    // Appliqué : les deux branches de `oneOf` passent.
    let s = call_tool(&core, "t", json!({ "value": "x" }));
    assert!(s.get("error").is_none(), "{s}");
    let i = call_tool(&core, "t", json!({ "value": 42 }));
    assert!(i.get("error").is_none(), "{i}");

    // Une valeur hors branches est refusée.
    let bad = call_tool(&core, "t", json!({ "value": true }));
    assert!(bad.get("error").is_some(), "{bad}");
}

#[cfg(feature = "schemars")]
#[test]
fn raw_json_schema_rejects_invalid_document() {
    use litemcp::schemars::from_json_schema;
    // Un document qui n'est ni un objet ni un booléen : erreur de
    // programmation → panique.
    assert!(std::panic::catch_unwind(|| from_json_schema(json!("pas un schéma"))).is_err());
}
