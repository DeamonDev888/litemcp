//! Types MCP 2026-07-28 — le strict nécessaire d'un serveur stateless.

use serde::Serialize;
use serde_json::Value;

/// Version de la spec MCP ciblée.
///
/// La spec `2026-07-28` est **stateless** : pas de handshake
/// `initialize`/`initialized`, pas de `Mcp-Session-Id`. Chaque requête porte
/// sa version dans `_meta["io.modelcontextprotocol/protocolVersion"]`.
pub const PROTOCOL_VERSION: &str = "2026-07-28";

/// Versions de protocole supportées par ce serveur.
///
/// La spec `2026-07-28` exige qu'un serveur annonce les versions qu'il
/// implémente (`server/discover`) et refuse une version non supportée avec
/// `UnsupportedProtocolVersionError` (`-32022`).
pub const SUPPORTED_VERSIONS: &[&str] = &[PROTOCOL_VERSION];

/// Clé `_meta` portant la version de protocole d'une requête (SEP-2575).
pub const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";

/// Clé `_meta` portant les informations client (SEP-2575).
pub const META_CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";

/// Clé `_meta` portant les informations serveur (SEP-2575).
pub const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

/// Méthodes supportées par le noyau.
pub mod method {
    /// Découverte des capacités du serveur (**MUST**, SEP-2575).
    pub const DISCOVER: &str = "server/discover";
    /// Handshake legacy `2025-11-25` — toléré, plus requis.
    pub const INITIALIZE: &str = "initialize";
    /// Méthode legacy `2025-11-25` — retirée de la spec `2026-07-28`.
    pub const PING: &str = "ping";
    pub const TOOLS_CALL: &str = "tools/call";
    pub const TOOLS_LIST: &str = "tools/list";
    pub const RESOURCES_LIST: &str = "resources/list";
    pub const RESOURCES_READ: &str = "resources/read";
    pub const PROMPTS_LIST: &str = "prompts/list";
    pub const PROMPTS_GET: &str = "prompts/get";
}

/// Cette méthode **exécute du code côté serveur** (effet de bord ou
/// accès à une ressource).
///
/// Une notification (requête sans `id`) sur une telle méthode est
/// inexploitable : JSON-RPC 2.0 §4 impose de la traiter, mais le
/// mode stateless de MCP ne peut **pas** en livrer le résultat au
/// client (aucune réponse) — l'effet de bord serait invisible et
/// invérifiable. Le noyau refuse donc explicitement ces
/// notifications (`-32600`) : le client sait que **rien n'a été
/// exécuté**.
#[must_use]
pub fn is_action(method: &str) -> bool {
    matches!(
        method,
        method::TOOLS_CALL | method::RESOURCES_READ | method::PROMPTS_GET
    )
}

/// Cette méthode est-elle une **notification de protocole** ?
///
/// Les notifications `notifications/*` (`initialized`, `cancelled`,
/// `progress`…) sont des messages de contrôle : le serveur n'a rien à
/// en faire en mode stateless — il les accepte et les ignore.
#[must_use]
pub fn is_protocol_notification(method: &str) -> bool {
    method.starts_with("notifications/")
}

/// Type de résultat — champ **obligatoire** dans chaque `result` (SEP-2322).
///
/// * `"complete"` : résultat ordinaire, terminal ;
/// * `"input_required"` : résultat intermédiaire d'une requête multi-tour
///   (MRTR) — le client doit réémettre la requête avec `inputResponses`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultType {
    /// Résultat terminal.
    Complete,
    /// Résultat intermédiaire : des informations supplémentaires sont requises.
    InputRequired,
}

/// Portée de cache d'un résultat de liste (SEP-2549).
///
/// * `"public"` : les intermédiaires partagés peuvent mettre en cache ;
/// * `"private"` : cache réservé au client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheScope {
    /// Cacheable par les intermédiaires.
    Public,
    /// Cacheable uniquement par le client.
    Private,
}

/// Description d'un outil exposé par `tools/list`.
#[derive(Debug, Clone, Serialize)]
pub struct Tool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "inputSchema", skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
    /// Schéma de sortie : publié tel quel et **appliqué** — un résultat
    /// `structuredContent` non conforme devient un résultat `isError`.
    #[serde(rename = "outputSchema", skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
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
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Titre lisible par l'humain.
    #[must_use]
    pub fn title(mut self, title: &str) -> Self {
        self.title = Some(title.to_string());
        self
    }

    /// `readOnlyHint` — l'outil ne modifie pas son environnement.
    #[must_use]
    pub fn read_only(mut self, hint: bool) -> Self {
        self.read_only_hint = Some(hint);
        self
    }

    /// `destructiveHint` — l'outil peut modifier de façon destructive
    /// (sans effet si `readOnlyHint` est vrai).
    #[must_use]
    pub fn destructive(mut self, hint: bool) -> Self {
        self.destructive_hint = Some(hint);
        self
    }

    /// `idempotentHint` — répéter l'appel ne change rien de plus.
    #[must_use]
    pub fn idempotent(mut self, hint: bool) -> Self {
        self.idempotent_hint = Some(hint);
        self
    }

    /// `openWorldHint` — l'outil interagit avec le monde extérieur.
    #[must_use]
    pub fn open_world(mut self, hint: bool) -> Self {
        self.open_world_hint = Some(hint);
        self
    }
}

/// Résultat de `tools/list`.
///
/// Conformité `2026-07-28` : porte `resultType`, `ttlMs` (indice de
/// fraîcheur) et `cacheScope` (SEP-2549), et identifie le serveur sous
/// `_meta` (SEP-2575).
#[derive(Debug, Serialize)]
pub struct ToolsListResult {
    /// Type de résultat — **obligatoire** (SEP-2322).
    #[serde(rename = "resultType")]
    pub result_type: ResultType,
    /// Durée de vie conseillée du catalogue, en millisecondes (SEP-2549).
    #[serde(rename = "ttlMs")]
    pub ttl_ms: u64,
    /// Portée de cache (SEP-2549).
    #[serde(rename = "cacheScope")]
    pub cache_scope: CacheScope,
    pub tools: Vec<Tool>,
    /// Identité du serveur (SEP-2575 : SHOULD dans chaque résultat).
    #[serde(rename = "_meta")]
    pub meta: Value,
}

/// Bloc de contenu texte d'un résultat d'outil.
#[derive(Debug, Serialize)]
pub struct TextContent {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub text: String,
}

/// Résultat de `tools/call` (MCP : `content` + `isError` + `structuredContent`).
///
/// Conformité `2026-07-28` : porte `resultType` (SEP-2322) et s'identifie
/// sous `_meta` (SEP-2575) — le champ est posé par le noyau après
/// construction, [`CallToolResult::from`] le laisse à `None`.
#[derive(Debug, Serialize)]
pub struct CallToolResult {
    /// Type de résultat — **obligatoire** (SEP-2322).
    #[serde(rename = "resultType")]
    pub result_type: ResultType,
    pub content: Vec<TextContent>,
    #[serde(rename = "isError", skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    /// Contenu structuré du résultat — la valeur **exactement** envoyée.
    ///
    /// forme : objet JSON (invariant de l'API) — sauf quand l'outil porte
    /// un `outputSchema` de racine non-objet : la valeur brute est alors
    /// envoyée (la spec `2026-07-28` / SEP-2106 autorise toute valeur JSON
    /// dans `structuredContent`, et l'enveloppe `{ "result": … }` serait
    /// elle-même non conforme au schéma publié).
    #[serde(rename = "structuredContent", skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    /// Identité du serveur (SEP-2575 : SHOULD dans chaque résultat).
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

impl CallToolResult {
    /// Construit un résultat de **succès** à partir du contenu structuré
    /// tel qu'il sera **effectivement envoyé**.
    ///
    /// Le contenu texte reflète la sérialisation exacte du
    /// `structuredContent` (compatibilité : un client qui ne lit que le
    /// texte voit la même valeur que celui qui lit le contenu structuré).
    ///
    /// Le `structuredContent` doit **conformer** au `outputSchema` publié
    /// (spec MCP `2026-07-28` : « Servers **MUST** provide structured
    /// results that conform to this schema ») — c'est au noyau
    /// ([`crate::server::ServerCore`]) que la forme est choisie, en
    /// fonction du schéma publié.
    #[must_use]
    pub fn from_structured(structured: Value) -> Self {
        Self {
            result_type: ResultType::Complete,
            content: vec![TextContent {
                kind: "text",
                text: structured.to_string(),
            }],
            is_error: None,
            structured_content: Some(structured),
            meta: None,
        }
    }
}

impl From<Result<Value, String>> for CallToolResult {
    fn from(result: Result<Value, String>) -> Self {
        match result {
            Ok(value) => {
                // Choix d'API : `structuredContent` est toujours un objet.
                // La spec `2026-07-28` (SEP-2106) autorise désormais toute
                // valeur JSON, mais un objet invariant garde le contrat
                // simple pour les clients : toute valeur non-objet est
                // enveloppée dans `{ "result": … }`, et le contenu texte
                // reflète la valeur structurée réellement envoyée.
                //
                // **Exception** : quand un `outputSchema` de racine
                // non-objet est attaché à l'outil, l'enveloppe serait
                // elle-même non conforme au schéma publié — le noyau
                // ([`crate::server::ServerCore`]) construit alors le
                // résultat par [`CallToolResult::from_structured`] avec la
                // valeur brute.
                let structured = if value.is_object() {
                    value
                } else {
                    serde_json::json!({ "result": value })
                };
                Self::from_structured(structured)
            }
            Err(message) => Self {
                result_type: ResultType::Complete,
                content: vec![TextContent {
                    kind: "text",
                    text: message,
                }],
                is_error: Some(true),
                structured_content: None,
                meta: None,
            },
        }
    }
}

// --- Resources (SEP-2549 : les résultats de liste/lecture portent les
// --- indices de cache comme tools/list) ---

/// Entrée du catalogue `resources/list`.
#[derive(Debug, Clone, Serialize)]
pub struct Resource {
    pub uri: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// Résultat de `resources/list`.
#[derive(Debug, Serialize)]
pub struct ResourcesListResult {
    #[serde(rename = "resultType")]
    pub result_type: ResultType,
    #[serde(rename = "ttlMs")]
    pub ttl_ms: u64,
    #[serde(rename = "cacheScope")]
    pub cache_scope: CacheScope,
    pub resources: Vec<Resource>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

/// Contenu texte d'une ressource lue (`contents[]` de `resources/read`).
#[derive(Debug, Clone, Serialize)]
pub struct ResourceContents {
    pub uri: String,
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    pub text: String,
}

/// Résultat de `resources/read`.
#[derive(Debug, Serialize)]
pub struct ReadResourceResult {
    #[serde(rename = "resultType")]
    pub result_type: ResultType,
    #[serde(rename = "ttlMs")]
    pub ttl_ms: u64,
    #[serde(rename = "cacheScope")]
    pub cache_scope: CacheScope,
    pub contents: Vec<ResourceContents>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

// --- Prompts ---

/// Entrée du catalogue `prompts/list`.
#[derive(Debug, Clone, Serialize)]
pub struct Prompt {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Résultat de `prompts/list`.
#[derive(Debug, Serialize)]
pub struct PromptsListResult {
    #[serde(rename = "resultType")]
    pub result_type: ResultType,
    #[serde(rename = "ttlMs")]
    pub ttl_ms: u64,
    #[serde(rename = "cacheScope")]
    pub cache_scope: CacheScope,
    pub prompts: Vec<Prompt>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

/// Bloc de contenu texte d'un message de prompt.
#[derive(Debug, Clone, Serialize)]
pub struct PromptContent {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub text: String,
}

/// Message d'un prompt (`role` : `"user"` ou `"assistant"`).
#[derive(Debug, Clone, Serialize)]
pub struct PromptMessage {
    pub role: &'static str,
    pub content: PromptContent,
}

impl PromptMessage {
    /// Message au rôle `user`.
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: "user",
            content: PromptContent {
                kind: "text",
                text: text.into(),
            },
        }
    }

    /// Message au rôle `assistant`.
    #[must_use]
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: "assistant",
            content: PromptContent {
                kind: "text",
                text: text.into(),
            },
        }
    }
}

/// Résultat de `prompts/get`.
#[derive(Debug, Serialize)]
pub struct GetPromptResult {
    #[serde(rename = "resultType")]
    pub result_type: ResultType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub messages: Vec<PromptMessage>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

/// Informations serveur renvoyées par `server/discover` et `initialize`
/// (compatibilité inspecteurs).
#[derive(Debug, Serialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

/// Résultat de `server/discover` (**MUST**, SEP-2575).
///
/// Annonce les versions de protocole supportées, les capacités du serveur et
/// son identité. C'est la méthode que les clients `2026-07-28` appellent
/// pour découvrir les capacités *avant* toute autre requête.
#[derive(Debug, Serialize)]
pub struct DiscoverResult {
    /// Type de résultat — **obligatoire** (SEP-2322).
    #[serde(rename = "resultType")]
    pub result_type: ResultType,
    /// Versions de protocole supportées par le serveur.
    #[serde(rename = "supportedVersions")]
    pub supported_versions: Vec<&'static str>,
    /// Capacités exposées par le serveur.
    pub capabilities: Value,
    /// Identité du serveur, sous `_meta` (SEP-2575).
    #[serde(rename = "_meta")]
    pub meta: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn non_object_results_are_wrapped_into_an_object() {
        // La spec MCP exige un objet dans structuredContent.
        let result = CallToolResult::from(Ok::<Value, String>(json!(42)));
        let v = serde_json::to_value(&result).unwrap();
        assert_eq!(v["structuredContent"], json!({ "result": 42 }));
        assert_eq!(v["content"][0]["text"], r#"{"result":42}"#);
        assert_eq!(v["resultType"], "complete");

        let result = CallToolResult::from(Ok::<Value, String>(Value::Null));
        let v = serde_json::to_value(&result).unwrap();
        assert_eq!(v["structuredContent"], json!({ "result": null }));

        let result = CallToolResult::from(Ok::<Value, String>(json!([1, 2])));
        let v = serde_json::to_value(&result).unwrap();
        assert_eq!(v["structuredContent"], json!({ "result": [1, 2] }));

        // Un objet passe tel quel, sans enveloppe.
        let result = CallToolResult::from(Ok::<Value, String>(json!({ "somme": 12 })));
        let v = serde_json::to_value(&result).unwrap();
        assert_eq!(v["structuredContent"], json!({ "somme": 12 }));
        assert_eq!(v["content"][0]["text"], r#"{"somme":12}"#);
    }

    #[test]
    fn every_result_carries_a_result_type() {
        // SEP-2322 : resultType est OBLIGATOIRE dans chaque result.
        let call = CallToolResult::from(Ok::<Value, String>(json!({})));
        let v = serde_json::to_value(&call).unwrap();
        assert_eq!(v["resultType"], "complete");
    }
}
