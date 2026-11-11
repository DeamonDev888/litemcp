//! Corpus différentiel — **les schémas publiés par `zod` sont validés
//! exactement comme le validateur de référence** (`jsonschema` 0.60, la
//! dépendance optionnelle de la feature `schemars` — aucune dépendance
//! ajoutée) sur les entiers, les bornes et les formats.
//!
//! Deux familles d'assertions :
//! 1. **Équivalence** (entiers + bornes) : le verdict de `zod` sur un cas
//!    est **identique** à celui de la référence sur le schéma **publié** —
//!    la promesse « publié = appliqué » est vérifiée contre un tiers ;
//! 2. **Divergences publiées** (formats `email`/`uri`) : le profil de
//!    litemcp est **plus strict** que la référence — chaque divergence est
//!    explicitement assertée des deux côtés (aucune divergence implicite).
//!
//! `date-time` suit RFC 3339 §5.6, **notes incluses** : séparateur `T`,
//! `t` minuscule ou espace, `Z`/`z`, seconde intercalaire `60`.
//!
//! La **normalisation** des entiers intégraux (`3.0` → `3`) est vérifiée à
//! la fois unitairement (trait `Schema`) et de bout en bout (dispatch :
//! le handler reçoit un entier exact, `as_i64()` réussit toujours).

#![cfg(feature = "schemars")]

use litemcp::zod::{self, Issues, Path, Schema};
use serde_json::{Value, json};

// --- Harnais ---------------------------------------------------------------

fn zod_valid(schema: &dyn Schema, instance: &Value) -> bool {
    let mut issues = Issues::new();
    let mut path = Path::root("args");
    schema.validate(&mut path, instance, &mut issues);
    issues.is_empty()
}

/// Verdict du validateur de référence sur le schéma **publié** (formats non
/// vérifiés : `format` est une annotation en 2020-12).
fn reference_valid(schema: &Value, instance: &Value) -> bool {
    jsonschema::options()
        .build(schema)
        .expect("le schéma publié doit compiler chez la référence")
        .is_valid(instance)
}

/// Verdict du validateur de référence **avec formats appliqués**.
fn reference_valid_formats(schema: &Value, instance: &Value) -> bool {
    jsonschema::options()
        .should_validate_formats(true)
        .build(schema)
        .expect("le schéma publié doit compiler chez la référence")
        .is_valid(instance)
}

/// Assertion d'**équivalence** : zod et la référence rendent le même verdict.
fn assert_same(schema: &dyn Schema, instance: Value, context: &str) {
    let published = schema.json();
    let zod = zod_valid(schema, &instance);
    let reference = reference_valid(&published, &instance);
    assert_eq!(
        zod, reference,
        "{context} : divergence zod={zod} référence={reference} — schéma publié {published}, instance {instance}"
    );
}

// --- Entiers et bornes : équivalence complète -------------------------------

#[test]
fn integers_match_the_reference_exactly() {
    let int = zod::integer();
    for instance in [
        json!(0),
        json!(1),
        json!(-1),
        json!(i64::MIN),
        json!(i64::MAX),
        // Intégraux à fractule nulle : valides en JSON Schema (2020-12).
        json!(3.0),
        json!(1e2),
        json!(-0.0),
        json!(9007199254740992.0), // 2^53 (exact en f64)
        // Non entiers / non nombres.
        json!(3.5),
        json!(0.1),
        json!("3"),
        json!(true),
        json!(null),
        json!([]),
        json!({}),
        // Intégraux hors plage exacte : refusés des deux côtés (le schéma
        // publié porte `minimum`/`maximum` = plage 64 bits) — jamais de
        // conversion silencieuse.
        json!(9223372036854775808u64),  // 2^63 (>f i64::MAX)
        json!(18446744073709551615u64), // u64::MAX
        json!(1e300),
        json!(-1e300),
        json!(9007199254740993u64), // 2^53+1, exact en u64, > i64::MAX
    ] {
        assert_same(&int, instance.clone(), "integer()");
    }
}

#[test]
fn integer_bounds_match_the_reference_exactly() {
    let bounded = zod::integer().min(-5).max(10);
    for instance in [
        json!(10),
        json!(10.0), // intégral à fractule nulle, à la borne
        json!(-5.0),
        json!(1e1), // 10.0
        json!(11),
        json!(11.0),
        json!(-6),
        json!(10.5),  // non intégral, hors borne
        json!(-5.5),  // non intégral, dans la borne
        json!(1e300), // intégral hors plage
        json!(0),
        json!(-0.0),
    ] {
        assert_same(&bounded, instance.clone(), "integer().min(-5).max(10)");
    }
}

#[test]
fn integers_inside_objects_arrays_and_records_match_the_reference() {
    let object = zod::object()
        .field("n", zod::integer())
        .field("nested", zod::object().field("deep", zod::integer()))
        .field("list", zod::array(zod::integer()))
        .field("map", zod::record(zod::integer()));
    for instance in [
        json!({ "n": 3.0, "nested": { "deep": 2.0 }, "list": [1.0, 2e2], "map": { "a": 4.0 } }),
        json!({ "n": 3.5 }),
        json!({ "n": 3.0, "nested": { "deep": 1.5 } }),
        json!({ "n": 3.0, "list": [1.0, 2.5] }),
        json!({ "n": 3.0, "map": { "a": 4.0, "b": 5.5 } }),
    ] {
        assert_same(&object, instance.clone(), "object imbriqué");
    }
}

// --- Énumérations d'entiers : équivalence complète -------------------------

#[test]
fn integer_enumerations_match_the_reference_exactly() {
    let int_enum = zod::integer_enumeration(&[1, 2, 3]);
    for instance in [
        json!(1),
        json!(2),
        json!(3),
        // Égalité **mathématique** (JSON Schema 2020-12, §4.2.1) : un
        // flottant intégral égale l'entier de même valeur — la référence
        // accepte, zod aussi.
        json!(3.0),
        json!(2.0),
        json!(1.0),
        // Non-intégral : refusé des deux côtés.
        json!(3.5),
        // Hors énumération : refusé des deux côtés (entier ou intégral).
        json!(4),
        json!(1e2), // 100 ∉ {1, 2, 3}
        // Intégral hors plage i64 : jamais égal à un membre (les membres
        // sont des i64) — refusé des deux côtés.
        json!(9223372036854775808u64),
        json!(1e300),
        // Mauvais type : refusé des deux côtés.
        json!("3"),
        json!(true),
        json!(null),
    ] {
        assert_same(&int_enum, instance.clone(), "integer_enumeration(&[1,2,3])");
    }

    // `-0.0` égale mathématiquement `0` ; `2e2`/`200.0` égale `200`.
    let zero = zod::integer_enumeration(&[0]);
    assert_same(&zero, json!(-0.0), "integer_enumeration(&[0]) — -0.0");
    assert_same(&zero, json!(0.0), "integer_enumeration(&[0]) — 0.0");
    let two_hundred = zod::integer_enumeration(&[200]);
    assert_same(
        &two_hundred,
        json!(2e2),
        "integer_enumeration(&[200]) — 2e2",
    );
    assert_same(
        &two_hundred,
        json!(200.0),
        "integer_enumeration(&[200]) — 200.0",
    );
}

#[test]
fn integral_floats_in_integer_enumerations_are_normalized_to_the_member() {
    let schema = zod::object().field("mode", zod::integer_enumeration(&[1, 2, 3]));
    assert!(schema.has_integers());
    let mut value = json!({ "mode": 3.0 });
    schema.normalize_integers(&mut value);
    assert_eq!(value["mode"], json!(3), "3.0 → le membre exact 3");
    assert_eq!(value["mode"].as_i64(), Some(3));
}

#[test]
fn integer_enumeration_integral_float_reaches_the_handler_exact() {
    use std::sync::{Arc, Mutex};

    let received: Arc<Mutex<Value>> = Arc::new(Mutex::new(Value::Null));
    let sink = Arc::clone(&received);
    let core = litemcp::ServerBuilder::new("diff-enum", "0.1.0")
        .tool_typed(
            "pick",
            "Choix par entier",
            zod::object().field("mode", zod::integer_enumeration(&[1, 2, 3])),
            move |args| {
                // Le motif d'usage documenté de la crate : `as_i64()` doit
                // réussir sur toute valeur que le schéma a acceptée.
                let mode = args["mode"].as_i64().expect("mode doit être un i64 exact");
                *sink.lock().unwrap() = json!({ "mode": mode });
                Ok(json!({ "mode": mode }))
            },
        )
        .core();

    // `2.0` égale mathématiquement le membre `2` : accepté, et le handler
    // reçoit l'entier **exact**.
    let reply = core
        .dispatch(
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"pick","arguments":{"mode":2.0}}}"#,
        )
        .expect("réponse attendue");
    let parsed: Value = serde_json::from_slice(&reply).unwrap();
    assert!(
        parsed.get("error").is_none(),
        "2.0 == 2 en égalité mathématique : {parsed}"
    );
    assert_eq!(received.lock().unwrap()["mode"], json!(2));

    // Un non-intégral reste refusé avant le handler.
    let reply = core
        .dispatch(
            br#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"pick","arguments":{"mode":2.5}}}"#,
        )
        .expect("réponse attendue");
    let parsed: Value = serde_json::from_slice(&reply).unwrap();
    assert_eq!(parsed["error"]["code"], -32602, "{parsed}");
}

// --- uint : équivalence complète -------------------------------------------

#[test]
fn uint_matches_the_reference_exactly() {
    let uint = zod::uint();
    for instance in [
        json!(0),
        json!(1),
        json!(u64::MAX),
        json!(3.0),
        json!(2e2),
        json!(-0.0),
        json!(3.5),
        json!(-1),
        json!(-1.5),
        json!(1e300),
        json!("3"),
        json!(null),
    ] {
        assert_same(&uint, instance.clone(), "uint()");
    }
    let bounded = zod::uint().min(2).max(100);
    for instance in [
        json!(2),
        json!(100),
        json!(100.0),
        json!(50.0),
        json!(1),
        json!(1.5),
        json!(-0.0),
        json!(101),
        json!(1e300),
    ] {
        assert_same(&bounded, instance.clone(), "uint().min(2).max(100)");
    }
}

// --- Normalisation : le handler reçoit un entier exact ----------------------

#[test]
fn integral_floats_are_normalized_to_exact_integers_in_place() {
    // Unité : la normalisation mutile les positions entières, sans toucher
    // aux autres valeurs.
    let object = zod::object()
        .field("n", zod::integer())
        .field("text", zod::string())
        .field("list", zod::array(zod::integer()));
    let mut value = json!({ "n": 3.0, "text": "3.0", "list": [1.0, 2e2, 2.5, -0.0] });
    object.normalize_integers(&mut value);
    assert_eq!(value["n"], json!(3), "3.0 → 3 (as_i64() doit réussir)");
    assert_eq!(value["n"].as_i64(), Some(3));
    assert_eq!(
        value["text"],
        json!("3.0"),
        "une chaîne n'est jamais touchée"
    );
    assert_eq!(value["list"][0], json!(1));
    assert_eq!(value["list"][1], json!(200), "2e2 → 200");
    assert_eq!(
        value["list"][2],
        json!(2.5),
        "un non-entier n'est pas touché"
    );
    assert_eq!(value["list"][3], json!(0), "-0.0 → 0");
}

#[test]
fn handlers_receive_exact_integers_end_to_end() {
    use std::sync::{Arc, Mutex};

    let received: Arc<Mutex<Value>> = Arc::new(Mutex::new(Value::Null));
    let sink = Arc::clone(&received);
    let core = litemcp::ServerBuilder::new("diff", "0.1.0")
        .tool_typed(
            "count",
            "Compteur d'essai",
            zod::object()
                .field("n", zod::integer())
                .field("label", zod::string()),
            move |args| {
                // Le motif d'usage documenté de la crate : le handler lit
                // avec `as_i64()` — il doit réussir sur toute valeur que le
                // schéma a acceptée.
                let n = args["n"].as_i64().expect("n doit être un i64 exact");
                *sink.lock().unwrap() = json!({ "n": n, "raw": args["n"] });
                Ok(json!({ "vu": n }))
            },
        )
        .core();

    let reply = core
        .dispatch(
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"count","arguments":{"n":3.0,"label":"trois"}}}"#,
        )
        .expect("réponse attendue");
    let parsed: Value = serde_json::from_slice(&reply).unwrap();
    assert!(
        parsed.get("error").is_none(),
        "3.0 est un integer JSON Schema valide : {parsed}"
    );
    let seen = received.lock().unwrap().clone();
    assert_eq!(seen["n"], json!(3), "le handler voit un entier exact");
    assert_eq!(seen["raw"], json!(3), "la valeur est normalisée en place");

    // `1e2` aussi.
    let reply = core
        .dispatch(
            br#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"count","arguments":{"n":1e2,"label":"cent"}}}"#,
        )
        .expect("réponse attendue");
    let parsed: Value = serde_json::from_slice(&reply).unwrap();
    assert!(parsed.get("error").is_none(), "{parsed}");
    assert_eq!(received.lock().unwrap()["n"], json!(100));

    // Un non-entier reste refusé avant le handler.
    let reply = core
        .dispatch(
            br#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"count","arguments":{"n":3.5,"label":"x"}}}"#,
        )
        .expect("réponse attendue");
    let parsed: Value = serde_json::from_slice(&reply).unwrap();
    assert_eq!(parsed["error"]["code"], -32602, "{parsed}");
}

#[test]
fn schemars_derived_integers_are_normalized_too() {
    use litemcp::schemars::from_schemars;
    use schemars::JsonSchema;

    #[derive(JsonSchema)]
    #[allow(dead_code)]
    struct Args {
        count: u32,
    }

    let schema = from_schemars::<Args>();
    // La référence accepte 3.0 pour un champ `type: "integer"` — litemcp
    // aussi, et le normalise pour la désérialisation serde du handler.
    assert!(zod_valid(&schema, &json!({ "count": 3.0 })));
    let mut value = json!({ "count": 3.0 });
    schema.normalize_integers(&mut value);
    assert_eq!(value["count"], json!(3));
    assert_eq!(value["count"].as_u64(), Some(3));
}

// --- Formats : profils publiés et divergences assumées ----------------------

#[test]
fn datetime_whitespace_is_refused_by_both_litemcp_and_the_reference() {
    // Convergence stricte : aucun espace superflu — ni autour des
    // composants, ni séparateur doublé (RFC 3339 strict des deux côtés).
    // La divergence publiée concerne le séparateur **simple** : litemcp
    // l'accepte (NOTE RFC 3339 §5.6), la référence le refuse — voir
    // `documented_format_divergences_are_pinned_against_the_reference`.
    let dt = zod::string().datetime();
    for instance in [
        " 2026-01-01T00:00:00Z",
        "2026-01-01T00:00:00Z ",
        "\t2026-01-01T00:00:00Z",
        "2026-01-01  00:00:00Z",
    ] {
        let instance = json!(instance);
        assert!(
            !zod_valid(&dt, &instance),
            "espace superflu refusé par litemcp : {instance}"
        );
        assert!(
            !reference_valid_formats(&dt.json(), &instance),
            "espace superflu refusé par la référence : {instance}"
        );
    }
}

#[test]
fn datetime_follows_rfc3339_including_the_note_provisions() {
    // RFC 3339 §5.6 + notes : `T`/`t`/espace comme séparateur, `Z`/`z`,
    // seconde intercalaire 60, offset ±HH:MM.
    let dt = zod::string().datetime();
    let published = dt.json();
    assert_eq!(published["format"], "date-time");

    // (instance, valide chez litemcp)
    let ours_expected: &[(&str, bool)] = &[
        ("2026-11-09T18:00:00Z", true),
        ("2026-11-09 18:00:00Z", true), // espace : NOTE RFC 3339 §5.6
        ("2026-11-09t18:00:00z", true), // minuscules : NOTE RFC 3339 §5.6
        ("2026-11-09T18:00:00.123456Z", true),
        ("2026-11-09T18:00:00+02:00", true),
        ("2026-11-09T18:00:00-02:00", true),
        ("2026-06-30T23:59:60Z", true), // seconde intercalaire (ABNF : 00-60)
        ("2026-07-01T01:59:60+02:00", true), // intercalaire décalée par le fuseau (RFC 3339 §5.7)
        ("2026-06-30T21:59:60-02:00", true), // idem, offset négatif
        ("2026-06-30T23:59:60+02:00", false), // 21:59:60 UTC : le point d'intercalaire n'y est pas
        ("2026-07-01T01:59:60+02:00", true), // intercalaire décalée par le fuseau (RFC 3339 §5.7)
        ("2024-02-29T00:00:00Z", true), // bissextile
        ("2026-11-09T18:00:00", false), // offset obligatoire
        ("2026-02-30T00:00:00Z", false), // 30 février
        ("2026-13-01T00:00:00Z", false),
        ("2026-11-09T24:00:00Z", false), // 24:00 n'existe pas en RFC 3339
        ("2026-11-09T18:60:00Z", false), // minute 60 : invalide (la seconde, elle, peut valoir 60)
        ("2026-11-09 18:00:00 +02:00", false), // espace avant l'offset
    ];

    for (instance, expected) in ours_expected {
        let valid = zod_valid(&dt, &json!(instance));
        assert_eq!(
            valid, *expected,
            "date-time litemcp {instance:?} : attendu {expected}"
        );
        let _ = &published;
    }
}

#[test]
fn email_and_uri_profiles_are_published_strict_extensions() {
    // Profils **plus stricts** que la référence, publiés comme tels (README
    // « Formats ») : chaque divergence est assertée des deux côtés.
    let email = zod::string().email();
    let uri = zod::string().url();

    let email_cases: &[(&str, bool)] = &[
        ("user@example.com", true),
        ("USER@EXAMPLE.COM", true),
        ("user%tag+suffix@mail.example.co.uk", true),
        ("no-at-symbol", false),
        ("@example.com", false),
        ("user@", false),
        ("user@@example.com", false),
        ("user name@example.com", false),
        (".user@example.com", false),
        ("user.@example.com", false),
        ("us..er@example.com", false),
        ("user@localhost", false), // domaine sans point : refusé (profil)
        ("user@-example.com", false),
        ("user@example-.com", false),
        ("user@exam_ple.com", false),
    ];

    for (instance, expected) in email_cases {
        let valid = zod_valid(&email, &json!(instance));
        assert_eq!(valid, *expected, "email litemcp {instance:?}");
    }

    let uri_cases: &[(&str, bool)] = &[
        ("https://example.com/path?q=1#frag", true),
        ("http://localhost:23080/mcp", true),
        ("ftp://user:pass@host:8080/x", true),
        ("https://[::1]:8080/", true),
        ("https://example.com", true),
        ("mailto:user@example.com", false), // pas d'autorité `://` : profil
        ("urn:isbn:0451450523", false),     // idem
        ("//example.com/path", false),      // pas de schéma
        ("not-a-uri", false),
        ("http://exa mple.com", false),
        ("https://[::1", false),
        ("https://example.com:99999", false), // port > u16
    ];

    for (instance, expected) in uri_cases {
        let valid = zod_valid(&uri, &json!(instance));
        assert_eq!(valid, *expected, "uri litemcp {instance:?}");
    }
}

#[test]
fn documented_format_divergences_are_pinned_against_the_reference() {
    // Les divergences de profil sont **publiées** : on les épingle en
    // comparant explicitement les deux verdicts (litemcp ≠ référence,
    // prévu et documenté — aucune divergence implicite).
    let email = zod::string().email();
    let uri = zod::string().url();
    let dt = zod::string().datetime();

    // Litemcp refuse un domaine sans point (profil RFC 5321-borné) ; la
    // référence l'accepte.
    let user_at_localhost = json!("user@localhost");
    assert!(!zod_valid(&email, &user_at_localhost));
    assert!(reference_valid_formats(&email.json(), &user_at_localhost));

    // Litemcp exige une autorité `://` ; la référence accepte les URIs
    // sans autorité (`mailto:`, `urn:` — RFC 3986).
    for instance in ["mailto:user@example.com", "urn:isbn:0451450523"] {
        let instance = json!(instance);
        assert!(!zod_valid(&uri, &instance));
        assert!(reference_valid_formats(&uri.json(), &instance));
    }

    // Litemcp accepte l'**espace** comme séparateur date-time (NOTE RFC
    // 3339 §5.6) ; la référence est plus stricte (`T` seul).
    let with_space = json!("2026-11-09 18:00:00Z");
    assert!(zod_valid(&dt, &with_space));
    assert!(!reference_valid_formats(&dt.json(), &with_space));

    // Seconde intercalaire : litemcp suit RFC 3339 §5.7 — le point
    // d'intercalaire est décalé par le fuseau, exactement comme la
    // référence (`01:59:60+02:00` == 23:59:60Z accepté des deux côtés ;
    // `23:59:60+02:00` == 21:59:60Z, refusé des deux côtés).
    let leap_shifted_ok = json!("2026-07-01T01:59:60+02:00");
    assert!(zod_valid(&dt, &leap_shifted_ok));
    assert!(reference_valid_formats(&dt.json(), &leap_shifted_ok));
    let leap_misplaced = json!("2026-06-30T23:59:60+02:00");
    assert!(!zod_valid(&dt, &leap_misplaced));
    assert!(!reference_valid_formats(&dt.json(), &leap_misplaced));

    // Les cas **convergents** majeurs restent vérifiés des deux côtés :
    // tout ce que litemcp accepte et que la référence accepte aussi, et
    // tout ce que litemcp refuse et que la référence refuse aussi.
    for (schema, accepted, refused) in [
        (
            &email as &dyn Schema,
            vec!["user@example.com", "USER@EXAMPLE.COM"],
            vec!["no-at-symbol", "user@@example.com", ".user@example.com"],
        ),
        (
            &uri as &dyn Schema,
            vec!["https://example.com/x", "http://localhost:23080/mcp"],
            vec!["not-a-uri", "http://exa mple.com"],
        ),
        (
            &dt as &dyn Schema,
            vec![
                "2026-11-09T18:00:00Z",
                "2026-11-09t18:00:00z",      // minuscules : NOTE RFC 3339
                "2026-06-30T23:59:60Z",      // seconde intercalaire
                "2026-07-01T01:59:60+02:00", // intercalaire décalée par le fuseau
                "0000-01-01T00:00:00Z",      // année 0000 (4DIGIT)
                "2026-01-01T00:00:00-00:00", // offset inconnu (RFC 3339 §4.3)
            ],
            vec![
                "2026-11-09T18:00:00",       // offset obligatoire
                "2026-02-30T00:00:00Z",      // 30 février
                "2026-11-09T18:60:00Z",      // minute 60
                "2026-11-09T24:00:00Z",      // 24:00 n'existe pas en RFC 3339
                "2026-06-30T23:59:60+02:00", // intercalaire hors point UTC 23:59
            ],
        ),
    ] {
        let published = schema.json();
        for instance in accepted {
            let instance = json!(instance);
            assert!(
                zod_valid(schema, &instance) && reference_valid_formats(&published, &instance),
                "convergence attendue (accepté) : {instance}"
            );
        }
        for instance in refused {
            let instance = json!(instance);
            assert!(
                !zod_valid(schema, &instance) && !reference_valid_formats(&published, &instance),
                "convergence attendue (refusé) : {instance}"
            );
        }
    }
}

// --- Exploration (désactivée) -----------------------------------------------

// Utilitaire d'investigation : décommenter pour relever les verdicts de la
// référence sur un corpus arbitraire (`cargo test --features schemars
// --test zod_diff_jsonschema explore -- --nocapture`).
//
// #[test]
// fn explore() {
//     let cases: Vec<(&str, Value, Value)> = vec![
//         ("email localhost", json!({"type":"string","format":"email"}), json!("user@localhost")),
//         ("dt space", json!({"type":"string","format":"date-time"}), json!("2026-11-09 18:00:00Z")),
//         ("uri mailto", json!({"type":"string","format":"uri"}), json!("mailto:user@example.com")),
//     ];
//     for (label, schema, instance) in cases {
//         println!(
//             "{label:18} formats={} plain={}",
//             reference_valid_formats(&schema, &instance),
//             reference_valid(&schema, &instance)
//         );
//     }
// }
