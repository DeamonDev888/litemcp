//! Fixture de conformance officielle MCP **2026-07-28** — construction du
//! serveur portant les objets `test_*` attendus par la suite
//! `modelcontextprotocol/conformance` (SEP-1730).
//!
//! Ce module est **partagé** : il construit le binaire
//! `examples/conformance_server` (via `main.rs`) et les tests de régression
//! `tests/conformance_fixture.rs`, `tests/conformance_fixture_mrtr.rs` et
//! `tests/mcp_progress_channel.rs` (inclusion `#[path]`) — une seule source
//! de vérité pour la surface testée par la suite.
//!
//! Un résultat obtenu contre cette fixture n'est un score de conformité que
//! si la suite est invoquée avec `--requirements 2026-07-28` (liste scorée
//! figée par le fichier `requirements/2026-07-28.yaml` du harness, version
//! `@modelcontextprotocol/conformance@0.2.0-alpha.12`).
//!
//! La fixture n'utilise que l'API publique de LiteMCP (`.tool_result`,
//! `.tool_mrtr`/`.prompt_mrtr`/`.resource_mrtr`, `.prompt`, `.resource`/
//! `.resource_data`, `.resource_template`, `.completions`,
//! `.require_client_capability`) — aucun contournement interne, aucune
//! modification de la suite pour la faire passer.
//!
//! La feature opt-in `schemars` ajoute `json_schema_2020_12_tool` (scénario
//! *pending*, non scoré) : un document JSON Schema 2020-12 brut publié tel
//! quel et validé par le cœur. Le workflow de conformance construit donc la
//! fixture avec `--features schemars` ; un build par défaut ne l'annonce pas.

use litemcp::prelude::*;
use litemcp::protocol::{
    CallToolResult, CompleteReference, Completion, CompletionRequest, ContentBlock,
    InputRequiredSpec, PromptMessage, ResourceContents, ResourceData,
};
use litemcp::server::{InputRound, PromptOutcome, ResourceOutcome, ToolOutcome};
use serde_json::{Value, json};

/// PNG 1×1 pixel **rouge** (8 bits RGB) — la suite n'en vérifie que la forme
/// (`data` base64 + `mimeType`), mais autant lui donner un vrai PNG valide.
const PNG_1X1_RED: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
    0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8, 0xCF, 0xC0, 0x00,
    0x00, 0x03, 0x01, 0x01, 0x00, 0xC9, 0xFE, 0x92, 0xEF, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E,
    0x44, 0xAE, 0x42, 0x60, 0x82,
];

/// WAV minimal valide (PCM 16 bits mono 8 kHz, 80 échantillons de silence).
const WAV_SILENCE: &[u8] = &[
    0x52, 0x49, 0x46, 0x46, 0xC4, 0x00, 0x00, 0x00, 0x57, 0x41, 0x56, 0x45, 0x66, 0x6D, 0x74, 0x20,
    0x10, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x40, 0x1F, 0x00, 0x00, 0x80, 0x3E, 0x00, 0x00,
    0x02, 0x00, 0x10, 0x00, 0x64, 0x61, 0x74, 0x61, 0xA0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

// ===== Handlers MRTR (SEP-2322) =====
//
// Un handler `*_mrtr` est rappelé à chaque tour : le noyau lui remet les
// réponses du client (`round.response`/`round.accepted`, non fiables) et
// l'état vérifié du `requestState` (`round.state`), jamais un état de
// connexion. Les noms et les formes de requêtes ci-dessous sont ceux que la
// suite officielle envoie ; toute divergence est visible au run.

/// Schéma d'élicitation minimal (objet avec une propriété requise).
fn elicit_schema(property: &str) -> Value {
    json!({
        "type": "object",
        "properties": { property: { "type": "string" } },
        "required": [property],
    })
}

/// `input-required-result-basic-elicitation` et ses scénarios dérivés :
/// une élicitation `user_name`, puis le salut reprenant la valeur acceptée.
/// Sans réponse `user_name` (clé absente ou refusée) le handler redemande
/// l'entrée — c'est ce que `input-required-result-missing-input-response`
/// attend (jamais une erreur JSON-RPC).
fn mrtr_elicitation(_args: &Value, round: &InputRound) -> Result<ToolOutcome, ToolError> {
    if let Some(content) = round.accepted("user_name") {
        let name = content
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("there");
        return Ok(ToolOutcome::Result(CallToolResult::from_content([
            ContentBlock::text(format!("Hello, {name}!")),
        ])));
    }
    Ok(ToolOutcome::InputRequired(InputRequiredSpec::new().elicit(
        "user_name",
        "What is your name?",
        elicit_schema("name"),
    )))
}

/// `input-required-result-basic-sampling` : une requête d'échantillonnage,
/// puis le texte restitué par le client.
fn mrtr_sampling(_args: &Value, round: &InputRound) -> Result<ToolOutcome, ToolError> {
    if let Some(text) = round
        .response("capital_question")
        .and_then(|response| response.get("content"))
        .and_then(|content| content.get("text"))
        .and_then(Value::as_str)
    {
        return Ok(ToolOutcome::Result(CallToolResult::from_content([
            ContentBlock::text(format!("Sampling answer: {text}")),
        ])));
    }
    Ok(ToolOutcome::InputRequired(
        InputRequiredSpec::new().create_message(
            "capital_question",
            json!({
                "messages": [{
                    "role": "user",
                    "content": { "type": "text", "text": "What is the capital of France?" },
                }],
                "maxTokens": 100,
            }),
        ),
    ))
}

/// `input-required-result-basic-list-roots` : une demande `roots/list`,
/// puis un résultat qui cite les racines reçues.
fn mrtr_list_roots(_args: &Value, round: &InputRound) -> Result<ToolOutcome, ToolError> {
    if let Some(roots) = round
        .response("client_roots")
        .and_then(|response| response.get("roots"))
        .and_then(Value::as_array)
    {
        let uris: Vec<&str> = roots
            .iter()
            .filter_map(|root| root.get("uri").and_then(Value::as_str))
            .collect();
        return Ok(ToolOutcome::Result(CallToolResult::from_content([
            ContentBlock::text(format!("Client roots: {}", uris.join(", "))),
        ])));
    }
    Ok(ToolOutcome::InputRequired(
        InputRequiredSpec::new().list_roots("client_roots"),
    ))
}

/// `input-required-result-request-state` : l'état signé porte la phase ;
/// le texte final doit contenir `state-ok` une fois l'état vérifié par le
/// noyau et restitué au handler.
fn mrtr_request_state(_args: &Value, round: &InputRound) -> Result<ToolOutcome, ToolError> {
    let validated = round
        .state
        .as_ref()
        .and_then(|state| state.get("phase"))
        .and_then(Value::as_str)
        == Some("confirm");
    if let (true, Some(content)) = (validated, round.accepted("confirm")) {
        let confirmed = content.get("ok").and_then(Value::as_bool).unwrap_or(false);
        return Ok(ToolOutcome::Result(CallToolResult::from_content([
            ContentBlock::text(format!("state-ok (confirmed={confirmed})")),
        ])));
    }
    Ok(ToolOutcome::InputRequired(
        InputRequiredSpec::new()
            .elicit(
                "confirm",
                "Please confirm",
                json!({
                    "type": "object",
                    "properties": { "ok": { "type": "boolean" } },
                    "required": ["ok"],
                }),
            )
            .with_state(json!({ "phase": "confirm" })),
    ))
}

/// `input-required-result-multiple-input-requests` : trois requêtes
/// embarquées de méthodes différentes dans un seul résultat.
fn mrtr_multiple_inputs(_args: &Value, round: &InputRound) -> Result<ToolOutcome, ToolError> {
    let ready = ["user_name", "greeting", "client_roots"]
        .iter()
        .all(|key| round.response(key).is_some());
    if ready {
        let name = round
            .accepted("user_name")
            .and_then(|content| content.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("there");
        return Ok(ToolOutcome::Result(CallToolResult::from_content([
            ContentBlock::text(format!("All inputs received for {name}.")),
        ])));
    }
    Ok(ToolOutcome::InputRequired(
        InputRequiredSpec::new()
            .elicit("user_name", "What is your name?", elicit_schema("name"))
            .create_message(
                "greeting",
                json!({
                    "messages": [{
                        "role": "user",
                        "content": { "type": "text", "text": "Generate a greeting" },
                    }],
                    "maxTokens": 50,
                }),
            )
            .list_roots("client_roots")
            .with_state(json!({ "phase": "multiple" })),
    ))
}

/// `input-required-result-multi-round` : le tour vit dans l'état signé ;
/// chaque tour émet un `requestState` différent (le scénario le vérifie).
fn mrtr_multi_round(_args: &Value, round: &InputRound) -> Result<ToolOutcome, ToolError> {
    let step = round
        .state
        .as_ref()
        .and_then(|state| state.get("step"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    match step {
        0 => Ok(ToolOutcome::InputRequired(
            InputRequiredSpec::new()
                .elicit("step1", "Step 1: What is your name?", elicit_schema("name"))
                .with_state(json!({ "step": 1 })),
        )),
        1 => {
            let name = round
                .accepted("step1")
                .and_then(|content| content.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("there");
            Ok(ToolOutcome::InputRequired(
                InputRequiredSpec::new()
                    .elicit(
                        "step2",
                        "Step 2: What is your favorite color?",
                        elicit_schema("color"),
                    )
                    .with_state(json!({ "step": 2, "name": name })),
            ))
        }
        _ => {
            let name = round
                .state
                .as_ref()
                .and_then(|state| state.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("there");
            let color = round
                .accepted("step2")
                .and_then(|content| content.get("color"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            Ok(ToolOutcome::Result(CallToolResult::from_content([
                ContentBlock::text(format!("Multi-round complete: {name} likes {color}.")),
            ])))
        }
    }
}

/// `input-required-result-non-tool-request` : `prompts/get` produit aussi
/// un résultat `input_required` (la méthode est supportée par la spec).
fn mrtr_prompt(_args: &Value, round: &InputRound) -> Result<PromptOutcome, String> {
    if let Some(content) = round.accepted("user_context") {
        let context = content
            .get("context")
            .and_then(Value::as_str)
            .unwrap_or("default context");
        return Ok(PromptOutcome::Messages(vec![PromptMessage::user(format!(
            "Prompt context: {context}"
        ))]));
    }
    Ok(PromptOutcome::InputRequired(
        InputRequiredSpec::new().elicit(
            "user_context",
            "What context should the prompt use?",
            elicit_schema("context"),
        ),
    ))
}

/// `input-required-result-tampered-state` : l'état signé est la seule
/// preuve d'un premier tour ; un jeton altéré est refusé par le noyau
/// (`-32602`) avant d'atteindre ce handler.
fn mrtr_tampered_state(_args: &Value, round: &InputRound) -> Result<ToolOutcome, ToolError> {
    let sealed = round
        .state
        .as_ref()
        .and_then(|state| state.get("sealed"))
        .and_then(Value::as_bool)
        == Some(true);
    if sealed {
        return Ok(ToolOutcome::Result(CallToolResult::from_content([
            ContentBlock::text("tampered-state: verified state accepted"),
        ])));
    }
    Ok(ToolOutcome::InputRequired(
        InputRequiredSpec::new()
            .elicit(
                "confirm",
                "Please confirm",
                json!({
                    "type": "object",
                    "properties": { "ok": { "type": "boolean" } },
                    "required": ["ok"],
                }),
            )
            .with_state(json!({ "sealed": true })),
    ))
}

/// `input-required-result-capability-check` : le scénario ne déclare que
/// `sampling`. Le noyau refuse en `-32021` toute requête embarquée dont la
/// capacité client n'est pas déclarée : la fixture n'émet donc que
/// `sampling/createMessage`, la seule méthode autorisée pour ce client.
fn mrtr_capabilities(_args: &Value, round: &InputRound) -> Result<ToolOutcome, ToolError> {
    if round.response("capital_question").is_some() {
        return Ok(ToolOutcome::Result(CallToolResult::from_content([
            ContentBlock::text("capability-check: sampling received"),
        ])));
    }
    Ok(ToolOutcome::InputRequired(
        InputRequiredSpec::new().create_message(
            "capital_question",
            json!({
                "messages": [{
                    "role": "user",
                    "content": { "type": "text", "text": "What is the capital of France?" },
                }],
                "maxTokens": 100,
            }),
        ),
    ))
}

/// Ressource MRTR — `resources/read` supporte le cycle comme `tools/call`.
/// L'URI se trie **après** les ressources scorées : le scénario `caching`
/// lit la première entrée de `resources/list`, qui doit rester
/// `test://static-binary`.
fn mrtr_resource(round: &InputRound) -> Result<ResourceOutcome, String> {
    if let Some(content) = round.accepted("resource_scope") {
        let scope = content
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("default");
        return Ok(ResourceOutcome::Data(ResourceData::text(
            MRTR_RESOURCE_URI,
            Some("text/plain".into()),
            format!("MRTR resource resolved for scope '{scope}'."),
        )));
    }
    Ok(ResourceOutcome::InputRequired(
        InputRequiredSpec::new()
            .elicit(
                "resource_scope",
                "Which scope should the resource use?",
                elicit_schema("scope"),
            )
            .with_state(json!({ "phase": "resource" })),
    ))
}

/// URI de la ressource MRTR — trié après `test://template/123/data` pour ne
/// pas devenir la première entrée lue par le scénario `caching`.
const MRTR_RESOURCE_URI: &str = "test://template/zz-mrtr/data";

/// Construit la fixture complète liée à `addr` (non démarrée : appeler
/// [`litemcp::http::BoundServer::serve`] pour la boucle d'écoute, en premier
/// plan pour le binaire, sur un thread pour les tests).
pub fn build_fixture(addr: &str) -> std::io::Result<litemcp::http::BoundServer> {
    let builder = ServerBuilder::new("litemcp-conformance", env!("CARGO_PKG_VERSION"))
        // ===== Outils (contenus riches — API `.tool_result`) =====
        .tool_result(
            "test_simple_text",
            "Returns a simple text content.",
            |_args| {
                Ok(CallToolResult::from_content([ContentBlock::text(
                    "This is a simple text response for testing.",
                )]))
            },
        )
        .tool_result(
            "test_image_content",
            "Returns a 1x1 red PNG image.",
            |_args| {
                Ok(CallToolResult::from_content([ContentBlock::image_bytes(
                    PNG_1X1_RED,
                    "image/png",
                )]))
            },
        )
        .tool_result(
            "test_audio_content",
            "Returns a minimal WAV audio clip.",
            |_args| {
                Ok(CallToolResult::from_content([ContentBlock::audio_bytes(
                    WAV_SILENCE,
                    "audio/wav",
                )]))
            },
        )
        .tool_result(
            "test_embedded_resource",
            "Returns an embedded text resource.",
            |_args| {
                Ok(CallToolResult::from_content([ContentBlock::embedded_text(
                    "test://embedded-resource",
                    Some("text/plain".into()),
                    "This is an embedded resource content.",
                )]))
            },
        )
        .tool_result(
            "test_multiple_content_types",
            "Returns text, image and embedded resource in one result.",
            |_args| {
                Ok(CallToolResult::from_content([
                    ContentBlock::text("Multiple content types test:"),
                    ContentBlock::image_bytes(PNG_1X1_RED, "image/png"),
                    ContentBlock::embedded_text(
                        "test://mixed-content-resource",
                        Some("application/json".into()),
                        "{\"test\":\"data\",\"value\":123}",
                    ),
                ]))
            },
        )
        .tool_result(
            "test_error_handling",
            "Always fails with isError: true.",
            |_args| {
                Err(ToolError::unclassified(
                    "This tool intentionally returns an error for testing",
                ))
            },
        )
        // Progression request-scoped : quand la requête porte
        // `_meta.progressToken` et que le client accepte `text/event-stream`,
        // le transport ouvre un flux SSE pour cette requête et `progress::report`
        // y pousse `notifications/progress` pendant l'exécution. Sans jeton
        // (ou sans SSE), les rapports sont des non-actions : le tool répond
        // normalement.
        .tool_result(
            "test_tool_with_progress",
            "Reports progress on the request-scoped SSE channel.",
            |_args| {
                for (progress, delay) in [(0.0, 50), (50.0, 50), (100.0, 0)] {
                    let _ = litemcp::progress::report(progress, Some(100.0), None);
                    if delay > 0 {
                        std::thread::sleep(std::time::Duration::from_millis(delay));
                    }
                }
                Ok(CallToolResult::from_content([ContentBlock::text(
                    "Progress tool executed.",
                )]))
            },
        )
        // ===== MRTR (SEP-2322) — handlers multi round-trip =====
        .tool_mrtr(
            "test_input_required_result_elicitation",
            "MRTR: elicits a user name, then greets it.",
            mrtr_elicitation,
        )
        .tool_mrtr(
            "test_input_required_result_sampling",
            "MRTR: requests a sampling completion.",
            mrtr_sampling,
        )
        .tool_mrtr(
            "test_input_required_result_list_roots",
            "MRTR: requests the client roots.",
            mrtr_list_roots,
        )
        .tool_mrtr(
            "test_input_required_result_request_state",
            "MRTR: validates the signed requestState.",
            mrtr_request_state,
        )
        .tool_mrtr(
            "test_input_required_result_multiple_inputs",
            "MRTR: elicitation, sampling and roots in one result.",
            mrtr_multiple_inputs,
        )
        .tool_mrtr(
            "test_input_required_result_multi_round",
            "MRTR: three rounds driven by the signed state.",
            mrtr_multi_round,
        )
        .tool_mrtr(
            "test_input_required_result_tampered_state",
            "MRTR: rejects a tampered requestState.",
            mrtr_tampered_state,
        )
        .tool_mrtr(
            "test_input_required_result_capabilities",
            "MRTR: only requests declared client capabilities.",
            mrtr_capabilities,
        )
        // Outils diagnostiques du scénario `server-stateless` (SEP-2575).
        .tool_result(
            "test_streaming_elicitation",
            "Plain success used to probe response streams.",
            |_args| {
                Ok(CallToolResult::from_content([ContentBlock::text(
                    "Streaming probe executed.",
                )]))
            },
        )
        .tool_result(
            "test_logging_tool",
            "Plain success, never logs without logLevel.",
            |_args| {
                Ok(CallToolResult::from_content([ContentBlock::text(
                    "Logging probe executed.",
                )]))
            },
        )
        .tool_result(
            "test_missing_capability",
            "Requires the `sampling` client capability (SEP-2575 diagnostic).",
            |_args| {
                Ok(CallToolResult::from_content([ContentBlock::text(
                    "unreachable without the sampling capability",
                )]))
            },
        )
        .require_client_capability("test_missing_capability", "sampling")
        // Outil diagnostique du scénario (pending) `http-custom-header-server-`
        // `validation` (SEP-2243) : le premier paramètre string annoté
        // `x-mcp-header` y est sondé (Base64 valide/invalide/sentinelle).
        .tool_typed(
            "test_header_param_tool",
            "Exercises x-mcp-header transport parameters (SEP-2243).",
            zod::object()
                .field("str_param", zod::string().header("str-h"))
                .field("int_param", zod::integer().header("int-h"))
                .field("bool_param", zod::boolean().header("bool-h"))
                .opt("opt_param", zod::string().header("opt-h")),
            |args| Ok(args.clone()),
        )
        // ===== Prompts =====
        .prompt_mrtr(
            "test_input_required_result_prompt",
            "MRTR: prompts/get asks for context before rendering.",
            mrtr_prompt,
        )
        .prompt(
            "test_simple_prompt",
            "Returns a simple text message.",
            |_args| {
                Ok(vec![PromptMessage::user(
                    "This is a simple prompt for testing.",
                )])
            },
        )
        .prompt(
            "test_prompt_with_arguments",
            "Substitutes arg1 and arg2 into the message.",
            |args| {
                let arg1 = args.get("arg1").and_then(|v| v.as_str()).unwrap_or("");
                let arg2 = args.get("arg2").and_then(|v| v.as_str()).unwrap_or("");
                Ok(vec![PromptMessage::user(format!(
                    "Prompt with arguments: arg1='{arg1}', arg2='{arg2}'"
                ))])
            },
        )
        .prompt(
            "test_prompt_with_embedded_resource",
            "Embeds the resource given via `resourceUri`.",
            |args| {
                let uri = args
                    .get("resourceUri")
                    .and_then(|v| v.as_str())
                    .unwrap_or("test://example-resource");
                Ok(vec![
                    PromptMessage::user_content(ContentBlock::embedded_text(
                        uri,
                        Some("text/plain".into()),
                        "Embedded resource content for testing.",
                    )),
                    PromptMessage::user("Please process the embedded resource above."),
                ])
            },
        )
        .prompt(
            "test_prompt_with_image",
            "Returns an image message.",
            |_args| {
                Ok(vec![
                    PromptMessage::user_content(ContentBlock::image_bytes(
                        PNG_1X1_RED,
                        "image/png",
                    )),
                    PromptMessage::user("Please analyze the image above."),
                ])
            },
        )
        // ===== Ressources =====
        .resource(
            "test://static-text",
            "Static text resource",
            "The static text resource of the conformance fixture.",
            || {
                Ok(ResourceContents {
                    uri: "test://static-text".into(),
                    mime_type: Some("text/plain".into()),
                    text: "This is the content of the static text resource.".into(),
                })
            },
        )
        .resource_data(
            "test://static-binary",
            "Static binary resource",
            "A binary (PNG) resource served as `blob`.",
            || {
                Ok(ResourceData::binary(
                    "test://static-binary",
                    Some("image/png".into()),
                    PNG_1X1_RED,
                ))
            },
        )
        // La lecture du gabarit par la suite cible l'URI **concierge**
        // `test://template/123/data` — le noyau matche des URIs exactes, la
        // fixture l'enregistre donc comme ressource concrète.
        .resource(
            "test://template/123/data",
            "Template data for id 123",
            "Concrete read of the {id} template for the conformance suite.",
            || {
                Ok(ResourceContents {
                    uri: "test://template/123/data".into(),
                    mime_type: Some("application/json".into()),
                    text: "{\"id\":\"123\",\"templateTest\":true,\"data\":\"Data for ID: 123\"}"
                        .into(),
                })
            },
        )
        .resource_template(
            "test://template/{id}/data",
            "Template data",
            "Data for a given identifier (RFC 6570 template).",
        )
        .resource_mrtr(
            MRTR_RESOURCE_URI,
            "MRTR resource",
            "Reads a scope through the MRTR cycle.",
            mrtr_resource,
        )
        // ===== Complétion =====
        .completions(|request: &CompletionRequest| match &request.reference {
            CompleteReference::Prompt { name, .. } if name == "test_prompt_with_arguments" => {
                Ok(Completion::new(vec![
                    "testValue1".into(),
                    "testValue2".into(),
                ]))
            }
            _ => Ok(Completion::new(vec![])),
        });

    // Scénario `json-schema-2020-12` (pending dans `requirements/2026-07-28.yaml`,
    // non scoré) : le DSL `zod` n'exprime ni `$defs`/`$anchor` ni
    // `if`/`then`/`else`. La fixture publie donc le document **brut** via
    // `from_json_schema`, disponible uniquement avec la feature opt-in
    // `schemars` (dépendances schemars/jsonschema hors du cœur) — le
    // workflow de conformance la construit avec `--features schemars` ; sans
    // elle, l'outil n'est simplement pas annoncé.
    #[cfg(feature = "schemars")]
    let builder = builder.tool_typed(
        "json_schema_2020_12_tool",
        "Tool with JSON Schema 2020-12 features (SEP-1613, SEP-2106).",
        litemcp::schemars::from_json_schema(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "$defs": {
                "address": {
                    "$anchor": "addressDef",
                    "type": "object",
                    "properties": {
                        "street": { "type": "string" },
                        "city": { "type": "string" },
                    },
                },
            },
            "properties": {
                "name": { "type": "string" },
                "address": { "$ref": "#/$defs/address" },
                "contactMethod": { "type": "string", "enum": ["phone", "email"] },
                "phone": { "type": "string" },
                "email": { "type": "string" },
            },
            "allOf": [{
                "anyOf": [
                    { "required": ["phone"] },
                    { "required": ["email"] },
                    // Branche propre à la fixture : `http-header-validation`
                    // (SEP-2243) appelle le PREMIER outil de `tools/list` avec
                    // `{}` pour vérifier le rognage des espaces — un schéma
                    // strictement conditionnel rendrait ce cas rouge.
                    {},
                ],
            }],
            "if": {
                "properties": { "contactMethod": { "const": "phone" } },
                "required": ["contactMethod"],
            },
            "then": { "required": ["phone"] },
            // `else` permissif (mais publié) : avec `{}`, le `if` échoue et
            // c'est `else` qui s'applique — voir la branche `{}` ci-dessus,
            // même contrainte de fixture (appel à vide du scénario SEP-2243).
            "else": {},
            "additionalProperties": false,
        })),
        |args| Ok(args.clone()),
    );

    builder.bind(addr)
}
