//! Tests de régression des correctifs d'audit (2026-10-24).

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

// --- Correctif 1 : l'outputSchema publié doit être réellement atteignable ---

#[test]
fn scalar_output_schema_is_reachable() {
    // AVANT : le schéma publié était `{"type":"integer"}` mais la valeur
    // était enveloppée dans `{"result": 42}` → l'enveloppe elle-même
    // était non conforme → *aucun* outil à outputSchema non-objet ne
    // pouvait jamais réussir.
    let core = ServerBuilder::new("t", "0.1.0")
        .output_schema("num", zod::integer())
        .tool("num", "renvoie 42", |_| Ok(json!(42)))
        .core();

    let v = call_tool(&core, "num", json!({}));
    assert!(
        v.get("error").is_none(),
        "un outputSchema scalaire doit être atteignable : {v}"
    );
    assert_eq!(v["result"]["structuredContent"], json!(42));
    // Le contenu texte reflète la valeur structurée réellement envoyée.
    assert_eq!(v["result"]["content"][0]["text"], "42");
    assert!(v["result"].get("isError").is_none());
}

#[test]
fn array_output_schema_is_reachable() {
    let core = ServerBuilder::new("t", "0.1.0")
        .output_schema("list", zod::array(zod::integer()))
        .tool("list", "renvoie [1,2]", |_| Ok(json!([1, 2])))
        .core();
    let v = call_tool(&core, "list", json!({}));
    assert!(v.get("error").is_none(), "{v}");
    assert_eq!(v["result"]["structuredContent"], json!([1, 2]));
}

#[test]
fn object_output_schema_keeps_the_invariant_envelope() {
    // Le cas courant : sortie objet → passe tal quel, sans enveloppe.
    let core = ServerBuilder::new("t", "0.1.0")
        .output_schema("obj", zod::object().field("total", zod::integer()))
        .tool("obj", "objet", |_| Ok(json!({ "total": 2 })))
        .core();
    let v = call_tool(&core, "obj", json!({}));
    assert_eq!(v["result"]["structuredContent"], json!({ "total": 2 }));
    assert_eq!(v["result"]["content"][0]["text"], r#"{"total":2}"#);
}

#[test]
fn object_output_schema_with_non_object_value_is_enveloped_and_fails() {
    // Sortie non-objet + schéma objet : l'enveloppe `{result: …}` est
    // non conforme au schéma → isError (comportement correct, et
    // l'enveloppe est tentée).
    let core = ServerBuilder::new("t", "0.1.0")
        .output_schema("obj", zod::object().field("total", zod::integer()))
        .tool("obj", "42", |_| Ok(json!(42)))
        .core();
    let v = call_tool(&core, "obj", json!({}));
    assert_eq!(v["result"]["isError"], json!(true));
    assert!(
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("outputSchema"),
        "{v}"
    );
}

#[test]
fn non_conforming_output_is_still_an_error() {
    let core = ServerBuilder::new("t", "0.1.0")
        .output_schema("obj", zod::object().field("total", zod::integer()))
        .tool("obj", "mauvais", |_| Ok(json!({ "autre": 1 })))
        .core();
    let v = call_tool(&core, "obj", json!({}));
    assert_eq!(v["result"]["isError"], json!(true));
}

// --- Correctif 2 : `format` doit être APPLIQUÉ par l'adaptateur schemars ---

#[cfg(feature = "schemars")]
#[test]
fn schemars_format_is_published_and_enforced() {
    use litemcp::schemars::from_schemars;
    use schemars::{JsonSchema, Schema, SchemaGenerator};
    use std::borrow::Cow;

    /// Type dont le schéma publié porte explicitement un `format`
    /// (schemars 1.2 n'expose pas d'attribut `format` : le document est
    /// construit à la main).
    struct Mail;

    impl JsonSchema for Mail {
        fn schema_name() -> Cow<'static, str> {
            "Mail".into()
        }

        fn schema_id() -> Cow<'static, str> {
            "litemcp:audit:Mail".into()
        }

        fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
            let document = json!({
                "type": "object",
                "properties": { "email": { "type": "string", "format": "email" } },
                "required": ["email"]
            });
            Schema::from(document.as_object().expect("document objet").clone())
        }
    }

    let core = ServerBuilder::new("t", "0.1.0")
        .tool_typed("mail", "mail", from_schemars::<Mail>(), |args| {
            Ok(args.clone())
        })
        .core();

    // Le schéma publié porte bien le `format`.
    let list = call(&core, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let schema = &list["result"]["tools"][0]["inputSchema"];
    assert_eq!(
        schema["properties"]["email"]["format"], "email",
        "le format doit être publié : {list}"
    );

    // Le `format` doit être APPLIQUÉ : `format` est une simple annotation
    // en JSON Schema 2020-12, ignorée par défaut par le validateur —
    // sans activation explicite, le schéma publié mentirait.
    let bad = call_tool(&core, "mail", json!({ "email": "PAS-UN-EMAIL" }));
    assert!(
        bad.get("error").is_some(),
        "un email invalide doit être refusé : {bad}"
    );
    assert_eq!(bad["error"]["code"], -32602);

    // Un email valide passe.
    let good = call_tool(&core, "mail", json!({ "email": "a@b.cd" }));
    assert!(good.get("error").is_none(), "{good}");
}

// --- Correctif 3 : le ré-enregistrement d'un outil ne doit pas perdre
//    annotations ni outputSchema déjà déclarés ---
#[test]
fn reregistration_keeps_annotations_and_output_schema() {
    let core = ServerBuilder::new("t", "0.1.0")
        .tool("x", "v1", |_| Ok(json!({ "a": 1 })))
        .annotate("x", ToolAnnotations::new().title("Titre").read_only(true))
        .output_schema("x", zod::object().field("a", zod::integer()))
        // Ré-enregistrement : le ToolDef est remplacé.
        .tool("x", "v2", |_| Ok(json!({ "a": 1 })))
        .core();

    let list = call(&core, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let tool = &list["result"]["tools"][0];
    assert_eq!(tool["description"], "v2", "la description suit le dernier");
    assert_eq!(
        tool["annotations"]["readOnlyHint"],
        json!(true),
        "les annotations doivent survivre au ré-enregistrement : {list}"
    );
    assert!(
        tool.get("outputSchema").is_some(),
        "l'outputSchema doit survivre au ré-enregistrement : {list}"
    );
}

// --- Correctif 4 : les notifications d'action sont REFUSÉES explicitement ---

#[test]
fn action_notification_is_refused_explicitly() {
    // JSON-RPC 2.0 §4 impose de traiter une notification, mais le mode
    // stateless ne peut pas livrer le résultat : le handler ne doit
    // surtout PAS tourner en silence. Refus explicite (-32600).
    let core = ServerBuilder::new("t", "0.1.0")
        .tool("x", "x", |_| Ok(json!({})))
        .core();

    let v = call(
        &core,
        r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"x"}}"#,
    );
    assert_eq!(
        v["error"]["code"], -32600,
        "INVALID_REQUEST : le refus est une requête invalide — {v}"
    );
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("notification refusée"),
        "le message doit expliciter le refus : {v}"
    );
    assert!(
        v.get("result").is_none(),
        "aucun résultat ne doit être produit : {v}"
    );
}

#[test]
fn protocol_notification_is_still_accepted_and_ignored() {
    // Les notifications `notifications/*` sont des messages de contrôle :
    // acceptées et ignorées (202 sans corps) — comportement inchangé.
    let core = ServerBuilder::new("t", "0.1.0")
        .tool("x", "x", |_| Ok(json!({})))
        .core();

    for method in [
        "notifications/initialized",
        "notifications/cancelled",
        "notifications/progress",
    ] {
        let body = format!(r#"{{"jsonrpc":"2.0","method":"{method}"}}"#);
        assert!(
            core.dispatch(body.as_bytes()).is_none(),
            "{method} doit rester acceptée et ignorée (aucune réponse)"
        );
    }
}

// --- Correctif 5 : `Box<dyn Schema>` et `Arc<dyn Schema>` sont des schémas ---

#[test]
fn boxed_and_arc_schemas_are_usable() {
    // Sans ces implémentations, impossible de stocker des schémas
    // hétérogènes (usage dynamique) — `Box<dyn Schema>` ne satisfait pas
    // `S: Schema`.
    // Un schéma encapsulé est utilisable : `Box<dyn Schema>` satisfait
    // désormais `S: Schema` (avant : wrapper manuel obligatoire).
    let concrete = zod::object().field("a", zod::integer());
    let boxed: Box<dyn zod::Schema> = Box::new(concrete);
    let core = ServerBuilder::new("t", "0.1.0")
        .output_schema("x", boxed)
        .tool("x", "x", |_| Ok(json!({ "a": 1 })))
        .core();

    let list = call(&core, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let tools = list["result"]["tools"].as_array().unwrap();
    assert!(
        tools[0].get("outputSchema").is_some(),
        "un schéma encapsulé doit être publiable : {list}"
    );
    let v = call_tool(&core, "x", json!({}));
    assert!(v.get("error").is_none(), "{v}");

    // Un registre hétérogène (usage dynamique : schémas d'une config,
    // d'une base…) est désormais possible — chaque schéma est possédé
    // (`'static` requis par l'API).
    let schemas: Vec<Box<dyn zod::Schema>> = vec![
        Box::new(zod::object().field("a", zod::integer())),
        Box::new(zod::string().min(2)),
        Box::new(zod::array(zod::integer())),
    ];
    assert_eq!(schemas.len(), 3);
    for schema in &schemas {
        // Chaque `Box<dyn Schema>` est utilisable comme `impl Schema`.
        let json = schema.json();
        assert!(json.is_object(), "un schéma encapsulé se publie : {json}");
    }

    // Un `Arc<dyn Schema>` aussi.
    let shared: std::sync::Arc<dyn zod::Schema> =
        std::sync::Arc::new(zod::object().field("a", zod::integer()));
    let core2 = ServerBuilder::new("t", "0.1.0")
        .output_schema("x", shared)
        .tool("x", "x", |_| Ok(json!({ "a": 1 })))
        .core();
    let v2 = call_tool(&core2, "x", json!({}));
    assert!(v2.get("error").is_none(), "{v2}");
}
