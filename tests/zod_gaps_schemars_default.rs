//! Test de régression — audit 2026-10-27 (M6) : le `default` publié par
//! `schemars` doit être INJECTÉ dans les arguments avant l'exécution.
//!
//! Bug : `FromSchemars` implémentait `json()` + `validate()` mais laissait
//! `default_value()`/`apply_defaults()`/`has_defaults()` aux no-ops du trait
//! `Schema`. `schemars` publie `"default": 7` pour un champ
//! `#[serde(default)]`, le champ sort de `required`, l'absence est acceptée…
//! et le handler recevait un objet SANS le champ : le contrat publié était
//! faux (règle du projet « publié = appliqué »).

#![cfg(feature = "schemars")]

use litemcp::prelude::*;
use litemcp::schemars::from_schemars;
use litemcp::zod::Schema;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Deserialize, JsonSchema)]
#[allow(dead_code)] // lu par serde uniquement
struct Args {
    name: String,
    #[serde(default = "def_count")]
    count: u32,
}

fn def_count() -> u32 {
    7
}

#[derive(Deserialize, JsonSchema, Default)]
#[allow(dead_code)] // lu par serde uniquement
struct Nested {
    #[serde(default = "def_depth")]
    depth: u8,
}

fn def_depth() -> u8 {
    3
}

#[derive(Deserialize, JsonSchema)]
#[allow(dead_code)] // lu par serde uniquement
struct Outer {
    #[serde(default)]
    inner: Nested,
}

#[test]
fn schemars_default_est_publie_et_injecte() {
    let schema = from_schemars::<Args>();

    // Moitié PUBLIÉE : le mot-clé `default` est dans l'inputSchema.
    let doc = schema.json();
    assert_eq!(
        doc["properties"]["count"]["default"],
        json!(7),
        "le document publié doit porter le default"
    );
    let required: Vec<&str> = doc["required"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    assert!(
        !required.contains(&"count"),
        "un champ avec default ne doit pas être required"
    );

    // Moitié APPLIQUÉE : l'injection part du dispatch complet.
    let seen: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let seen2 = Arc::clone(&seen);
    let core = ServerBuilder::new("reg", "1.0")
        .tool_typed("t", "t", from_schemars::<Args>(), move |args| {
            *seen2.lock().unwrap() = Some(args.clone());
            Ok(json!({}))
        })
        .core();
    let reply = core
        .dispatch(
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"t","arguments":{"name":"x"}}}"#,
        )
        .unwrap();
    let v: Value = serde_json::from_slice(&reply).unwrap();
    assert!(v["result"].is_object(), "l'appel doit réussir : {v}");
    let vu = seen
        .lock()
        .unwrap()
        .clone()
        .expect("le handler doit tourner");
    assert_eq!(
        vu.get("count"),
        Some(&json!(7)),
        "le default publié doit être INJECTÉ : le handler a reçu {vu}"
    );

    // Une valeur explicite prime sur le défaut.
    let seen3: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let seen4 = Arc::clone(&seen3);
    let core2 = ServerBuilder::new("reg", "1.0")
        .tool_typed("t", "t", from_schemars::<Args>(), move |args| {
            *seen4.lock().unwrap() = Some(args.clone());
            Ok(json!({}))
        })
        .core();
    let _ = core2.dispatch(
        br#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"t","arguments":{"name":"y","count":42}}}"#,
    );
    let vu2 = seen3
        .lock()
        .unwrap()
        .clone()
        .expect("le handler doit tourner");
    assert_eq!(
        vu2.get("count"),
        Some(&json!(42)),
        "explicite prime : {vu2}"
    );
}

#[test]
fn schemars_default_imbrique_est_injecte_recursivement() {
    let schema = from_schemars::<Outer>();
    assert!(
        schema.has_defaults(),
        "has_defaults doit remonter depuis le sous-document ($ref local)"
    );

    let seen: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let seen2 = Arc::clone(&seen);
    let core = ServerBuilder::new("reg", "1.0")
        .tool_typed("t", "t", from_schemars::<Outer>(), move |args| {
            *seen2.lock().unwrap() = Some(args.clone());
            Ok(json!({}))
        })
        .core();
    // `inner` est PRÉSENT mais vide : le défaut imbriqué (`depth`) doit
    // être injecté — le sous-document est derrière un `$ref` local
    // (`#/$defs/Nested`), résolu sans réseau.
    let _ = core.dispatch(
        br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"t","arguments":{"inner":{}}}}"#,
    );
    let vu = seen
        .lock()
        .unwrap()
        .clone()
        .expect("le handler doit tourner");
    assert_eq!(
        vu["inner"]["depth"],
        json!(3),
        "le défaut imbriqué doit être injecté : {vu}"
    );
}

#[test]
fn schemars_sans_default_publie_ne_laisse_passer_rien() {
    // `inner` est ABSENT et ne porte AUCUN default publié : rien n'est
    // injecté (le contrat publié ne promet rien sur ce champ).
    let seen: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let seen2 = Arc::clone(&seen);
    let core = ServerBuilder::new("reg", "1.0")
        .tool_typed("t", "t", from_schemars::<Outer>(), move |args| {
            *seen2.lock().unwrap() = Some(args.clone());
            Ok(json!({}))
        })
        .core();
    let _ = core.dispatch(
        br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"t","arguments":{}}}"#,
    );
    let vu = seen
        .lock()
        .unwrap()
        .clone()
        .expect("le handler doit tourner");
    assert!(
        vu.get("inner").is_none(),
        "aucun default n'est publié pour `inner` : rien ne doit être injecté : {vu}"
    );
}
