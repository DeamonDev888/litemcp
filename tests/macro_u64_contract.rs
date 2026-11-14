//! Test de régression — TODO « Corriger le support u64 de litemcp-macros ».
//!
//! Avant correctif, la macro annonçait des bornes exactes qu'elle ne tenait
//! pas :
//!
//! 1. **Schéma** : un paramètre `u64` publiait `zod::integer().min(0)` —
//!    donc `maximum: i64::MAX` par défaut de `Int::json()` — et `usize`
//!    publiait `maximum` **borné à i64::MAX** : `i64::MAX + 1` et
//!    `u64::MAX` étaient rejetés à la validation alors que le type Rust les
//!    représente ;
//! 2. **Extraction** : tous les entiers extraient via `as_i64()… as T` —
//!    au-delà de i64::MAX, `as_i64()` renvoie `None` et
//!    `unwrap_or_default()` rendait **0** ; pour `Option<u64>`, une grande
//!    valeur rendait `None` ;
//! 3. **Défauts** : portés par un `i64` — un littéral `u64::MAX` ne
//!    compilait même pas.
//!
//! Le correctif : chemin **non signé cohérent** — schéma `zod::uint()`
//! (bornes exactes, `u64::MAX` et `usize::MAX` **selon la cible**),
//! extraction `as_u64` + `try_from` (jamais de troncature, jamais de
//! négatif transformé en grand entier), littéraux par défaut portés par
//! `i128` et publiés en `u64` quand ils dépassent i64::MAX.

#![cfg(feature = "macros")]

use litemcp::ServerBuilder;
use litemcp::macros::tool;
use serde_json::{Value, json};

fn call(core: &litemcp::ServerCore, body: &str) -> Value {
    let reply = core.dispatch_unchecked(body.as_bytes()).unwrap();
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

// ===========================================================================
// Outils sous test — un par type entier, signature identique
// ===========================================================================

#[tool(name = "pris_u64", description = "u64 exact")]
fn pris_u64(v: u64) -> litemcp::ToolResult {
    Ok(json!({ "recu": v }))
}

#[tool(name = "pris_usize", description = "usize selon la cible")]
fn pris_usize(v: usize) -> litemcp::ToolResult {
    Ok(json!({ "recu": v }))
}

#[tool(name = "pris_option_u64", description = "Option<u64> exact")]
fn pris_option_u64(v: Option<u64>) -> litemcp::ToolResult {
    Ok(json!({ "recu": v }))
}

#[tool(name = "pris_u8", description = "u8 borné")]
fn pris_u8(v: u8) -> litemcp::ToolResult {
    Ok(json!({ "recu": v }))
}

#[tool(name = "pris_u16", description = "u16 borné")]
fn pris_u16(v: u16) -> litemcp::ToolResult {
    Ok(json!({ "recu": v }))
}

#[tool(name = "pris_u32", description = "u32 borné")]
fn pris_u32(v: u32) -> litemcp::ToolResult {
    Ok(json!({ "recu": v }))
}

#[tool(name = "pris_i8", description = "i8 borné")]
fn pris_i8(v: i8) -> litemcp::ToolResult {
    Ok(json!({ "recu": v }))
}

#[tool(name = "pris_i16", description = "i16 borné")]
fn pris_i16(v: i16) -> litemcp::ToolResult {
    Ok(json!({ "recu": v }))
}

#[tool(name = "pris_i32", description = "i32 borné")]
fn pris_i32(v: i32) -> litemcp::ToolResult {
    Ok(json!({ "recu": v }))
}

#[tool(name = "pris_i64", description = "i64 exact")]
fn pris_i64(v: i64) -> litemcp::ToolResult {
    Ok(json!({ "recu": v }))
}

#[tool(name = "pris_isize", description = "isize selon la cible")]
fn pris_isize(v: isize) -> litemcp::ToolResult {
    Ok(json!({ "recu": v }))
}

#[tool(name = "vec_u64", description = "Vec<u64> éléments exacts")]
fn vec_u64(v: Vec<u64>) -> litemcp::ToolResult {
    Ok(json!({ "recu": v }))
}

#[tool(
    name = "defaut_u64_sans_suffixe",
    description = "défaut littéral sans suffixe au-delà de i64::MAX",
    default(v = 9223372036854775808)
)]
fn defaut_u64_sans_suffixe(v: u64) -> litemcp::ToolResult {
    Ok(json!({ "recu": v }))
}

#[tool(
    name = "defaut_u64_max",
    description = "défaut u64::MAX publié et injecté",
    default(v = 18446744073709551615u64)
)]
fn defaut_u64_max(v: u64) -> litemcp::ToolResult {
    Ok(json!({ "recu": v }))
}

#[tool(
    name = "defaut_u64_zero",
    description = "défaut zéro",
    default(v = 0u64)
)]
fn defaut_u64_zero(v: u64) -> litemcp::ToolResult {
    Ok(json!({ "recu": v }))
}

#[tool(
    name = "defaut_i64_min",
    description = "défaut i64::MIN (compatibilité signée)",
    default(v = -9223372036854775808i64)
)]
fn defaut_i64_min(v: i64) -> litemcp::ToolResult {
    Ok(json!({ "recu": v }))
}

fn core_with(tools: &[&str]) -> litemcp::ServerCore {
    let mut builder = ServerBuilder::new("m", "0.1.0");
    for name in tools {
        builder = match *name {
            "pris_u64" => builder.with_tool(register_pris_u64),
            "pris_usize" => builder.with_tool(register_pris_usize),
            "pris_option_u64" => builder.with_tool(register_pris_option_u64),
            "pris_u8" => builder.with_tool(register_pris_u8),
            "pris_u16" => builder.with_tool(register_pris_u16),
            "pris_u32" => builder.with_tool(register_pris_u32),
            "pris_i8" => builder.with_tool(register_pris_i8),
            "pris_i16" => builder.with_tool(register_pris_i16),
            "pris_i32" => builder.with_tool(register_pris_i32),
            "pris_i64" => builder.with_tool(register_pris_i64),
            "pris_isize" => builder.with_tool(register_pris_isize),
            "vec_u64" => builder.with_tool(register_vec_u64),
            "defaut_u64_sans_suffixe" => builder.with_tool(register_defaut_u64_sans_suffixe),
            "defaut_u64_max" => builder.with_tool(register_defaut_u64_max),
            "defaut_u64_zero" => builder.with_tool(register_defaut_u64_zero),
            "defaut_i64_min" => builder.with_tool(register_defaut_i64_min),
            other => panic!("outil inconnu du harnais : {other}"),
        };
    }
    builder.core()
}

// ===========================================================================
// 1. u64 : 0, i64::MAX, i64::MAX + 1, u64::MAX — exacts, jamais tronqués
// ===========================================================================

#[test]
fn u64_valeurs_limites_sont_exactes() {
    let core = core_with(&["pris_u64"]);
    for (envoye, attendu) in [
        (json!(0u64), 0u64),
        (json!(9223372036854775807u64), i64::MAX as u64),
        (json!(9223372036854775808u64), i64::MAX as u64 + 1),
        (json!(18446744073709551615u64), u64::MAX),
    ] {
        let v = call_tool(&core, 1, "pris_u64", json!({ "v": envoye }));
        assert!(
            v.get("error").is_none(),
            "{envoye} doit être accepté par un paramètre u64 : {v}"
        );
        assert_eq!(
            v["result"]["structuredContent"]["recu"],
            json!(attendu),
            "u64 doit arriver EXACT au handler (jamais tronqué) : {v}"
        );
    }
}

#[test]
fn u64_negatif_est_refuse_sans_transformation() {
    let core = core_with(&["pris_u64"]);
    // -1 ne doit JAMAIS devenir un grand entier non signé : refus -32602
    // (minimum publié 0), handler jamais exécuté.
    let v = call_tool(&core, 1, "pris_u64", json!({ "v": -1 }));
    assert_eq!(v["error"]["code"], -32602, "-1 doit être refusé : {v}");
    let issues = v["error"]["data"]["issues"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        issues
            .iter()
            .any(|i| i.as_str().unwrap_or("").contains("≥ 0")),
        "le refus doit citer la borne non signée publiée : {v}"
    );
}

#[test]
fn u64_schema_publie_les_bornes_exactes() {
    let core = core_with(&["pris_u64"]);
    let list = call(&core, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let schema = &list["result"]["tools"][0]["inputSchema"]["properties"]["v"];
    assert_eq!(
        schema["minimum"],
        json!(0u64),
        "minimum non signé publié : {list}"
    );
    assert_eq!(
        schema["maximum"],
        json!(18446744073709551615u64),
        "maximum u64::MAX publié (pas i64::MAX) : {list}"
    );
}

// ===========================================================================
// 2. usize : bornes selon la cible
// ===========================================================================

#[test]
fn usize_bornes_selon_la_cible_sont_exactes() {
    let core = core_with(&["pris_usize"]);
    for (envoye, attendu) in [(json!(0usize), 0usize), (json!(usize::MAX), usize::MAX)] {
        let v = call_tool(&core, 1, "pris_usize", json!({ "v": envoye }));
        assert!(
            v.get("error").is_none(),
            "usize::MAX doit être accepté (borne exacte de la cible) : {v}"
        );
        assert_eq!(
            v["result"]["structuredContent"]["recu"],
            json!(attendu),
            "usize doit arriver exact : {v}"
        );
    }
    // Le schéma publié annonce la borne de LA CIBLE (expression, pas
    // littéral figé 64 bits).
    let list = call(&core, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let maximum = &list["result"]["tools"][0]["inputSchema"]["properties"]["v"]["maximum"];
    assert_eq!(
        maximum,
        &json!(usize::MAX as u64),
        "maximum usize publié = usize::MAX de la cible : {list}"
    );
}

// ===========================================================================
// 2bis. isize : bornes selon la cible — compatibilité signée incluse
// ===========================================================================

#[test]
fn isize_bornes_selon_la_cible_sont_exactes() {
    let core = core_with(&["pris_isize"]);
    for (envoye, attendu) in [
        (json!(0i64), 0isize),
        (json!(isize::MAX as i64), isize::MAX),
        (json!(isize::MIN as i64), isize::MIN),
    ] {
        let v = call_tool(&core, 1, "pris_isize", json!({ "v": envoye }));
        assert!(
            v.get("error").is_none(),
            "isize {envoye} doit être accepté (borne exacte de la cible) : {v}"
        );
        assert_eq!(
            v["result"]["structuredContent"]["recu"],
            json!(attendu),
            "isize doit arriver exact : {v}"
        );
    }
    // Un cran au-dessus de la borne de la cible : refusé — la borne est
    // calculée PAR LA CIBLE (isize::MAX as i128 + 1 tient toujours en u64,
    // quelle que soit la largeur d'isize).
    let hors = json!({ "v": (isize::MAX as i128 + 1) as u64 });
    let v = call_tool(&core, 2, "pris_isize", hors);
    assert_eq!(
        v["error"]["code"], -32602,
        "isize::MAX + 1 doit être refusé : {v}"
    );

    // Le schéma publié annonce la borne de LA CIBLE (64 bits : i64::MAX,
    // 32 bits : i32::MAX — jamais un littéral figé).
    let list = call(&core, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let maximum = &list["result"]["tools"][0]["inputSchema"]["properties"]["v"]["maximum"];
    assert_eq!(
        maximum,
        &json!(isize::MAX as i64),
        "maximum isize publié = isize::MAX de la cible : {list}"
    );
}

// ===========================================================================
// 2ter. Vec<u64> : chaque élément exact ; un négatif est refusé
// ===========================================================================

#[test]
fn vec_u64_elements_exact_et_negatif_refuse() {
    let core = core_with(&["vec_u64"]);
    // 0, i64::MAX + 1 et u64::MAX traversent la boucle d'extraction exacts.
    let v = call_tool(
        &core,
        1,
        "vec_u64",
        json!({ "v": [0u64, 9223372036854775808u64, u64::MAX] }),
    );
    assert!(v.get("error").is_none(), "{v}");
    assert_eq!(
        v["result"]["structuredContent"]["recu"],
        json!([0u64, 9223372036854775808u64, u64::MAX]),
        "chaque élément Vec<u64> doit arriver exact : {v}"
    );
    // Un élément négatif est refusé (jamais transformé en grand entier) —
    // l'issue cite le chemin indexé de l'élément fautif.
    let v = call_tool(&core, 2, "vec_u64", json!({ "v": [1, -1] }));
    assert_eq!(
        v["error"]["code"], -32602,
        "-1 en élément doit être refusé : {v}"
    );
    let issues = v["error"]["data"]["issues"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        issues
            .iter()
            .any(|i| i.as_str().unwrap_or("").contains("v[1]")
                && i.as_str().unwrap_or("").contains("≥ 0")),
        "le refus doit pointer l'élément fautif et citer la borne non signée : {v}"
    );
}

// ===========================================================================
// 2quater. Option<u64> : optionnel ne veut pas dire sans validation
// ===========================================================================

#[test]
fn option_u64_negatif_fourni_est_refuse() {
    let core = core_with(&["pris_option_u64"]);
    // Le champ est optionnel, la valeur fournie reste validée : -1 est
    // refusé — jamais transformé en grand entier non signé.
    let v = call_tool(&core, 1, "pris_option_u64", json!({ "v": -1 }));
    assert_eq!(v["error"]["code"], -32602, "{v}");
}

// ===========================================================================
// 2quinquies. Intégraux flottants : normalisés en u64 EXACT, au-delà refusé
// ===========================================================================

#[test]
fn u64_integration_flottant_est_normalise_exact() {
    let core = core_with(&["pris_u64"]);
    // 1e19 : intégral en f64, dans la plage u64 (1e19 < 2^64, exactement
    // représentable) — accepté (sémantique JSON Schema d'`integer`) et
    // normalisé en u64 EXACT avant le handler : `as_u64()` réussit.
    let v = call_tool(&core, 1, "pris_u64", json!({ "v": 1e19 }));
    assert!(
        v.get("error").is_none(),
        "1e19 intégral doit être accepté : {v}"
    );
    assert_eq!(
        v["result"]["structuredContent"]["recu"],
        json!(10_000_000_000_000_000_000u64),
        "1e19 doit arriver en u64 exact (jamais approximé ni 0) : {v}"
    );
    // 2^64 exactement : intégral mais hors de la plage u64 — refus de borne,
    // jamais de troncature silencieuse.
    let v = call_tool(
        &core,
        2,
        "pris_u64",
        json!({ "v": 18_446_744_073_709_551_616.0f64 }),
    );
    assert_eq!(v["error"]["code"], -32602, "2^64 doit être refusé : {v}");
    let issues = v["error"]["data"]["issues"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        issues
            .iter()
            .any(|i| i.as_str().unwrap_or("").contains("≤ 18446744073709551615")),
        "le refus de 2^64 doit citer la borne u64::MAX publiée : {v}"
    );
}

// ===========================================================================
// 3. Option<u64> : une grande valeur fournie reste Some(exact)
// ===========================================================================

#[test]
fn option_u64_grande_valeur_est_some_exact() {
    let core = core_with(&["pris_option_u64"]);
    // Fournie : Some(i64::MAX + 1) — pas None, pas 0.
    let v = call_tool(
        &core,
        1,
        "pris_option_u64",
        json!({ "v": 9223372036854775808u64 }),
    );
    assert!(v.get("error").is_none(), "{v}");
    assert_eq!(
        v["result"]["structuredContent"]["recu"],
        json!(9223372036854775808u64),
        "Option<u64> doit porter la valeur exacte : {v}"
    );
    // Absente : None.
    let v = call_tool(&core, 2, "pris_option_u64", json!({}));
    assert!(v.get("error").is_none(), "{v}");
    assert_eq!(
        v["result"]["structuredContent"]["recu"],
        json!(Value::Null),
        "champ absent → None : {v}"
    );

    // `null` : même verdict que l'absence (M3) — accepté, `None`.
    let v = call_tool(&core, 3, "pris_option_u64", json!({ "v": null }));
    assert!(
        v.get("error").is_none(),
        "`null` doit valoir absence pour `Option<u64>` : {v}"
    );
    assert_eq!(
        v["result"]["structuredContent"]["recu"],
        json!(Value::Null),
        "champ `null` → None : {v}"
    );

    // Le schéma publié dit la même chose : `null` est admis.
    let list = call(&core, r#"{"jsonrpc":"2.0","id":4,"method":"tools/list"}"#);
    let tools = list["result"]["tools"].as_array().unwrap();
    let schema = &tools
        .iter()
        .find(|t| t["name"] == "pris_option_u64")
        .unwrap()["inputSchema"];
    assert_eq!(
        schema["properties"]["v"]["type"],
        json!(["integer", "null"]),
        "{list}"
    );
}

// ===========================================================================
// 4. Bornes exactes de chaque type (compatibilité signée incluse)
// ===========================================================================

#[test]
fn bornes_exactes_des_types_non_signes() {
    let core = core_with(&["pris_u8", "pris_u16", "pris_u32"]);
    for (name, max) in [
        ("pris_u8", u8::MAX as u64),
        ("pris_u16", u16::MAX as u64),
        ("pris_u32", u32::MAX as u64),
    ] {
        let v = call_tool(&core, 1, name, json!({ "v": max }));
        assert!(
            v.get("error").is_none(),
            "{name} doit accepter sa borne haute ({max}) : {v}"
        );
        assert_eq!(v["result"]["structuredContent"]["recu"], json!(max), "{v}");
    }
    // Un cran au-dessus de la borne : refusé (bornes exactes publiées).
    for (name, debordement) in [
        ("pris_u8", json!(256u64)),
        ("pris_u16", json!(65536u64)),
        ("pris_u32", json!(4294967296u64)),
    ] {
        let v = call_tool(&core, 2, name, json!({ "v": debordement }));
        assert_eq!(
            v["error"]["code"], -32602,
            "{name} doit refuser {debordement} : {v}"
        );
    }
}

#[test]
fn bornes_exactes_des_types_signes_inchangees() {
    // Compatibilité signée : les mêmes valeurs qu'avant le correctif.
    let core = core_with(&["pris_i8", "pris_i16", "pris_i32", "pris_i64"]);
    for (name, envoye) in [
        ("pris_i8", json!(i8::MAX)),
        ("pris_i8", json!(i8::MIN)),
        ("pris_i16", json!(i16::MAX)),
        ("pris_i16", json!(i16::MIN)),
        ("pris_i32", json!(i32::MAX)),
        ("pris_i32", json!(i32::MIN)),
        ("pris_i64", json!(i64::MAX)),
        ("pris_i64", json!(i64::MIN)),
    ] {
        let v = call_tool(&core, 1, name, json!({ "v": envoye }));
        assert!(
            v.get("error").is_none(),
            "{name} doit accepter {envoye} (compatibilité signée) : {v}"
        );
        assert_eq!(v["result"]["structuredContent"]["recu"], envoye, "{v}");
    }
    // Hors bornes signées : toujours refusé.
    for (name, hors) in [
        ("pris_i8", json!(128)),
        ("pris_i8", json!(-129)),
        ("pris_i16", json!(32768)),
        ("pris_i32", json!(2147483648i64)),
        ("pris_i64", json!(9223372036854775808u64)),
    ] {
        let v = call_tool(&core, 2, name, json!({ "v": hors }));
        assert_eq!(
            v["error"]["code"], -32602,
            "{name} doit refuser {hors} : {v}"
        );
    }
}

// ===========================================================================
// 5. Défauts : u64::MAX publié et injecté, zéro, et compatibilité i64
// ===========================================================================

#[test]
fn defaut_u64_max_est_publie_puis_injecte() {
    let core = core_with(&["defaut_u64_max"]);
    let list = call(&core, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let schema = &list["result"]["tools"][0]["inputSchema"]["properties"]["v"];
    assert_eq!(
        schema["default"],
        json!(18446744073709551615u64),
        "le littéral u64::MAX doit être publié tel quel (pas via i64) : {list}"
    );

    // Absent → le défaut u64::MAX arrive EXACT au handler.
    let v = call_tool(&core, 2, "defaut_u64_max", json!({}));
    assert!(v.get("error").is_none(), "{v}");
    assert_eq!(
        v["result"]["structuredContent"]["recu"],
        json!(u64::MAX),
        "le défaut u64::MAX doit être injecté exact : {v}"
    );

    // Fourni → la valeur explicite gagne.
    let v = call_tool(&core, 3, "defaut_u64_max", json!({ "v": 42u64 }));
    assert!(v.get("error").is_none(), "{v}");
    assert_eq!(
        v["result"]["structuredContent"]["recu"],
        json!(42u64),
        "{v}"
    );
}

#[test]
fn defaut_zero_et_defaut_i64_min_restent_exact() {
    let core = core_with(&["defaut_u64_zero", "defaut_i64_min"]);
    let v = call_tool(&core, 1, "defaut_u64_zero", json!({}));
    assert!(v.get("error").is_none(), "{v}");
    assert_eq!(v["result"]["structuredContent"]["recu"], json!(0u64), "{v}");

    let v = call_tool(&core, 2, "defaut_i64_min", json!({}));
    assert!(v.get("error").is_none(), "{v}");
    assert_eq!(
        v["result"]["structuredContent"]["recu"],
        json!(i64::MIN),
        "le défaut i64::MIN doit rester exact (compatibilité signée) : {v}"
    );
}

#[test]
fn defaut_u64_sans_suffixe_est_publie_puis_injecte() {
    // Littéral décimal SANS suffixe au-delà de i64::MAX : porté par i128 à
    // la compilation et publié en u64 exact — jamais tronqué, jamais refusé
    // par le parsing.
    let core = core_with(&["defaut_u64_sans_suffixe"]);
    let list = call(&core, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let schema = &list["result"]["tools"][0]["inputSchema"]["properties"]["v"];
    assert_eq!(
        schema["default"],
        json!(9223372036854775808u64),
        "le littéral sans suffixe i64::MAX + 1 doit être publié en u64 exact : {list}"
    );

    let v = call_tool(&core, 1, "defaut_u64_sans_suffixe", json!({}));
    assert!(v.get("error").is_none(), "{v}");
    assert_eq!(
        v["result"]["structuredContent"]["recu"],
        json!(9223372036854775808u64),
        "le défaut sans suffixe doit être injecté exact : {v}"
    );
}

#[test]
fn null_sur_un_parametre_a_defaut_recoit_le_defaut() {
    // « `null` ≡ absence » vaut aussi pour un paramètre de macro couvert par
    // `default(..)` : la macro émet `.opt(nom, schéma.default(valeur))`, donc
    // le champ publie `type: [T, "null"]` **avec** son défaut et `null` est
    // traité comme l'absence (résidu D1).
    let core = core_with(&["defaut_u64_max"]);
    let list = call(&core, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let schema = &list["result"]["tools"][0]["inputSchema"]["properties"]["v"];
    assert_eq!(schema["type"], json!(["integer", "null"]), "{list}");
    assert_eq!(schema["default"], json!(u64::MAX), "{list}");

    let v = call_tool(&core, 2, "defaut_u64_max", json!({ "v": null }));
    assert!(v.get("error").is_none(), "`null` doit valoir absence : {v}");
    assert_eq!(
        v["result"]["structuredContent"]["recu"],
        json!(u64::MAX),
        "`null` doit recevoir le défaut : {v}"
    );
}
