//! Tests de l'adaptateur schemars (feature `schemars`) : un type
//! `#[derive(JsonSchema)]` devient un schéma exécuteur — publié ET validé.

#![cfg(feature = "schemars")]

use litemcp::ServerBuilder;
use litemcp::schemars::{from_json_schema, from_schemars};
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
    let reply = core.dispatch_unchecked(body.as_bytes()).unwrap();
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
    // Bornes publiées par `#[schemars(range(..))]` conservées telles quelles :
    // l'adaptateur ne complète que les bornes **absentes** (sinon `limit`
    // porterait `[0, 255]` au lieu de `[1, 100]`).
    assert_eq!(schema["properties"]["limit"]["minimum"], json!(1), "{v}");
    assert_eq!(schema["properties"]["limit"]["maximum"], json!(100), "{v}");
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

// ===========================================================================
// M2 — `u64` : bornes publiées, refus hors plage, entier exact au handler
// ===========================================================================

#[derive(JsonSchema)]
#[allow(dead_code)] // dérivation uniquement
struct U64Args {
    v: u64,
}

fn call_named(core: &litemcp::ServerCore, id: i64, name: &str, arguments: Value) -> Value {
    call(
        core,
        &format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{arguments}}}}}"#
        ),
    )
}

#[test]
fn u64_du_schema_schemars_est_borne_et_livre_exact() {
    let core = ServerBuilder::new("m", "0.1.0")
        .tool_typed("u", "u64", from_schemars::<U64Args>(), |args| {
            Ok(args.clone())
        })
        .core();

    // Le schéma PUBLIÉ porte les bornes de `u64` : `minimum: 0` vient de
    // schemars, le `maximum` de l'adaptateur (schemars ne publie pas
    // u64::MAX).
    let v = call(&core, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let schema = &v["result"]["tools"][0]["inputSchema"];
    assert_eq!(schema["properties"]["v"]["minimum"], json!(0), "{v}");
    assert_eq!(
        schema["properties"]["v"]["maximum"],
        json!(u64::MAX),
        "le schéma publié doit porter la borne u64::MAX : {v}"
    );

    // u64::MAX : accepté, livré EXACT.
    let v = call_named(&core, 2, "u", json!({ "v": u64::MAX }));
    assert_eq!(
        v["result"]["structuredContent"]["v"],
        json!(u64::MAX),
        "{v}"
    );

    // 1e19 : intégral dans [0, u64::MAX] → normalisé en u64 exact. L'audit
    // recevait `1e+19` (f64) au handler.
    let v = call_named(&core, 3, "u", json!({ "v": 1e19 }));
    assert_eq!(
        v["result"]["structuredContent"]["v"],
        json!(10_000_000_000_000_000_000u64),
        "1e19 doit arriver en u64 exact : {v}"
    );

    // 2^64 : hors plage → -32602 (le schéma publié porte la borne).
    let two_pow_64: Value = serde_json::from_str("18446744073709551616").unwrap();
    let v = call_named(&core, 4, "u", json!({ "v": two_pow_64 }));
    assert_eq!(v["error"]["code"], -32602, "2^64 doit être refusé : {v}");

    // Négatif et non entier : -32602.
    let v = call_named(&core, 5, "u", json!({ "v": -1 }));
    assert_eq!(v["error"]["code"], -32602, "{v}");
    let v = call_named(&core, 6, "u", json!({ "v": 1.5 }));
    assert_eq!(v["error"]["code"], -32602, "{v}");
}

// ===========================================================================
// D1 — bornes d'entiers : tous les formats publiés par schemars, pas `u64` seul
// ===========================================================================

/// Un champ par type entier : le schéma publié doit porter les bornes exactes
/// du type Rust, et une valeur hors plage doit être refusée **par le schéma**
/// (`-32602`), jamais atteindre la désérialisation.
#[derive(serde::Deserialize, serde::Serialize, JsonSchema)]
#[allow(dead_code)] // dérivation et relecture serde uniquement
struct IntArgs {
    u8v: u8,
    u16v: u16,
    u32v: u32,
    u64v: u64,
    usizev: usize,
    i8v: i8,
    i16v: i16,
    i32v: i32,
    i64v: i64,
    isizev: isize,
}

fn int_args_min() -> Value {
    json!({
        "u8v": 0u8, "u16v": 0u16, "u32v": 0u32, "u64v": 0u64, "usizev": 0u64,
        "i8v": i8::MIN, "i16v": i16::MIN, "i32v": i32::MIN,
        "i64v": i64::MIN, "isizev": isize::MIN,
    })
}

fn int_args_max() -> Value {
    json!({
        "u8v": u8::MAX, "u16v": u16::MAX, "u32v": u32::MAX, "u64v": u64::MAX,
        "usizev": usize::MAX,
        "i8v": i8::MAX, "i16v": i16::MAX, "i32v": i32::MAX,
        "i64v": i64::MAX, "isizev": isize::MAX,
    })
}

#[test]
fn bornes_integeres_schemars_publiees_et_refusees_hors_plage() {
    let core = ServerBuilder::new("m", "0.1.0")
        .tool_typed("ints", "entiers", from_schemars::<IntArgs>(), |args| {
            Ok(args.clone())
        })
        .core();

    // 1. Bornes PUBLIÉES, exactes, pour chaque type (schemars omet celles de
    //    `u32`/`u64`/`usize` et de `i32`/`i64`/`isize` — sonde 1.2.3).
    let list = call(&core, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let props = &list["result"]["tools"][0]["inputSchema"]["properties"];
    for (field, min, max) in [
        ("u8v", json!(0), json!(u8::MAX)),
        ("u16v", json!(0), json!(u16::MAX)),
        ("u32v", json!(0), json!(u32::MAX)),
        ("u64v", json!(0), json!(u64::MAX)),
        ("usizev", json!(0), json!(usize::MAX)),
        ("i8v", json!(i8::MIN), json!(i8::MAX)),
        ("i16v", json!(i16::MIN), json!(i16::MAX)),
        ("i32v", json!(i32::MIN), json!(i32::MAX)),
        ("i64v", json!(i64::MIN), json!(i64::MAX)),
        ("isizev", json!(isize::MIN), json!(isize::MAX)),
    ] {
        assert_eq!(
            props[field]["minimum"], min,
            "borne basse de {field}: {list}"
        );
        assert_eq!(
            props[field]["maximum"], max,
            "borne haute de {field}: {list}"
        );
    }

    // 2. Valeurs LIMITES : acceptées, relues exactement par serde et livrées
    //    telles quelles au handler.
    for args in [int_args_min(), int_args_max()] {
        let parsed: IntArgs =
            serde_json::from_value(args.clone()).expect("une valeur limite est désérialisable");
        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            args,
            "aller-retour serde exact sur les limites"
        );
        let v = call_named(&core, 2, "ints", args.clone());
        assert_eq!(
            v["result"]["structuredContent"], args,
            "limites livrées exactes : {v}"
        );
    }

    // 3. Valeurs HORS PLAGE Rust : `-32602` par le schéma publié — ni panique,
    //    ni valeur acceptée puis refusée à la désérialisation.
    let above_u64: Value = serde_json::from_str("18446744073709551616").unwrap();
    let above_usize: Value = serde_json::from_str(&(usize::MAX as u128 + 1).to_string()).unwrap();
    let above_i64: Value = serde_json::from_str("9223372036854775808").unwrap();
    for (field, value) in [
        ("u8v", json!(256)),
        ("u16v", json!(65536)),
        ("u32v", json!(u64::from(u32::MAX) + 1)),
        ("u64v", above_u64.clone()),
        ("usizev", above_usize),
        ("i8v", json!(-129)),
        ("i16v", json!(-32769)),
        ("i32v", json!(i64::from(i32::MIN) - 1)),
        ("i64v", above_i64.clone()),
        ("isizev", above_i64.clone()),
    ] {
        let mut args = int_args_min();
        args[field] = value;
        let v = call_named(&core, 3, "ints", args);
        assert_eq!(
            v["error"]["code"], -32602,
            "{field} hors plage doit être refusé par le schéma : {v}"
        );
    }
}

// ===========================================================================
// M4 — schéma brut invalide : refusé à la CONSTRUCTION
// ===========================================================================

#[test]
#[should_panic(expected = "JSON Schema")]
fn from_json_schema_refuse_un_document_invalide() {
    let _ = from_json_schema(json!({ "type": "pas-un-type" }));
}

#[test]
#[should_panic(expected = "JSON Schema")]
fn from_json_schema_refuse_un_ref_local_absent() {
    // Un `$ref` local non résoluble rendait CHAQUE appel fautif (-32602 et
    // « invalid internal schema ») : c'est une erreur de programmation,
    // refusée dès la construction.
    let _ = from_json_schema(json!({
        "type": "object",
        "properties": { "x": { "$ref": "#/$defs/Absent" } }
    }));
}

#[test]
fn from_json_schema_accepte_recursivite_et_refs_locaux() {
    // Contre-preuve : une récursivité légitime et un `$ref` local valide
    // restent acceptés (le refus ne vise que les documents incompilables).
    let _ = from_json_schema(json!({
        "type": "object",
        "properties": { "node": { "$ref": "#/$defs/Node" } },
        "$defs": {
            "Node": {
                "type": "object",
                "properties": {
                    "value": { "type": "integer", "default": 5 },
                    "next": { "$ref": "#/$defs/Node" }
                }
            }
        }
    }));
}

// ===========================================================================
// M5 (complément) — erreur de schéma INTERNE : -32603 neutre, jamais -32602
// ===========================================================================

/// Type dont le schéma DÉRIVÉ est mal formé : la dérivation `schemars`
/// n'est pas vérifiée par l'adaptateur, le validateur compilé est donc
/// cassé — c'est le cas « erreur de schéma interne ».
struct BrokenDerive;

impl JsonSchema for BrokenDerive {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "BrokenDerive".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "object",
            "properties": { "x": { "type": "pas-un-type" } }
        })
    }
}

#[test]
fn erreur_de_schema_interne_sort_en_32603_neutre() {
    let core = ServerBuilder::new("m", "0.1.0")
        .tool_typed("b", "b", from_schemars::<BrokenDerive>(), |_args| {
            Ok(json!({}))
        })
        .core();
    let v = call_named(&core, 1, "b", json!({ "x": 1 }));
    assert_eq!(v["error"]["code"], -32603, "{v}");
    assert_eq!(
        v["error"]["message"],
        json!("internal error"),
        "message neutre attendu : {v}"
    );
    let wire = v.to_string();
    for leak in [
        "jsonschema",
        "invalid internal schema",
        "http",
        "feature",
        "$defs",
    ] {
        assert!(!wire.contains(leak), "fuite interne `{leak}` dans {wire}");
    }
}
