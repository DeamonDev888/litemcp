//! Tests de régression — audit schémas/macros 2026-11-13 (constat m6 → A7) :
//! un défaut incohérent avec son schéma est une **erreur de programmation**,
//! refusée à la construction (`Defaulted::new` panique au démarrage, même
//! politique que `assert_ordered` et la garde de finitude des bornes) au
//! lieu d'être publié puis de rendre `-32602` à chaque appel vide.
//!
//! Contre-épreuves : un défaut VALIDE construit, est publié, et est injecté
//! (y compris imbriqué récursivement) ; `nullable(s).default(null)` reste
//! licite — le contrat « `null` = absence » ne change pas, c'est le défaut
//! lui-même qui doit être une valeur valide du schéma interne.
//!
//! Le constat m1 (A4 — `serde_json` exigé chez l'appelant) est prouvé par la
//! crate-sonde externe de l'audit du 2026-11-13 (hors dépôt).

use litemcp::prelude::*;
use serde_json::{Value, json};

fn call_tool(core: &litemcp::ServerCore, name: &str, arguments: Value) -> Value {
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{name}","arguments":{arguments}}}}}"#
    );
    let reply = core.dispatch_unchecked(body.as_bytes()).unwrap();
    serde_json::from_slice(&reply).unwrap()
}

fn list_tools(core: &litemcp::ServerCore) -> Value {
    let reply = core
        .dispatch_unchecked(br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
        .unwrap();
    serde_json::from_slice(&reply).unwrap()
}

// --- A7 : défaut invalide → panique à la construction ---

/// `string().default(null)` : `null` n'est pas une chaîne. Le contrat
/// « `null` = absence » couvre la **valeur du champ**, pas le défaut
/// lui-même : le défaut doit être une valeur valide du schéma interne.
#[test]
#[should_panic(expected = "valeur par défaut invalide")]
fn default_null_sur_string_panique_a_la_construction() {
    let _ = zod::string().default(json!(null));
}

/// `integer().max(5).default(9)` : le défaut viole la borne `max` du schéma
/// — refusé au démarrage (erreur de programmation), pas à chaque appel.
#[test]
#[should_panic(expected = "valeur par défaut invalide")]
fn default_hors_borne_panique_a_la_construction() {
    let _ = zod::integer().max(5).default(9);
}

/// `number().default(f64::NAN)` : NaN/±∞ ne sont pas représentables en JSON
/// — serde_json les dégrade en `null`, qui n'est pas un nombre → refusé.
#[test]
#[should_panic(expected = "valeur par défaut invalide")]
fn default_nan_panique_a_la_construction() {
    let _ = zod::number().default(f64::NAN);
}

// --- contre-épreuves : défauts valides ---

/// Défauts valides : construits, **publiés** (bornes conservées) et
/// **injectés** dans les arguments du handler.
#[test]
fn default_valide_construit_publie_et_injecte() {
    let schema = zod::object()
        .field("query", zod::string())
        .opt("limit", zod::integer().max(5).default(3))
        .opt("mode", zod::string().default("x"));

    // Publié : `default` dans l'inputSchema, `maximum` conservé.
    let doc = schema.json();
    assert_eq!(doc["properties"]["limit"]["default"], json!(3), "{doc}");
    assert_eq!(doc["properties"]["limit"]["maximum"], json!(5), "{doc}");
    assert_eq!(doc["properties"]["mode"]["default"], json!("x"), "{doc}");

    // Injecté : appel vide → défauts au handler.
    let core = ServerBuilder::new("t", "0.1.0")
        .tool_typed("t", "t", schema, |args| Ok(args.clone()))
        .core();
    let v = call_tool(&core, "t", json!({ "query": "q" }));
    assert!(v.get("error").is_none(), "{v}");
    assert_eq!(v["result"]["structuredContent"]["limit"], json!(3), "{v}");
    assert_eq!(v["result"]["structuredContent"]["mode"], json!("x"), "{v}");
}

/// Cohérence avec `nullable()` : le défaut doit être une valeur valide du
/// schéma **interne** — `nullable(string())` accepte `null`, donc
/// `nullable(string()).default(null)` est licite, publié et injecté.
#[test]
fn nullable_default_null_est_licite_publie_et_injecte() {
    let core = ServerBuilder::new("t", "0.1.0")
        .tool_typed(
            "t",
            "t",
            zod::object().opt("note", zod::nullable(zod::string()).default(json!(null))),
            |args| Ok(args.clone()),
        )
        .core();

    // Publié : `default: null` à côté du type nullable.
    let list = list_tools(&core);
    let note = &list["result"]["tools"][0]["inputSchema"]["properties"]["note"];
    assert_eq!(note["default"], json!(null), "{list}");
    assert!(note["anyOf"].is_array(), "{list}");

    // Injecté : `{}` et `{"note": null}` donnent `note: null`.
    let v = call_tool(&core, "t", json!({}));
    assert!(v.get("error").is_none(), "{v}");
    assert_eq!(v["result"]["structuredContent"]["note"], json!(null), "{v}");
    let v2 = call_tool(&core, "t", json!({ "note": null }));
    assert!(v2.get("error").is_none(), "{v2}");
    assert_eq!(
        v2["result"]["structuredContent"]["note"],
        json!(null),
        "{v2}"
    );
}

/// Un objet imbriqué à défaut reste injecté **récursivement** : le défaut
/// de l'objet (`{"host": "h"}`) est une valeur valide du schéma interne
/// (le champ interne à défaut couvre l'absence de `port`), et le défaut
/// interne est rempli dans la valeur injectée.
#[test]
fn objet_imbrique_a_defaut_injecte_recursivement() {
    let inner = zod::object()
        .field("host", zod::string())
        .opt("port", zod::integer().default(8080));
    let core = ServerBuilder::new("t", "0.1.0")
        .tool_typed(
            "t",
            "t",
            zod::object().opt("config", inner.default(json!({ "host": "h" }))),
            |args| Ok(args.clone()),
        )
        .core();

    let v = call_tool(&core, "t", json!({}));
    assert!(v.get("error").is_none(), "{v}");
    assert_eq!(
        v["result"]["structuredContent"]["config"]["host"],
        json!("h"),
        "{v}"
    );
    assert_eq!(
        v["result"]["structuredContent"]["config"]["port"],
        json!(8080),
        "le défaut interne doit être rempli récursivement : {v}"
    );
}
