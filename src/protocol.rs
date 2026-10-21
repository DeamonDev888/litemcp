//! Types MCP 2026-07-28 — le strict nécessaire d'un serveur stateless.

use serde::Serialize;
use serde_json::Value;

/// Version de la spec MCP ciblée.
pub const PROTOCOL_VERSION: &str = "2026-07-28";

/// Méthodes supportées par le noyau.
pub mod method {
    pub const INITIALIZE: &str = "initialize";
    pub const PING: &str = "ping";
    pub const TOOLS_CALL: &str = "tools/call";
    pub const TOOLS_LIST: &str = "tools/list";
}

/// Description d'un outil exposé par `tools/list`.
#[derive(Debug, Clone, Serialize)]
pub struct Tool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "inputSchema", skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
}

/// Résultat de `tools/list`.
#[derive(Debug, Serialize)]
pub struct ToolsListResult {
    pub tools: Vec<Tool>,
}

/// Bloc de contenu texte d'un résultat d'outil.
#[derive(Debug, Serialize)]
pub struct TextContent {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub text: String,
}

/// Résultat de `tools/call` (MCP : `content` + `isError` + `structuredContent`).
#[derive(Debug, Serialize)]
pub struct CallToolResult {
    pub content: Vec<TextContent>,
    #[serde(rename = "isError", skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(rename = "structuredContent", skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
}

impl From<Result<Value, String>> for CallToolResult {
    fn from(result: Result<Value, String>) -> Self {
        match result {
            Ok(value) => Self {
                content: vec![TextContent {
                    kind: "text",
                    text: value.to_string(),
                }],
                is_error: None,
                structured_content: Some(value),
            },
            Err(message) => Self {
                content: vec![TextContent {
                    kind: "text",
                    text: message,
                }],
                is_error: Some(true),
                structured_content: None,
            },
        }
    }
}

/// Informations serveur renvoyées à `initialize` (compatibilité inspecteurs).
#[derive(Debug, Serialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}
