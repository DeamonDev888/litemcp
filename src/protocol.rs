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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<ToolAnnotations>,
}

/// Annotations d'un outil (spec MCP 2026-07-28) — métadonnées descriptives
/// pour les clients : `title` lisible et indices de comportement.
/// Champ absent = le client applique le défaut de la spec (ex. un outil sans
/// `readOnlyHint` est considéré comme potentiellement modifiant).
#[derive(Debug, Clone, Default, Serialize)]
pub struct ToolAnnotations {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "readOnlyHint", skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    #[serde(rename = "destructiveHint", skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
    #[serde(rename = "idempotentHint", skip_serializing_if = "Option::is_none")]
    pub idempotent_hint: Option<bool>,
    #[serde(rename = "openWorldHint", skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
}

impl ToolAnnotations {
    /// Annotations vides — tout est omis du JSON jusqu'à ce qu'il soit posé.
    pub fn new() -> Self {
        Self::default()
    }

    /// Titre lisible par l'humain.
    pub fn title(mut self, title: &str) -> Self {
        self.title = Some(title.to_string());
        self
    }

    /// `readOnlyHint` — l'outil ne modifie pas son environnement.
    pub fn read_only(mut self, hint: bool) -> Self {
        self.read_only_hint = Some(hint);
        self
    }

    /// `destructiveHint` — l'outil peut modifier de façon destructive
    /// (sans effet si `readOnlyHint` est vrai).
    pub fn destructive(mut self, hint: bool) -> Self {
        self.destructive_hint = Some(hint);
        self
    }

    /// `idempotentHint` — répéter l'appel ne change rien de plus.
    pub fn idempotent(mut self, hint: bool) -> Self {
        self.idempotent_hint = Some(hint);
        self
    }

    /// `openWorldHint` — l'outil interagit avec le monde extérieur.
    pub fn open_world(mut self, hint: bool) -> Self {
        self.open_world_hint = Some(hint);
        self
    }
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
