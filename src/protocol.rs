//! Types MCP 2026-07-28 — le strict nécessaire d'un serveur stateless.

use crate::error::ToolError;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

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

/// Révision legacy la plus récente servie par le profil `.legacy(true)`.
///
/// Le profil legacy est un serveur **dual-era** (spec `2026-07-28`,
/// « Backward Compatibility with Initialization-Based Versions ») : une
/// requête portant `_meta` est servie selon `2026-07-28`, un `initialize`
/// ouvre la sémantique legacy de cette révision.
pub const LEGACY_PROTOCOL_VERSION: &str = "2025-11-25";

/// Révisions servies par le handshake `initialize` du profil legacy, de la
/// plus récente à la plus ancienne (indice 0 = réponse par défaut).
///
/// Distinct de [`SUPPORTED_VERSIONS`], qui régit la négociation par requête
/// (`_meta`, `server/discover`) : `2025-11-25` n'accepte pas `_meta` et ne
/// connaît pas `server/discover`.
pub const LEGACY_SUPPORTED_VERSIONS: &[&str] = &[LEGACY_PROTOCOL_VERSION];

/// Négociation de version du handshake legacy (révision `2025-11-25`,
/// « Version Negotiation ») : la révision demandée si le profil la sert,
/// sinon la plus récente révision legacy servie.
///
/// « If the server supports the requested protocol version, it MUST respond
/// with the same version. Otherwise, the server MUST respond with another
/// protocol version it supports. »
#[must_use]
pub fn negotiate_legacy_version(requested: Option<&str>) -> &'static str {
    let served = requested.and_then(|version| {
        LEGACY_SUPPORTED_VERSIONS
            .iter()
            .find(|served| **served == version)
            .copied()
    });
    served.unwrap_or_else(|| {
        LEGACY_SUPPORTED_VERSIONS
            .first()
            .copied()
            .unwrap_or(LEGACY_PROTOCOL_VERSION)
    })
}

/// Clé `_meta` portant la version de protocole d'une requête (SEP-2575).
pub const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";

/// Clé `_meta` portant les capacités déclarées par le client (SEP-2575).
pub const META_CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";

/// Clé `_meta` portant les informations client (SEP-2575).
pub const META_CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";

/// Clé `_meta` portant les informations serveur (SEP-2575).
pub const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

/// Méthodes supportées par le noyau.
pub mod method {
    /// Découverte des capacités du serveur (**MUST**, SEP-2575).
    pub const DISCOVER: &str = "server/discover";
    /// Handshake legacy `2025-11-25` — servi uniquement en profil legacy
    /// (`.legacy(true)`) ; refusé en `-32601` en profil strict (défaut).
    pub const INITIALIZE: &str = "initialize";
    /// Méthode legacy `2025-11-25` — retirée de la spec `2026-07-28`.
    pub const PING: &str = "ping";
    pub const TOOLS_CALL: &str = "tools/call";
    pub const TOOLS_LIST: &str = "tools/list";
    pub const RESOURCES_LIST: &str = "resources/list";
    /// Catalogue des gabarits de ressources (URI templates RFC 6570).
    pub const RESOURCES_TEMPLATES_LIST: &str = "resources/templates/list";
    pub const RESOURCES_READ: &str = "resources/read";
    pub const PROMPTS_LIST: &str = "prompts/list";
    pub const PROMPTS_GET: &str = "prompts/get";
    /// Suggestions d'auto-complétion pour les arguments de prompt et de
    /// gabarit de ressource.
    pub const COMPLETION_COMPLETE: &str = "completion/complete";
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
        method::TOOLS_CALL
            | method::RESOURCES_READ
            | method::PROMPTS_GET
            | method::COMPLETION_COMPLETE
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

/// Clé `params._meta` portant le jeton de progression fourni par le client
/// (`notifications/progress` — le serveur reprend **ce** jeton, il ne
/// l'invente jamais).
pub const META_PROGRESS_TOKEN: &str = "progressToken";

/// Nom de la méthode `notifications/progress` (spec MCP).
pub const NOTIFICATION_PROGRESS: &str = "notifications/progress";

/// Type de résultat — champ **obligatoire** dans chaque `result` (SEP-2322).
///
/// * `"complete"` : résultat ordinaire, terminal ;
/// * `"input_required"` : résultat intermédiaire d'une requête multi-tour
///   (MRTR) — le client doit réémettre la requête avec `inputResponses`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ResultType {
    /// Résultat terminal.
    Complete,
    /// Résultat intermédiaire : des informations supplémentaires sont requises.
    InputRequired,
}

/// Clés fil du cycle MRTR (multi round-trip requests, spec `2026-07-28`).
///
/// Au rejeu, le client renvoie **au niveau des `params`** de la requête
/// d'origine (id JSON-RPC différent) la map `inputResponses` et l'écho exact
/// du jeton `requestState` reçu.
pub mod mrtr {
    /// Clé `params` portant les réponses du client (`InputResponses` —
    /// map `clé de requête → résultat du client`).
    pub const INPUT_RESPONSES: &str = "inputResponses";

    /// Clé `params` portant l'écho du `requestState` émis par le serveur.
    pub const REQUEST_STATE: &str = "requestState";
}

/// Méthodes de requête **embarquables** dans `inputRequests` et la capacité
/// client que chacune exige.
///
/// Spec MRTR `2026-07-28` : « Servers MUST NOT send an `inputRequests` that
/// the client has not declared support for in its capabilities » — l'émmission
/// d'une requête non déclarée est refusée en `-32021` par le noyau.
pub mod embedded_request {
    /// Élicitation (formulaire ou URL hors-bande).
    pub const ELICIT_CREATE: &str = "elicitation/create";
    /// Échantillonnage (SEP-2577 : déprécié, toujours embarquable).
    pub const SAMPLING_CREATE_MESSAGE: &str = "sampling/createMessage";
    /// Liste des racines.
    pub const ROOTS_LIST: &str = "roots/list";

    /// Capacité client (`io.modelcontextprotocol/clientCapabilities`) que le
    /// client doit avoir déclarée pour que cette méthode puisse lui être
    /// embarquée — `None` pour une méthode non embarquable.
    #[must_use]
    pub fn client_capability(method: &str) -> Option<&'static str> {
        match method {
            ELICIT_CREATE => Some("elicitation"),
            SAMPLING_CREATE_MESSAGE => Some("sampling"),
            ROOTS_LIST => Some("roots"),
            _ => None,
        }
    }
}

/// Spécification d'un résultat `input_required` (MRTR) produite par un
/// handler `tool_mrtr`/`prompt_mrtr`/`resource_mrtr`.
///
/// Le noyau fait le reste du chemin : validation des requêtes embarquées
/// (méthode embarquable + capacité client déclarée, sinon `-32021`), frappe
/// d'un `requestState` opaque signé (HMAC-SHA256, TTL, liaison
/// méthode/paramètres/id) portant `state`, et sérialisation fil
/// `{ resultType: "input_required", inputRequests?, requestState }`.
///
/// ```
/// use litemcp::protocol::InputRequiredSpec;
///
/// let spec = InputRequiredSpec::new()
///     .elicit(
///         "confirm",
///         "Deploy to prod?",
///         serde_json::json!({"type": "object", "properties": {"confirm": {"type": "boolean"}}, "required": ["confirm"]}),
///     )
///     .with_state(serde_json::json!({"step": 1}));
/// assert_eq!(spec.input_requests().len(), 1);
/// ```
#[derive(Debug, Clone, Default)]
pub struct InputRequiredSpec {
    pub(crate) input_requests: serde_json::Map<String, Value>,
    pub(crate) state: Option<Value>,
}

impl InputRequiredSpec {
    /// Spécification vide — complètez-la avec les constructeurs.
    /// Le noyau refuse une spécification qui ne porte ni `inputRequests` ni
    /// `state` (spec : au moins un des deux dans chaque résultat).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ajoute (ou remplace — la même clé posée deux fois : dernier gagne)
    /// une requête embarquée brute. `method` doit être l'une des trois
    /// méthodes embarquables ([`embedded_request`]) — vérifié à l'émission.
    #[must_use]
    pub fn request(mut self, key: &str, method: &str, params: Value) -> Self {
        self.input_requests.insert(
            key.to_string(),
            serde_json::json!({ "method": method, "params": params }),
        );
        self
    }

    /// Élicitation formulaire (`elicitation/create`, mode `form`).
    #[must_use]
    pub fn elicit(self, key: &str, message: impl Into<String>, requested_schema: Value) -> Self {
        self.request(
            key,
            embedded_request::ELICIT_CREATE,
            serde_json::json!({
                "mode": "form",
                "message": message.into(),
                "requestedSchema": requested_schema,
            }),
        )
    }

    /// Élicitation hors-bande (`elicitation/create`, mode `url`).
    #[must_use]
    pub fn elicit_url(self, key: &str, message: impl Into<String>, url: &str) -> Self {
        self.request(
            key,
            embedded_request::ELICIT_CREATE,
            serde_json::json!({
                "mode": "url",
                "message": message.into(),
                "url": url,
            }),
        )
    }

    /// Échantillonnage (`sampling/createMessage`) — `params` est le corps
    /// complet de la requête (messages, maxTokens…).
    #[must_use]
    pub fn create_message(self, key: &str, params: Value) -> Self {
        self.request(key, embedded_request::SAMPLING_CREATE_MESSAGE, params)
    }

    /// Liste des racines (`roots/list`).
    #[must_use]
    pub fn list_roots(self, key: &str) -> Self {
        self.request(key, embedded_request::ROOTS_LIST, serde_json::json!({}))
    }

    /// Charge utile **opaque** restituée au handler au tour suivant, une fois
    /// le `requestState` vérifié (HMAC + TTL + liaison méthode/paramètres/id).
    /// Signée, **pas chiffrée** : n'y mettez aucun secret. Bornée (8 Kio
    /// sérialisés — au-delà, refus en `-32603`).
    #[must_use]
    pub fn with_state(mut self, state: Value) -> Self {
        self.state = Some(state);
        self
    }

    /// Map des requêtes embarquées (clé → `{method, params}`).
    #[must_use]
    pub fn input_requests(&self) -> &serde_json::Map<String, Value> {
        &self.input_requests
    }
}

/// Portée de cache d'un résultat de liste (SEP-2549).
///
/// * `"public"` : les intermédiaires partagés peuvent mettre en cache ;
/// * `"private"` : cache réservé au client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
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
#[derive(Debug, Clone, Serialize)]
pub struct TextContent {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub text: String,
}

impl TextContent {
    /// Bloc texte — le constructeur des chemins usuels.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            kind: "text",
            text: text.into(),
        }
    }
}

/// Image fournie à ou rendue par un LLM (`ImageContent` du schéma MCP).
///
/// `data` est **base64** : [`ImageContent::from_bytes`] encode les octets
/// réels — un binaire n'est jamais déguisé en texte JSON pour simuler le
/// support. L'encodage est interne (le crate garde exactement deux
/// dépendances : `serde`, `serde_json`).
#[derive(Debug, Clone, Serialize)]
pub struct ImageContent {
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Données image encodées base64.
    pub data: String,
    /// Type MIME (`image/png`, `image/jpeg`…).
    #[serde(rename = "mimeType")]
    pub mime_type: String,
}

impl ImageContent {
    /// Image déjà encodée base64.
    #[must_use]
    pub fn new(data: impl Into<String>, mime_type: &str) -> Self {
        Self {
            kind: "image",
            data: data.into(),
            mime_type: mime_type.to_string(),
        }
    }

    /// Image depuis des octets réels — encodés base64 ici, une fois.
    #[must_use]
    pub fn from_bytes(bytes: &[u8], mime_type: &str) -> Self {
        Self::new(encode_base64(bytes), mime_type)
    }
}

/// Audio fourni à ou rendu par un LLM (`AudioContent` du schéma MCP) —
/// mêmes conventions que [`ImageContent`] : `data` est base64.
#[derive(Debug, Clone, Serialize)]
pub struct AudioContent {
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Données audio encodées base64.
    pub data: String,
    /// Type MIME (`audio/wav`, `audio/mpeg`…).
    #[serde(rename = "mimeType")]
    pub mime_type: String,
}

impl AudioContent {
    /// Audio déjà encodé base64.
    #[must_use]
    pub fn new(data: impl Into<String>, mime_type: &str) -> Self {
        Self {
            kind: "audio",
            data: data.into(),
            mime_type: mime_type.to_string(),
        }
    }

    /// Audio depuis des octets réels — encodés base64 ici, une fois.
    #[must_use]
    pub fn from_bytes(bytes: &[u8], mime_type: &str) -> Self {
        Self::new(encode_base64(bytes), mime_type)
    }
}

/// Lien vers une ressource que le serveur sait lire, inclus dans un
/// résultat d'outil (`ResourceLinkContent` du schéma — `"resource_link"`).
///
/// Un lien renvoyé par un outil n'a pas d'obligation d'apparaître dans
/// `resources/list` : c'est une référence, pas une entrée de catalogue.
#[derive(Debug, Clone, Serialize)]
pub struct ResourceLinkContent {
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// URI de la ressource.
    pub uri: String,
    /// Nom programmatique.
    pub name: String,
    /// Titre lisible — omis du fil tant qu'il n'est pas posé.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Taille du contenu brut, en octets (avant encodage base64), si connue.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

impl ResourceLinkContent {
    /// Lien minimal — `uri` + `name` (champs requis du fil).
    #[must_use]
    pub fn new(uri: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            kind: "resource_link",
            uri: uri.into(),
            name: name.into(),
            title: None,
            description: None,
            mime_type: None,
            size: None,
        }
    }

    /// Titre lisible (chaînable).
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Description (chaînable).
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Type MIME (chaînable).
    #[must_use]
    pub fn mime_type(mut self, mime_type: impl Into<String>) -> Self {
        self.mime_type = Some(mime_type.into());
        self
    }

    /// Taille brute en octets (chaînable).
    #[must_use]
    pub fn size(mut self, size: u64) -> Self {
        self.size = Some(size);
        self
    }
}

/// Ressource embarquée dans un résultat d'outil ou de prompt
/// (`EmbeddedResourceContent` du schéma — `"resource"`) : le client choisit
/// comment la rendre au LLM ou à l'utilisateur.
#[derive(Debug, Clone, Serialize)]
pub struct EmbeddedResourceContent {
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Contenu de la ressource — texte ([`ResourceData::Text`]) ou
    /// binaire ([`ResourceData::Blob`]).
    pub resource: ResourceData,
}

/// Contenu d'une ressource — texte ([`ResourceContents`]) ou binaire
/// ([`BlobResourceContents`]). Union du `contents[]` de `resources/read`
/// et du champ `resource` d'une ressource embarquée.
///
/// La sérialisation est **sans tag** : chaque variante porte sa propre
/// forme fil (`text` ou `blob`, jamais les deux).
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ResourceData {
    /// Ressource textuelle.
    Text(ResourceContents),
    /// Ressource binaire — `blob` est base64.
    Blob(BlobResourceContents),
}

impl ResourceData {
    /// Ressource textuelle.
    #[must_use]
    pub fn text(
        uri: impl Into<String>,
        mime_type: Option<String>,
        text: impl Into<String>,
    ) -> Self {
        Self::Text(ResourceContents {
            uri: uri.into(),
            mime_type,
            text: text.into(),
        })
    }

    /// Ressource binaire depuis des **octets réels** — encodés base64 ici,
    /// une seule fois (jamais une copie texte du binaire).
    #[must_use]
    pub fn binary(uri: impl Into<String>, mime_type: Option<String>, bytes: &[u8]) -> Self {
        Self::Blob(BlobResourceContents {
            uri: uri.into(),
            mime_type,
            blob: encode_base64(bytes),
        })
    }

    /// Ressource binaire déjà encodée base64.
    #[must_use]
    pub fn blob(
        uri: impl Into<String>,
        mime_type: Option<String>,
        base64: impl Into<String>,
    ) -> Self {
        Self::Blob(BlobResourceContents {
            uri: uri.into(),
            mime_type,
            blob: base64.into(),
        })
    }
}

impl From<ResourceContents> for ResourceData {
    fn from(contents: ResourceContents) -> Self {
        Self::Text(contents)
    }
}

impl From<BlobResourceContents> for ResourceData {
    fn from(contents: BlobResourceContents) -> Self {
        Self::Blob(contents)
    }
}

/// Contenu **binaire** d'une ressource (`BlobResourceContents` du schéma
/// MCP) : `uri` + `mimeType` optionnel + `blob` **base64** — un binaire
/// part tel quel, jamais sérialisé en texte JSON pour simuler le support.
#[derive(Debug, Clone, Serialize)]
pub struct BlobResourceContents {
    pub uri: String,
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Données binaires encodées base64.
    pub blob: String,
}

/// Encodage base64 (alphabet standard, avec padding) — **interne** : le
/// crate garde exactement deux dépendances (`serde`, `serde_json`),
/// l'encodage des charges binaires (images, audio, blobs) vit ici.
pub(crate) fn encode_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18) as usize & 0x3F] as char);
        out.push(ALPHABET[(n >> 12) as usize & 0x3F] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 0x3F] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Bloc de contenu MCP — chaque variante autorisée dans le `content[]`
/// d'un résultat d'outil et dans le `content` d'un message de prompt
/// (`ContentBlock` du schéma 2026-07-28).
///
/// La sérialisation est **sans tag** : chaque struct interne porte son
/// propre discriminant fil (`"type": "text" | "image" | "audio" |
/// "resource_link" | "resource"`) — le JSON émis est exactement la forme
/// de la variante, sans enveloppe. Les constructions passent par les
/// constructeurs ([`ContentBlock::text`], [`ContentBlock::image_bytes`]…)
/// ou par `From` (`"texte".into()`, `ImageContent::new(..).into()`).
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum ContentBlock {
    /// Texte.
    Text(TextContent),
    /// Image encodée base64.
    Image(ImageContent),
    /// Audio encodé base64.
    Audio(AudioContent),
    /// Lien vers une ressource lisible.
    ResourceLink(ResourceLinkContent),
    /// Ressource embarquée.
    EmbeddedResource(EmbeddedResourceContent),
}

impl ContentBlock {
    /// Bloc texte.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(TextContent::new(text))
    }

    /// Image déjà encodée base64.
    #[must_use]
    pub fn image(data: impl Into<String>, mime_type: &str) -> Self {
        Self::Image(ImageContent::new(data, mime_type))
    }

    /// Image depuis des octets réels (encodage base64 interne).
    #[must_use]
    pub fn image_bytes(bytes: &[u8], mime_type: &str) -> Self {
        Self::Image(ImageContent::from_bytes(bytes, mime_type))
    }

    /// Audio déjà encodé base64.
    #[must_use]
    pub fn audio(data: impl Into<String>, mime_type: &str) -> Self {
        Self::Audio(AudioContent::new(data, mime_type))
    }

    /// Audio depuis des octets réels (encodage base64 interne).
    #[must_use]
    pub fn audio_bytes(bytes: &[u8], mime_type: &str) -> Self {
        Self::Audio(AudioContent::from_bytes(bytes, mime_type))
    }

    /// Lien de ressource minimal.
    #[must_use]
    pub fn resource_link(uri: impl Into<String>, name: impl Into<String>) -> Self {
        Self::ResourceLink(ResourceLinkContent::new(uri, name))
    }

    /// Ressource embarquée textuelle.
    #[must_use]
    pub fn embedded_text(
        uri: impl Into<String>,
        mime_type: Option<String>,
        text: impl Into<String>,
    ) -> Self {
        Self::EmbeddedResource(EmbeddedResourceContent {
            kind: "resource",
            resource: ResourceData::text(uri, mime_type, text),
        })
    }

    /// Ressource embarquée binaire depuis des octets réels (base64 interne).
    #[must_use]
    pub fn embedded_binary(
        uri: impl Into<String>,
        mime_type: Option<String>,
        bytes: &[u8],
    ) -> Self {
        Self::EmbeddedResource(EmbeddedResourceContent {
            kind: "resource",
            resource: ResourceData::binary(uri, mime_type, bytes),
        })
    }

    /// Ressource embarquée binaire déjà encodée base64.
    #[must_use]
    pub fn embedded_blob(
        uri: impl Into<String>,
        mime_type: Option<String>,
        base64: impl Into<String>,
    ) -> Self {
        Self::EmbeddedResource(EmbeddedResourceContent {
            kind: "resource",
            resource: ResourceData::blob(uri, mime_type, base64),
        })
    }
}

impl From<&str> for ContentBlock {
    fn from(text: &str) -> Self {
        Self::text(text)
    }
}

impl From<String> for ContentBlock {
    fn from(text: String) -> Self {
        Self::text(text)
    }
}

impl From<TextContent> for ContentBlock {
    fn from(content: TextContent) -> Self {
        Self::Text(content)
    }
}

impl From<ImageContent> for ContentBlock {
    fn from(content: ImageContent) -> Self {
        Self::Image(content)
    }
}

impl From<AudioContent> for ContentBlock {
    fn from(content: AudioContent) -> Self {
        Self::Audio(content)
    }
}

impl From<ResourceLinkContent> for ContentBlock {
    fn from(content: ResourceLinkContent) -> Self {
        Self::ResourceLink(content)
    }
}

impl From<EmbeddedResourceContent> for ContentBlock {
    fn from(content: EmbeddedResourceContent) -> Self {
        Self::EmbeddedResource(content)
    }
}

/// Résultat de `tools/call` (MCP : `content` + `isError` + `structuredContent`).
///
/// `content` porte des **blocs multiples et mixtes** (spec 2026-07-28 :
/// texte, image, audio, lien de ressource, ressource embarquée) — la
/// construction passe par [`CallToolResult::from_content`] ou par l'API
/// Value historique ([`CallToolResult::from_structured`],
/// `From<Result<Value, ToolError>>`) : les deux restent sources-compatibles.
///
/// Conformité `2026-07-28` : porte `resultType` (SEP-2322) et s'identifie
/// sous `_meta` (SEP-2575) — le champ est posé par le noyau après
/// construction, [`CallToolResult::from`] le laisse à `None`.
#[derive(Debug, Serialize)]
pub struct CallToolResult {
    /// Type de résultat — **obligatoire** (SEP-2322).
    #[serde(rename = "resultType")]
    pub result_type: ResultType,
    /// Blocs de contenu — mixtes et multiples (texte, image, audio,
    /// `resource_link`, ressource embarquée).
    pub content: Vec<ContentBlock>,
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
            content: vec![ContentBlock::text(structured.to_string())],
            is_error: None,
            structured_content: Some(structured),
            meta: None,
        }
    }

    /// Construit un résultat de **succès** à partir de blocs de contenu —
    /// le chemin des contenus riches MCP : texte, image, audio,
    /// `resource_link`, ressource embarquée, en nombre et mélange libres.
    ///
    /// `structuredContent` reste **absent** : un résultat de contenu n'a
    /// pas de sortie structurée déclarée — l'`outputSchema` de l'outil, s'il
    /// existe, ne s'applique qu'au champ `structuredContent` explicite.
    /// Le `_meta` d'identité serveur est posé par le noyau au dispatch.
    ///
    /// ```
    /// use litemcp::protocol::{CallToolResult, ContentBlock, ImageContent};
    ///
    /// let result = CallToolResult::from_content([
    ///     ContentBlock::text("Capture d'écran :"),
    ///     ContentBlock::image_bytes(&[0x89, 0x50, 0x4E, 0x47], "image/png"),
    /// ]);
    /// assert_eq!(result.content.len(), 2);
    /// ```
    #[must_use]
    pub fn from_content(blocks: impl IntoIterator<Item = impl Into<ContentBlock>>) -> Self {
        Self {
            result_type: ResultType::Complete,
            content: blocks.into_iter().map(Into::into).collect(),
            is_error: None,
            structured_content: None,
            meta: None,
        }
    }

    /// Construit un résultat d'**échec d'exécution** (`isError: true`) à
    /// partir d'une erreur structurée.
    ///
    /// Forme fil : le `content` textuel porte le message seul (compatible
    /// avec les clients qui ne lisent que le texte), et
    /// `structuredContent.error` porte l'erreur machine-lisible (`code`,
    /// `retryable`, `retryAfterMs`) — voir [`crate::error`]. Le
    /// `structuredContent` d'un échec n'a pas à conformer à l'`outputSchema`
    /// publié : il décrit l'échec, pas une sortie réussie.
    #[must_use]
    pub fn from_tool_error(error: &ToolError) -> Self {
        Self {
            result_type: ResultType::Complete,
            content: vec![ContentBlock::text(error.message.clone())],
            is_error: Some(true),
            structured_content: Some(json!({ "error": error.to_json() })),
            meta: None,
        }
    }
}

impl From<Vec<ContentBlock>> for CallToolResult {
    fn from(blocks: Vec<ContentBlock>) -> Self {
        Self::from_content(blocks)
    }
}

impl From<ContentBlock> for CallToolResult {
    fn from(block: ContentBlock) -> Self {
        Self::from_content([block])
    }
}

impl From<Result<Value, ToolError>> for CallToolResult {
    fn from(result: Result<Value, ToolError>) -> Self {
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
                    json!({ "result": value })
                };
                Self::from_structured(structured)
            }
            Err(error) => Self::from_tool_error(&error),
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

/// Contenu **textuel** d'une ressource lue (`contents[]` de `resources/read`).
///
/// Pour du **binaire**, le handler renvoie [`ResourceData::binary`] ou
/// [`ResourceData::blob`] ([`BlobResourceContents`]) — jamais un texte qui
/// simulerait le binaire. Les deux formes se déclarent via [`ResourceData`].
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
    /// Contenus lus — textuels ([`ResourceContents`]) ou binaires
    /// ([`BlobResourceContents`]), mélanges libres.
    pub contents: Vec<ResourceData>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

/// Gabarit de ressource annoncé par `resources/templates/list` — une URI
/// template RFC 6570 (`test://template/{id}/data`) décrit une **famille**
/// de ressources. La lecture reste celle de `resources/read` (le registre
/// du noyau matche des URIs exactes : la substitution de gabarit côté noyau
/// n'est pas implémentée — voir les limites documentées).
#[derive(Debug, Clone, Serialize)]
pub struct ResourceTemplate {
    /// Gabarit d'URI (RFC 6570) — clé du catalogue.
    #[serde(rename = "uriTemplate")]
    pub uri_template: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Type MIME commun à toutes les ressources du gabarit, si connu.
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// Résultat de `resources/templates/list` — mêmes indices de cache que
/// `resources/list` (SEP-2549).
#[derive(Debug, Serialize)]
pub struct ResourceTemplatesListResult {
    #[serde(rename = "resultType")]
    pub result_type: ResultType,
    #[serde(rename = "ttlMs")]
    pub ttl_ms: u64,
    #[serde(rename = "cacheScope")]
    pub cache_scope: CacheScope,
    #[serde(rename = "resourceTemplates")]
    pub resource_templates: Vec<ResourceTemplate>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

// --- Prompts ---

/// Argument annoncé par un prompt dans `prompts/list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PromptArgument {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
}

/// Entrée du catalogue `prompts/list`.
#[derive(Debug, Clone, Serialize)]
pub struct Prompt {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Vec<PromptArgument>>,
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

// (Le type `PromptContent` du texte de prompt a été remplacé par
// `ContentBlock` — les formes texte passent par `PromptMessage::user`/
// `assistant` sans changement.)

/// Message d'un prompt (`role` : `"user"` ou `"assistant"`).
///
/// Le `content` est un [`ContentBlock`] complet (spec 2026-07-28) : texte,
/// image, audio, lien de ressource ou **ressource embarquée** — les
/// constructeurs [`PromptMessage::user`]/[`PromptMessage::assistant`]
/// conservent la forme texte historique, [`PromptMessage::user_content`]
/// et [`PromptMessage::assistant_content`] ouvrent les blocs riches.
#[derive(Debug, Clone, Serialize)]
pub struct PromptMessage {
    pub role: &'static str,
    pub content: ContentBlock,
}

impl PromptMessage {
    /// Message au rôle `user` — contenu texte.
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: "user",
            content: ContentBlock::text(text),
        }
    }

    /// Message au rôle `assistant` — contenu texte.
    #[must_use]
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: "assistant",
            content: ContentBlock::text(text),
        }
    }

    /// Message au rôle `user` avec un bloc de contenu quelconque (image,
    /// ressource embarquée, lien…).
    #[must_use]
    pub fn user_content(content: impl Into<ContentBlock>) -> Self {
        Self {
            role: "user",
            content: content.into(),
        }
    }

    /// Message au rôle `assistant` avec un bloc de contenu quelconque.
    #[must_use]
    pub fn assistant_content(content: impl Into<ContentBlock>) -> Self {
        Self {
            role: "assistant",
            content: content.into(),
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

// --- Completion (`completion/complete`) ---

/// Référence cible d'une demande de complétion (`ref` de
/// `CompleteRequestParams`) : un prompt (`"ref/prompt"`) ou un gabarit de
/// ressource (`"ref/resource"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type")]
pub enum CompleteReference {
    /// Complétion des arguments d'un prompt.
    #[serde(rename = "ref/prompt")]
    Prompt {
        /// Nom du prompt.
        name: String,
        /// Titre lisible, s'il est connu — omis sinon.
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// Complétion des variables d'un gabarit de ressource.
    #[serde(rename = "ref/resource")]
    Resource {
        /// URI ou URI template (RFC 6570) du gabarit.
        uri: String,
    },
}

/// Argument en cours de complétion (`argument` de `CompleteRequestParams`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompletionArgument {
    /// Nom de l'argument.
    pub name: String,
    /// Valeur partielle à compléter.
    pub value: String,
}

/// Demande de complétion transmise au fournisseur
/// (`ServerBuilder::completions`) — forme typée des `params` de
/// `completion/complete` après validation.
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    /// Cible : prompt ou gabarit de ressource.
    pub reference: CompleteReference,
    /// Argument en cours de complétion.
    pub argument: CompletionArgument,
    /// Variables déjà résolues (`context.arguments`) — **vide** si absentes.
    pub context: BTreeMap<String, String>,
}

/// Suggestions de complétion (`completion` de `CompleteResult`).
///
/// La spec borne `values` à **100 chaînes** : le noyau tronque au-delà et
/// pose `hasMore: true` si le fournisseur ne l'a pas déjà fait.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Completion {
    /// Suggestions, ordonnées par pertinence — au plus 100.
    pub values: Vec<String>,
    /// Nombre total d'options disponibles, si connu — omis sinon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    /// D'autres options existent-elles au-delà de `values` ? — omis sinon.
    #[serde(rename = "hasMore", skip_serializing_if = "Option::is_none")]
    pub has_more: Option<bool>,
}

impl Completion {
    /// Suggestions seules (`total`/`hasMore` absents du fil).
    #[must_use]
    pub fn new(values: Vec<String>) -> Self {
        Self {
            values,
            total: None,
            has_more: None,
        }
    }

    /// Nombre total d'options disponibles (chaînable).
    #[must_use]
    pub fn total(mut self, total: u64) -> Self {
        self.total = Some(total);
        self
    }

    /// Marque la présence d'options au-delà de `values` (chaînable).
    #[must_use]
    pub fn has_more(mut self, has_more: bool) -> Self {
        self.has_more = Some(has_more);
        self
    }
}

impl From<Vec<String>> for Completion {
    fn from(values: Vec<String>) -> Self {
        Self::new(values)
    }
}

/// Résultat de `completion/complete` (`CompleteResult` du schéma —
/// `resultType` SEP-2322, `_meta` SEP-2575 posé par le noyau).
#[derive(Debug, Serialize)]
pub struct CompleteResult {
    #[serde(rename = "resultType")]
    pub result_type: ResultType,
    pub completion: Completion,
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
///
/// Conformité `2026-07-28` : la page *Caching* exige `ttlMs` et `cacheScope`
/// sur tout résultat `resultType: "complete"` de `server/discover`, comme sur
/// `tools/list` — mêmes valeurs, même mécanisme (SEP-2549).
#[derive(Debug, Serialize)]
pub struct DiscoverResult {
    /// Type de résultat — **obligatoire** (SEP-2322).
    #[serde(rename = "resultType")]
    pub result_type: ResultType,
    /// Durée de vie conseillée des capacités, en millisecondes (SEP-2549).
    #[serde(rename = "ttlMs")]
    pub ttl_ms: u64,
    /// Portée de cache (SEP-2549).
    #[serde(rename = "cacheScope")]
    pub cache_scope: CacheScope,
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
        let result = CallToolResult::from(Ok::<Value, ToolError>(json!(42)));
        let v = serde_json::to_value(&result).unwrap();
        assert_eq!(v["structuredContent"], json!({ "result": 42 }));
        assert_eq!(v["content"][0]["text"], r#"{"result":42}"#);
        assert_eq!(v["resultType"], "complete");

        let result = CallToolResult::from(Ok::<Value, ToolError>(Value::Null));
        let v = serde_json::to_value(&result).unwrap();
        assert_eq!(v["structuredContent"], json!({ "result": null }));

        let result = CallToolResult::from(Ok::<Value, ToolError>(json!([1, 2])));
        let v = serde_json::to_value(&result).unwrap();
        assert_eq!(v["structuredContent"], json!({ "result": [1, 2] }));

        // Un objet passe tel quel, sans enveloppe.
        let result = CallToolResult::from(Ok::<Value, ToolError>(json!({ "somme": 12 })));
        let v = serde_json::to_value(&result).unwrap();
        assert_eq!(v["structuredContent"], json!({ "somme": 12 }));
        assert_eq!(v["content"][0]["text"], r#"{"somme":12}"#);
    }

    #[test]
    fn tool_error_result_carries_a_structured_error() {
        // L'échec d'exécution garde le message en texte (compatibilité
        // clients texte) ET l'erreur machine-lisible sous
        // structuredContent.error (réaction programmatique du client).
        let error = ToolError::rate_limited("quota exhausted").retry_after_ms(500);
        let result = CallToolResult::from(Err::<Value, ToolError>(error));
        let v = serde_json::to_value(&result).unwrap();
        assert_eq!(v["isError"], true);
        assert_eq!(v["content"][0]["text"], "quota exhausted");
        assert_eq!(v["structuredContent"]["error"]["code"], "rate_limited");
        assert_eq!(v["structuredContent"]["error"]["retryable"], true);
        assert_eq!(v["structuredContent"]["error"]["retryAfterMs"], 500);
    }

    #[test]
    fn every_result_carries_a_result_type() {
        // SEP-2322 : resultType est OBLIGATOIRE dans chaque result.
        let call = CallToolResult::from(Ok::<Value, ToolError>(json!({})));
        let v = serde_json::to_value(&call).unwrap();
        assert_eq!(v["resultType"], "complete");
    }

    #[test]
    fn base64_encoding_matches_known_vectors() {
        assert_eq!(encode_base64(&[]), "");
        assert_eq!(encode_base64(&[0xDE, 0xAD, 0xBE, 0xEF]), "3q2+7w==");
        assert_eq!(encode_base64(&[0x89, 0x50, 0x4E, 0x47]), "iVBORw==");
        assert_eq!(encode_base64(&[0xFF]), "/w==");
        assert_eq!(encode_base64(&[0xFF, 0xFE]), "//4=");
        assert_eq!(
            encode_base64(b"any carnal pleasure."),
            "YW55IGNhcm5hbCBwbGVhc3VyZS4="
        );
        assert_eq!(
            encode_base64(b"any carnal pleasure"),
            "YW55IGNhcm5hbCBwbGVhc3VyZQ=="
        );
    }

    #[test]
    fn every_content_block_serializes_to_its_exact_wire_shape() {
        // Chaque variante émet exactement sa forme `ContentBlock` du schéma
        // (pas d'enveloppe serde autour de l'énumération sans tag).
        let blocks = [
            ContentBlock::text("bonjour"),
            ContentBlock::image("aW1n", "image/png"),
            ContentBlock::audio("YXVk", "audio/wav"),
            ContentBlock::resource_link("mem://doc", "Doc"),
            ContentBlock::embedded_text("mem://note", Some("text/plain".into()), "note"),
        ];
        let v = serde_json::to_value(&blocks).unwrap();
        assert_eq!(
            v,
            json!([
                { "type": "text", "text": "bonjour" },
                { "type": "image", "data": "aW1n", "mimeType": "image/png" },
                { "type": "audio", "data": "YXVk", "mimeType": "audio/wav" },
                { "type": "resource_link", "uri": "mem://doc", "name": "Doc" },
                { "type": "resource", "resource": { "uri": "mem://note", "mimeType": "text/plain", "text": "note" } },
            ])
        );
    }

    #[test]
    fn resource_link_optional_fields_are_omitted_until_set() {
        let link = ResourceLinkContent::new("mem://big", "Big")
            .title("Gros fichier")
            .mime_type("application/octet-stream")
            .size(1024);
        let v = serde_json::to_value(&link).unwrap();
        assert_eq!(
            v,
            json!({
                "type": "resource_link",
                "uri": "mem://big",
                "name": "Big",
                "title": "Gros fichier",
                "mimeType": "application/octet-stream",
                "size": 1024,
            })
        );
        assert!(v.get("description").is_none());
    }

    #[test]
    fn blob_resource_contents_serializes_without_a_text_field() {
        let data = ResourceData::binary("test://bin", Some("image/png".into()), &[0xFF, 0xFE]);
        let v = serde_json::to_value(&data).unwrap();
        assert_eq!(
            v,
            json!({ "uri": "test://bin", "mimeType": "image/png", "blob": "//4=" })
        );
        assert!(v.get("text").is_none(), "un blob ne porte jamais de texte");
    }

    #[test]
    fn from_content_keeps_empty_list_and_omits_structured() {
        let result = CallToolResult::from_content(Vec::<ContentBlock>::new());
        let v = serde_json::to_value(&result).unwrap();
        assert_eq!(v["content"], json!([]), "content requis, même vide");
        assert!(v.get("structuredContent").is_none());
        assert!(v.get("isError").is_none());
        assert_eq!(v["resultType"], "complete");
    }

    #[test]
    fn completion_wire_shape_omits_unset_hints() {
        let bare = serde_json::to_value(Completion::new(vec!["a".into()])).unwrap();
        assert_eq!(bare, json!({ "values": ["a"] }));

        let full = Completion::new(vec!["a".into(), "b".into()])
            .total(150)
            .has_more(true);
        let v = serde_json::to_value(&full).unwrap();
        assert_eq!(
            v,
            json!({ "values": ["a", "b"], "total": 150, "hasMore": true })
        );
    }

    #[test]
    fn complete_reference_tags_are_exact() {
        let prompt = serde_json::to_value(CompleteReference::Prompt {
            name: "p".into(),
            title: None,
        })
        .unwrap();
        assert_eq!(prompt, json!({ "type": "ref/prompt", "name": "p" }));

        let resource = serde_json::to_value(CompleteReference::Resource {
            uri: "test://tpl/{id}".into(),
        })
        .unwrap();
        assert_eq!(
            resource,
            json!({ "type": "ref/resource", "uri": "test://tpl/{id}" })
        );
    }
}
