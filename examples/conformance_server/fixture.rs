//! Fixture de conformance officielle MCP **2026-07-28** — construction du
//! serveur portant les objets `test_*` attendus par la suite
//! `modelcontextprotocol/conformance` (SEP-1730).
//!
//! Ce module est **partagé** : il construit à la fois le binaire
//! `examples/conformance_server` (via `main.rs`) et le test de régression
//! `tests/conformance_fixture.rs` (inclusion `#[path]`) — une seule source de
//! vérité pour la surface testée par la suite.
//!
//! Un résultat obtenu contre cette fixture n'est un score de conformité que
//! si la suite est invoquée avec `--requirements 2026-07-28` (liste scorée
//! figée par le fichier `requirements/2026-07-28.yaml` du harness, version
//! `@modelcontextprotocol/conformance@0.2.0-alpha.12`).
//!
//! La fixture n'utilise que l'API publique de LiteMCP (`.tool_result`,
//! `.prompt`, `.resource`/`.resource_data`, `.resource_template`,
//! `.completions`, `.require_client_capability`) — aucun contournement
//! interne, aucune modification de la suite pour la faire passer.

use litemcp::prelude::*;
use litemcp::protocol::{
    CallToolResult, CompleteReference, Completion, CompletionRequest, ContentBlock, PromptMessage,
    ResourceContents, ResourceData,
};

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

/// Construit la fixture complète liée à `addr` (non démarrée : appeler
/// [`litemcp::http::BoundServer::serve`] pour la boucle d'écoute, en premier
/// plan pour le binaire, sur un thread pour les tests).
pub fn build_fixture(addr: &str) -> std::io::Result<litemcp::http::BoundServer> {
    ServerBuilder::new("litemcp-conformance", env!("CARGO_PKG_VERSION"))
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
        // Le transport stateless de LiteMCP n'a pas de canal de notifications
        // pendant l'exécution : le tool répond directement (le scénario
        // `tools-call-with-progress` échoue sur l'absence de notifications —
        // limitation consignée dans la baseline datée).
        .tool_result(
            "test_tool_with_progress",
            "Executes without progress channel.",
            |_args| {
                Ok(CallToolResult::from_content([ContentBlock::text(
                    "Progress tool executed (no progress channel in stateless transport).",
                )]))
            },
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
        // ===== Complétion =====
        .completions(|request: &CompletionRequest| match &request.reference {
            CompleteReference::Prompt { name, .. } if name == "test_prompt_with_arguments" => {
                Ok(Completion::new(vec![
                    "testValue1".into(),
                    "testValue2".into(),
                ]))
            }
            _ => Ok(Completion::new(vec![])),
        })
        .bind(addr)
}
