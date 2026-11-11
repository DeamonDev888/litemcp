//! Tests de régression du 2026-11-08 — surfaces de contenu MCP et capacités.
//!
//! Formes fil vérifiées contre le schéma officiel MCP 2026-07-28
//! (`draft.schema.json` de la suite modelcontextprotocol/conformance) :
//!
//! - `CallToolResult.content[]` : blocs multiples et mixtes — `text`,
//!   `image`, `audio`, `resource_link`, `resource` (ressource embarquée) ;
//! - `PromptMessage.content` : un `ContentBlock` complet (image, ressource
//!   embarquée) — la forme texte historique reste identique ;
//! - `resources/read` : `contents[]` textuel **ou** binaire (`blob`
//!   base64, `BlobResourceContents`) — jamais un texte qui simule le
//!   binaire ;
//! - `completion/complete` : `CompleteResult` complet (références prompt /
//!   gabarit, troncature à 100, erreurs) ;
//! - `resources/templates/list` : catalogue réel (plus de stub vide) ;
//! - capacités : `completions` annoncée seulement avec fournisseur.

use litemcp::protocol::{
    AudioContent, CallToolResult, CompleteReference, Completion, CompletionRequest, ContentBlock,
    ImageContent, PromptMessage, ResourceContents, ResourceData,
};
use litemcp::{CallEvent, CallStatus, ServerBuilder, ServerCore};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};

// --- Harnais dispatch (sans transport) — profil strict 2026-07-28 ---

fn params_with_meta(mut params: Value) -> Value {
    params
        .as_object_mut()
        .expect("params doit être un objet")
        .insert(
            "_meta".into(),
            json!({
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
            }),
        );
    params
}

fn dispatch(core: &ServerCore, id: i64, method: &str, params: Value) -> Value {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params_with_meta(params),
    });
    let reply = core
        .dispatch(serde_json::to_string(&body).unwrap().as_bytes())
        .expect("une requête identifiée reçoit une réponse");
    serde_json::from_slice(&reply).expect("réponse JSON-RPC lisible")
}

fn call_tool(core: &ServerCore, id: i64, name: &str) -> Value {
    dispatch(
        core,
        id,
        "tools/call",
        json!({ "name": name, "arguments": {} }),
    )
}

// ===========================================================================
// Contenus d'outil — formes fil exactes de chaque variante
// ===========================================================================

/// Outil à contenus mixtes — un bloc de chaque variante, sérialisés sur le
/// fil dans l'ordre, chacun exactement à sa forme de schéma.
fn core_with_mixed_tool() -> ServerCore {
    ServerBuilder::new("contents", "0.1.0")
        .tool_result("rapport", "Rapport mixte", |_args| {
            Ok(CallToolResult::from_content([
                ContentBlock::text("Rapport prêt"),
                ContentBlock::image_bytes(&[0xDE, 0xAD, 0xBE, 0xEF], "image/png"),
                ContentBlock::audio_bytes(&[0x00, 0x01], "audio/wav"),
                ContentBlock::resource_link("mem://doc", "Doc"),
                ContentBlock::embedded_text(
                    "mem://note",
                    Some("text/plain".into()),
                    "Embedded resource content.",
                ),
            ]))
        })
        .core()
}

#[test]
fn mixed_tool_content_has_the_exact_wire_shape_of_every_variant() {
    let core = core_with_mixed_tool();
    let v = call_tool(&core, 1, "rapport");
    assert!(v.get("error").is_none(), "appel doit réussir : {v}");
    assert_eq!(v["result"]["resultType"], "complete", "{v}");
    // Pas de structuredContent : un résultat de contenu n'en déclare pas.
    assert!(
        v["result"].get("structuredContent").is_none(),
        "from_content ne doit pas inventer de structuredContent : {v}"
    );
    assert_eq!(
        v["result"]["content"],
        json!([
            { "type": "text", "text": "Rapport prêt" },
            { "type": "image", "data": "3q2+7w==", "mimeType": "image/png" },
            { "type": "audio", "data": "AAE=", "mimeType": "audio/wav" },
            { "type": "resource_link", "uri": "mem://doc", "name": "Doc" },
            { "type": "resource", "resource": { "uri": "mem://note", "mimeType": "text/plain", "text": "Embedded resource content." } },
        ]),
        "chaque bloc doit être à sa forme de schéma exacte : {v}"
    );
    // Le champ content est OBLIGATOIRE (même l'ordre des champs du premier
    // bloc est verrouillé : type, puis charge utile).
    let first = &v["result"]["content"][0];
    assert_eq!(first["type"], "text");
}

#[test]
fn binary_encoding_never_simulates_text() {
    // base64 exact (vecteur connu), octets réels encodés une fois.
    let image = ImageContent::from_bytes(&[0xDE, 0xAD, 0xBE, 0xEF], "image/png");
    assert_eq!(image.data, "3q2+7w==");
    let audio = AudioContent::from_bytes(&[], "audio/wav");
    assert_eq!(audio.data, "", "octets vides : base64 vide, pas de texte");
    // Un bloc embarqué binaire porte `blob`, jamais `text`.
    let embedded = ContentBlock::embedded_binary(
        "test://bin",
        Some("application/octet-stream".into()),
        &[0xFF, 0xFE],
    );
    let v = serde_json::to_value(&embedded).unwrap();
    assert_eq!(
        v,
        json!({
            "type": "resource",
            "resource": { "uri": "test://bin", "mimeType": "application/octet-stream", "blob": "//4=" }
        })
    );
    assert!(v["resource"].get("text").is_none());
}

#[test]
fn empty_content_list_is_a_required_empty_array() {
    let core = ServerBuilder::new("vide", "0.1.0")
        .tool_result("rien", "Sans contenu", |_args| {
            Ok(CallToolResult::from_content(Vec::<ContentBlock>::new()))
        })
        .core();
    let v = call_tool(&core, 1, "rien");
    assert_eq!(v["result"]["content"], json!([]), "{v}");
    assert_eq!(v["result"]["resultType"], "complete", "{v}");
}

#[test]
fn text_shorthand_and_from_impls_stay_ergonomic() {
    // `From<&str>`, `From<String>`, `From<TextContent>` et les paires
    // `From<…> for ContentBlock` / `for CallToolResult` gardent l'écriture
    // des handlers courte.
    let blocks: Vec<ContentBlock> = vec!["texte brut".into(), String::from("texte possédé").into()];
    assert_eq!(blocks.len(), 2);
    let result: CallToolResult = blocks.into();
    assert_eq!(result.content.len(), 2);
    let single: CallToolResult = ContentBlock::text("seul").into();
    assert_eq!(single.content.len(), 1);
}

/// Le chemin historique (`tool` + `Ok(Value)`) reste **identique** sur le
/// fil : `structuredContent` avec l'enveloppe habituelle, texte reflétant
/// exactement la valeur envoyée — régression de compatibilité.
#[test]
fn legacy_value_path_wire_shape_is_unchanged() {
    let core = ServerBuilder::new("calc", "1.0.0")
        .tool("add", "Additionne", |args: &Value| {
            let a = args["a"].as_i64().ok_or("a manquant")?;
            let b = args["b"].as_i64().ok_or("b manquant")?;
            Ok(json!({ "somme": a + b }))
        })
        .core();
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": params_with_meta(json!({ "name": "add", "arguments": { "a": 5, "b": 7 } })),
    });
    let reply = String::from_utf8(
        core.dispatch(serde_json::to_string(&body).unwrap().as_bytes())
            .unwrap(),
    )
    .unwrap();
    // Les octets du fil (pas une re-sérialisation d'un `Value`, qui
    // réordonnerait les champs) : la forme historique reste intangible,
    // ordre des champs compris. Les `\"` du champ text sont réels : le
    // texte porte la sérialisation JSON de la valeur structurée.
    assert_eq!(
        reply,
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","content":[{"type":"text","text":"{\"somme\":12}"}],"#,
            r#""structuredContent":{"somme":12},"_meta":{"io.modelcontextprotocol/serverInfo":{"name":"calc","version":"1.0.0"}}}}"#,
        ),
        "forme fil historique intangible : {reply}"
    );
}

/// Un résultat riche peut porter **à la fois** des blocs et un
/// `structuredContent` explicite — les deux voyagent tels quels.
#[test]
fn rich_result_keeps_structured_content_alongside_blocks() {
    let core = ServerBuilder::new("mixte", "0.1.0")
        .tool_result("deux", "Deux canaux", |_args| {
            Ok(CallToolResult {
                content: vec![ContentBlock::text("Détail :")],
                ..CallToolResult::from_structured(json!({ "total": 3 }))
            })
        })
        .core();
    let v = call_tool(&core, 1, "deux");
    assert_eq!(v["result"]["content"][0]["text"], "Détail :", "{v}");
    assert_eq!(
        v["result"]["structuredContent"],
        json!({ "total": 3 }),
        "{v}"
    );
}

/// Un `isError: true` construit par le handler est rapporté aux hooks
/// comme un échec d'exécution — et part sur le fil tel quel.
#[test]
fn rich_error_result_maps_to_a_tool_failure_status() {
    let failures = std::sync::Arc::new(AtomicUsize::new(0));
    let hook_failures = std::sync::Arc::clone(&failures);
    let core = ServerBuilder::new("echec", "0.1.0")
        .tool_result("rate", "Échec choisi", |_args| {
            Ok(CallToolResult {
                is_error: Some(true),
                ..CallToolResult::from_content([ContentBlock::text("échec métier")])
            })
        })
        .on_call(move |event: &CallEvent<'_>| {
            if matches!(event.status, CallStatus::ToolFailure(_)) {
                hook_failures.fetch_add(1, Ordering::AcqRel);
            }
        })
        .core();
    let v = call_tool(&core, 1, "rate");
    assert_eq!(v["result"]["isError"], true, "{v}");
    assert_eq!(
        failures.load(Ordering::Acquire),
        1,
        "le hook doit voir un ToolFailure"
    );
}

/// L'`outputSchema` reste exécuteur sur un `structuredContent` explicite du
/// chemin riche — non conforme → résultat `isError` (comme le chemin Value).
#[test]
fn rich_structured_output_is_still_schema_enforced() {
    let core = ServerBuilder::new("garde", "0.1.0")
        .tool_result("hors_schema", "Sortie non conforme", |_args| {
            Ok(CallToolResult::from_structured(
                json!({ "n": "pas-un-nombre" }),
            ))
        })
        .output_schema(
            "hors_schema",
            litemcp::zod::object().field("n", litemcp::zod::integer()),
        )
        .core();
    let v = call_tool(&core, 1, "hors_schema");
    assert_eq!(v["result"]["isError"], true, "{v}");
    assert!(
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("outputSchema"),
        "l'échec cite le contrat de sortie : {v}"
    );
}

// ===========================================================================
// Prompts — texte, image, ressource embarquée
// ===========================================================================

#[test]
fn prompt_messages_accept_rich_content_blocks() {
    let core = ServerBuilder::new("prompts", "0.1.0")
        .prompt("visuel", "Prompt avec image et ressource", |_args| {
            Ok(vec![
                PromptMessage::user_content(ContentBlock::image_bytes(
                    &[0x89, 0x50, 0x4E, 0x47],
                    "image/png",
                )),
                PromptMessage::user_content(ContentBlock::embedded_text(
                    "test://example-resource",
                    Some("text/plain".into()),
                    "Embedded resource content for testing.",
                )),
                PromptMessage::assistant_content(ContentBlock::resource_link("mem://doc", "Doc")),
                // La forme texte historique reste disponible et identique.
                PromptMessage::user("Please analyze the image above."),
            ])
        })
        .core();
    let v = dispatch(
        &core,
        1,
        "prompts/get",
        json!({ "name": "visuel", "arguments": {} }),
    );
    assert!(v.get("error").is_none(), "prompts/get doit réussir : {v}");
    assert_eq!(
        v["result"]["messages"][0]["content"],
        json!({ "type": "image", "data": "iVBORw==", "mimeType": "image/png" }),
        "{v}"
    );
    assert_eq!(
        v["result"]["messages"][1]["content"],
        json!({
            "type": "resource",
            "resource": { "uri": "test://example-resource", "mimeType": "text/plain", "text": "Embedded resource content for testing." }
        }),
        "{v}"
    );
    assert_eq!(
        v["result"]["messages"][2]["content"],
        json!({ "type": "resource_link", "uri": "mem://doc", "name": "Doc" }),
        "{v}"
    );
    // Régression : un message texte s'émet exactement comme avant.
    assert_eq!(
        v["result"]["messages"][3]["content"],
        json!({ "type": "text", "text": "Please analyze the image above." }),
        "{v}"
    );
}

// ===========================================================================
// resources/read — texte (régression) ou blob
// ===========================================================================

#[test]
fn resources_read_serves_a_real_blob_never_a_text_proxy() {
    let core = ServerBuilder::new("assets", "0.1.0")
        .resource_data("test://static-binary", "Binaire", "PNG de test", || {
            Ok(ResourceData::binary(
                "test://static-binary",
                Some("image/png".into()),
                &[0x89, 0x50, 0x4E, 0x47],
            ))
        })
        .core();
    let v = dispatch(
        &core,
        1,
        "resources/read",
        json!({ "uri": "test://static-binary" }),
    );
    assert!(v.get("error").is_none(), "lecture doit réussir : {v}");
    assert_eq!(
        v["result"]["contents"],
        json!([{ "uri": "test://static-binary", "mimeType": "image/png", "blob": "iVBORw==" }]),
        "forme BlobResourceContents exacte : {v}"
    );
    assert!(v["result"]["contents"][0].get("text").is_none());
    // Indices de cache SEP-2549 comme pour une lecture texte.
    assert_eq!(v["result"]["ttlMs"], 60_000, "{v}");
    assert_eq!(v["result"]["cacheScope"], "private", "{v}");
}

/// Le handler de `resource_data` peut choisir texte ou binaire à l'exécution.
#[test]
fn resource_data_handler_chooses_the_shape_at_runtime() {
    let core = ServerBuilder::new("mixte", "0.1.0")
        .resource_data("app://doc", "Doc", "Texte ou binaire selon l'état", || {
            // Même handler : le choix est fait à chaque lecture — ici le
            // chemin texte, via From<ResourceContents>.
            Ok(ResourceContents {
                uri: "app://doc".into(),
                mime_type: Some("text/plain".into()),
                text: "texte simple".into(),
            }
            .into())
        })
        .core();
    let v = dispatch(&core, 1, "resources/read", json!({ "uri": "app://doc" }));
    assert_eq!(
        v["result"]["contents"],
        json!([{ "uri": "app://doc", "mimeType": "text/plain", "text": "texte simple" }]),
        "{v}"
    );
    assert!(v["result"]["contents"][0].get("blob").is_none());
}

#[test]
fn resources_read_text_shape_is_unchanged() {
    // Régression : la lecture texte (API historique `.resource`) garde sa
    // forme fil exacte — aucun champ `blob` inventé.
    let core = ServerBuilder::new("docs", "0.1.0")
        .resource("mem://readme", "README", "La documentation", || {
            Ok(ResourceContents {
                uri: "mem://readme".into(),
                mime_type: Some("text/markdown".into()),
                text: "# Titre".into(),
            })
        })
        .core();
    let v = dispatch(&core, 1, "resources/read", json!({ "uri": "mem://readme" }));
    assert_eq!(
        v["result"]["contents"],
        json!([{ "uri": "mem://readme", "mimeType": "text/markdown", "text": "# Titre" }]),
        "{v}"
    );
    assert!(v["result"]["contents"][0].get("blob").is_none());
}

#[test]
fn resource_read_failure_is_still_a_resource_failure() {
    // Régression : un handler de ressource en échec reste un code -32603
    // avec data `handler_failed` (défaillance interne — MCP 2026-07-28
    // déconseille d'introduire de nouveaux codes en -32000).
    let core = ServerBuilder::new("hs", "0.1.0")
        .resource_data("app://hs", "HS", "Toujours en échec", || {
            Err("backend unreachable".to_string())
        })
        .core();
    let v = dispatch(&core, 1, "resources/read", json!({ "uri": "app://hs" }));
    assert_eq!(v["error"]["code"], -32603, "{v}");
    assert_eq!(v["error"]["data"]["reason"], "handler_failed", "{v}");
}

// ===========================================================================
// resources/templates/list — catalogue réel
// ===========================================================================

#[test]
fn resource_templates_are_listed_with_their_exact_wire_shape() {
    let core = ServerBuilder::new("tpl", "0.1.0")
        .resource_template(
            "test://template/{id}/data",
            "Données",
            "Données par identifiant",
        )
        .core();
    let v = dispatch(&core, 1, "resources/templates/list", json!({}));
    assert!(
        v.get("error").is_none(),
        "templates/list doit réussir : {v}"
    );
    assert_eq!(
        v["result"]["resourceTemplates"],
        json!([{
            "uriTemplate": "test://template/{id}/data",
            "name": "Données",
            "description": "Données par identifiant",
        }]),
        "forme ResourceTemplate exacte (uriTemplate en camelCase) : {v}"
    );
    assert_eq!(v["result"]["resultType"], "complete", "{v}");
    assert_eq!(v["result"]["ttlMs"], 60_000, "{v}");
    assert_eq!(v["result"]["cacheScope"], "private", "{v}");
    assert!(
        v["result"]["_meta"]["io.modelcontextprotocol/serverInfo"].is_object(),
        "{v}"
    );
}

#[test]
fn resource_template_registration_can_be_removed() {
    let mut builder =
        ServerBuilder::new("tpl", "0.1.0").resource_template("test://a/{id}", "A", "A");
    assert!(builder.remove_resource_template("test://a/{id}"));
    assert!(!builder.remove_resource_template("test://a/{id}"));
    let core = builder.core();
    let v = dispatch(&core, 1, "resources/templates/list", json!({}));
    assert_eq!(v["result"]["resourceTemplates"], json!([]), "{v}");
}

#[test]
fn templates_are_covered_by_the_resources_capability() {
    // Un serveur qui n'expose QUE des gabarits sert bien `resources` —
    // le gabarit vit sous la capacité resources (pas de sous-capacité
    // dédiée dans le schéma 2026-07-28).
    let core = ServerBuilder::new("tpl", "0.1.0")
        .resource_template("test://tpl/{id}", "T", "T")
        .core();
    let v = dispatch(&core, 1, "server/discover", json!({}));
    assert!(
        v["result"]["capabilities"]["resources"].is_object(),
        "resources annoncé pour un serveur à templates : {v}"
    );
}

// ===========================================================================
// completion/complete — matrice complète
// ===========================================================================

fn core_with_completions() -> ServerCore {
    ServerBuilder::new("comp", "0.1.0")
        .prompt("test_prompt_with_arguments", "Prompt de test", |_args| {
            Ok(vec![])
        })
        .completions(|request: &CompletionRequest| {
            match &request.reference {
                CompleteReference::Prompt { name, .. } if name == "test_prompt_with_arguments" => {
                    // Vérifie la transmission du contexte et de l'argument.
                    assert_eq!(request.argument.name, "arg1");
                    assert_eq!(request.argument.value, "par");
                    assert!(request.context.is_empty());
                    Ok(Completion::new(vec!["paris".into(), "park".into()]).total(150))
                }
                CompleteReference::Resource { uri } => {
                    assert_eq!(uri, "test://template/{id}/data");
                    assert_eq!(request.context.get("id").map(String::as_str), Some("123"));
                    Ok(Completion::new(vec![format!("{uri} => 123")]))
                }
                _ => Ok(Completion::new(vec![])),
            }
        })
        .core()
}

#[test]
fn completion_serves_prompt_references() {
    let core = core_with_completions();
    let v = dispatch(
        &core,
        1,
        "completion/complete",
        json!({
            "ref": { "type": "ref/prompt", "name": "test_prompt_with_arguments" },
            "argument": { "name": "arg1", "value": "par" },
        }),
    );
    assert!(v.get("error").is_none(), "completion doit réussir : {v}");
    assert_eq!(v["result"]["resultType"], "complete", "{v}");
    assert_eq!(
        v["result"]["completion"],
        json!({ "values": ["paris", "park"], "total": 150 }),
        "forme CompleteResult exacte (total publié, hasMore omis) : {v}"
    );
    assert!(
        v["result"]["_meta"]["io.modelcontextprotocol/serverInfo"].is_object(),
        "{v}"
    );
}

#[test]
fn completion_serves_resource_template_references_with_context() {
    let core = core_with_completions();
    let v = dispatch(
        &core,
        1,
        "completion/complete",
        json!({
            "ref": { "type": "ref/resource", "uri": "test://template/{id}/data" },
            "argument": { "name": "id", "value": "12" },
            "context": { "arguments": { "id": "123" } },
        }),
    );
    assert!(v.get("error").is_none(), "completion doit réussir : {v}");
    assert_eq!(
        v["result"]["completion"]["values"],
        json!(["test://template/{id}/data => 123"]),
        "{v}"
    );
}

#[test]
fn completion_values_over_100_are_truncated_with_has_more() {
    let core = ServerBuilder::new("comp", "0.1.0")
        .completions(|_request| Ok(Completion::new((0..150).map(|i| format!("v{i}")).collect())))
        .core();
    let v = dispatch(
        &core,
        1,
        "completion/complete",
        json!({
            "ref": { "type": "ref/prompt", "name": "p" },
            "argument": { "name": "a", "value": "v" },
        }),
    );
    let values = v["result"]["completion"]["values"].as_array().unwrap();
    assert_eq!(values.len(), 100, "borne de la spec : {v}");
    assert_eq!(values[0], "v0");
    assert_eq!(values[99], "v99");
    assert_eq!(v["result"]["completion"]["hasMore"], true, "{v}");
}

#[test]
fn completion_accepts_empty_values() {
    // Le scénario officiel précise que « minimal or empty arrays » suffit :
    // un fournisseur qui renvoie zéro suggestion réussit.
    let core = ServerBuilder::new("comp", "0.1.0")
        .completions(|_request| Ok(Completion::new(vec![])))
        .core();
    let v = dispatch(
        &core,
        1,
        "completion/complete",
        json!({
            "ref": { "type": "ref/prompt", "name": "p" },
            "argument": { "name": "a", "value": "x" },
        }),
    );
    assert_eq!(v["result"]["completion"], json!({ "values": [] }), "{v}");
}

#[test]
fn completion_provider_failure_is_a_resource_failure() {
    // Convention du noyau : échec de handler ressource/prompt/complétion
    // → -32603 avec data structuré (`reason: handler_failed`).
    let core = ServerBuilder::new("comp", "0.1.0")
        .completions(|_request| Err("index unavailable".to_string()))
        .core();
    let v = dispatch(
        &core,
        1,
        "completion/complete",
        json!({
            "ref": { "type": "ref/prompt", "name": "p" },
            "argument": { "name": "a", "value": "x" },
        }),
    );
    assert_eq!(v["error"]["code"], -32603, "{v}");
    assert_eq!(v["error"]["data"]["reason"], "handler_failed", "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Completion failed"),
        "{v}"
    );
}

#[test]
fn completion_validates_params_precisely() {
    let core = ServerBuilder::new("comp", "0.1.0")
        .completions(|_request| Ok(Completion::new(vec![])))
        .core();
    // ref absente.
    let v = dispatch(
        &core,
        1,
        "completion/complete",
        json!({ "argument": { "name": "a", "value": "x" } }),
    );
    assert_eq!(v["error"]["code"], -32602, "{v}");
    // ref.type inconnu.
    let v = dispatch(
        &core,
        2,
        "completion/complete",
        json!({
            "ref": { "type": "ref/inconnu", "name": "p" },
            "argument": { "name": "a", "value": "x" },
        }),
    );
    assert_eq!(v["error"]["code"], -32602, "{v}");
    // ref/prompt sans name.
    let v = dispatch(
        &core,
        3,
        "completion/complete",
        json!({
            "ref": { "type": "ref/prompt" },
            "argument": { "name": "a", "value": "x" },
        }),
    );
    assert_eq!(v["error"]["code"], -32602, "{v}");
    // ref/resource sans uri.
    let v = dispatch(
        &core,
        4,
        "completion/complete",
        json!({
            "ref": { "type": "ref/resource" },
            "argument": { "name": "a", "value": "x" },
        }),
    );
    assert_eq!(v["error"]["code"], -32602, "{v}");
    // argument.value manquant.
    let v = dispatch(
        &core,
        5,
        "completion/complete",
        json!({
            "ref": { "type": "ref/prompt", "name": "p" },
            "argument": { "name": "a" },
        }),
    );
    assert_eq!(v["error"]["code"], -32602, "{v}");
    // argument non-objet.
    let v = dispatch(
        &core,
        6,
        "completion/complete",
        json!({
            "ref": { "type": "ref/prompt", "name": "p" },
            "argument": "x",
        }),
    );
    assert_eq!(v["error"]["code"], -32602, "{v}");
    // context.arguments avec valeur non-chaîne.
    let v = dispatch(
        &core,
        7,
        "completion/complete",
        json!({
            "ref": { "type": "ref/prompt", "name": "p" },
            "argument": { "name": "a", "value": "x" },
            "context": { "arguments": { "id": 12 } },
        }),
    );
    assert_eq!(v["error"]["code"], -32602, "{v}");
}

#[test]
fn completions_capability_is_announced_with_a_provider() {
    let core = core_with_completions();
    let v = dispatch(&core, 1, "server/discover", json!({}));
    assert_eq!(
        v["result"]["capabilities"]["completions"],
        json!({}),
        "capacité annoncée quand un fournisseur existe : {v}"
    );
    // Les trois capacités du travail voisin restent annoncées.
    assert!(v["result"]["capabilities"]["tools"].is_object(), "{v}");
    assert!(v["result"]["capabilities"]["resources"].is_object(), "{v}");
    assert!(v["result"]["capabilities"]["prompts"].is_object(), "{v}");
}
