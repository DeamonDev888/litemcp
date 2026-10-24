//! Noyau de dispatch JSON-RPC + builder — indépendant du transport.

use crate::jsonrpc::{ErrorObject, Outgoing, RequestId};
use crate::protocol;
use crate::protocol::{
    CacheScope, CallToolResult, DiscoverResult, GetPromptResult, Prompt, PromptMessage,
    PromptsListResult, ReadResourceResult, Resource, ResourceContents, ResourcesListResult,
    ResultType, ServerInfo, Tool, ToolAnnotations, ToolsListResult,
};
use crate::zod::{Issues, Schema};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

/// Résultat d'un outil : `Ok(valeur)` ou `Err(message lisible par le client)`.
///
/// À l'ok, la valeur devient `structuredContent` : les objets JSON passent
/// tels quels ; toute autre valeur (scalaire, tableau, `null`) est enveloppée
/// dans `{ "result": … }` pour rester un objet invariant (choix d'API).
pub type ToolResult = Result<Value, String>;

type ToolFn = dyn Fn(&Value) -> Result<Value, ToolFailure> + Send + Sync;
type ResourceFn = dyn Fn() -> Result<ResourceContents, String> + Send + Sync;
type PromptFn = dyn Fn(&Value) -> Result<Vec<PromptMessage>, String> + Send + Sync;

/// Objet vide partagé — utilisé quand `params` ou `arguments` sont absents,
/// sans réallouer à chaque appel.
static EMPTY_OBJECT: std::sync::OnceLock<Value> = std::sync::OnceLock::new();

fn empty_object() -> &'static Value {
    EMPTY_OBJECT.get_or_init(|| json!({}))
}

/// Durée de vie conseillée des catalogues, en millisecondes (`ttlMs`,
/// SEP-2549) — s'applique à `tools/list`, `resources/list`,
/// `resources/read` et `prompts/list`.
pub const DEFAULT_LIST_TTL_MS: u64 = 60_000;

/// Code d'erreur implementation-defined (plage `-32000`..`-32019` réservée
/// aux implémentations par la spec 2026-07-28 ; `-32020`+ est réservé à la
/// spec) : échec de lecture d'une ressource ou d'un prompt.
const RESOURCE_FAILURE: i64 = -32000;

/// Issue d'une exécution bornée par [`ServerBuilder::tool_timeout`].
enum Outcome<T> {
    /// Terminé dans le délai.
    Done(T),
    /// Le handler a paniqué (rattrapé dans son thread) ou le thread n'a pas
    /// pu être créé.
    Panicked,
    /// Délai dépassé — le thread du handler **continue** en arrière-plan
    /// (Rust ne peut pas tuer un thread), son résultat sera ignoré.
    TimedOut,
}

/// Exécute `f` borné par `timeout` — dans un thread détaché.
///
/// En cas de timeout, le thread de `f` n'est **pas** interrompu (impossible
/// en Rust) : il termine à son rythme et son résultat est ignoré. La
/// panique de `f` est rattrapée dans son thread et rendue comme
/// [`Outcome::Panicked`].
fn run_with_timeout<T: Send + 'static>(
    timeout: Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> Outcome<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("litemcp-handler".into())
        .spawn(move || {
            let _ = tx.send(std::panic::catch_unwind(AssertUnwindSafe(f)));
        });
    if spawned.is_err() {
        return Outcome::Panicked;
    }
    match rx.recv_timeout(timeout) {
        Ok(Ok(value)) => Outcome::Done(value),
        Ok(Err(_)) => Outcome::Panicked,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Outcome::TimedOut,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Outcome::Panicked,
    }
}

/// Échec remonté par un wrapper de handler :
/// - `Tool(String)` → échec d'exécution (`isError: true`, HTTP 200) ;
/// - `Protocol(i64, String)` → erreur de protocole (ex. arguments qui ne
///   désérialisent pas dans le struct typé → `-32602`).
#[derive(Debug)]
enum ToolFailure {
    Tool(String),
    Protocol(i64, String),
}

#[derive(Clone)]
struct ToolDef {
    description: Option<String>,
    schema: Option<Arc<dyn Schema>>,
    output_schema: Option<Arc<dyn Schema>>,
    annotations: Option<ToolAnnotations>,
    f: Arc<ToolFn>,
}

/// Définition d'une ressource (clé : `uri`).
#[derive(Clone)]
struct ResourceDef {
    name: String,
    description: Option<String>,
    f: Arc<ResourceFn>,
}

/// Définition d'un prompt (clé : `name`).
#[derive(Clone)]
struct PromptDef {
    description: Option<String>,
    f: Arc<PromptFn>,
}

/// En-têtes de routage MCP 2026-07-28 (SEP-2243).
///
/// `Mcp-Method` et `Mcp-Name` permettent à un gateway de router et
/// d'autoriser sans analyser le corps JSON. Optionnels chez litemcp
/// (compatibilité) mais vérifiés quand ils sont présents.
#[derive(Debug, Clone, Default)]
pub struct McpHeaders {
    /// Méthode MCP annoncée par l'en-tête `Mcp-Method`.
    pub method: Option<String>,
    /// Nom de l'outil annoncé par l'en-tête `Mcp-Name`.
    pub name: Option<String>,
}

/// Cœur de dispatch MCP stateless, réutilisable sans le transport HTTP.
#[derive(Clone)]
pub struct ServerCore {
    name: String,
    version: String,
    tools: Arc<BTreeMap<String, ToolDef>>,
    resources: Arc<BTreeMap<String, ResourceDef>>,
    prompts: Arc<BTreeMap<String, PromptDef>>,
    /// Délai maximal d'exécution des handlers (tools, resources, prompts) —
    /// `None` : illimité (comportement historique).
    tool_timeout: Option<Duration>,
}

impl ServerCore {
    /// Traite un corps de requête JSON-RPC.
    ///
    /// Renvoie `None` pour une notification (aucune réponse attendue —
    /// le transport répondra `202 Accepted` sans corps).
    ///
    /// Conforme JSON-RPC : `id: null` explicite reçoit une réponse avec
    /// `id: null` ; version `jsonrpc` invalide ou lot → `-32600` ; JSON
    /// illisible → `-32700`. **L'`id` lisible est toujours repris** dans la
    /// réponse d'erreur (JSON-RPC 2.0 §5).
    ///
    /// Conforme MCP `2026-07-28` : `server/discover` est implémentée, chaque
    /// `result` porte `resultType`, et une version de protocole non supportée
    /// est refusée en `-32022`.
    pub fn dispatch(&self, body: &[u8]) -> Option<Vec<u8>> {
        self.dispatch_with_headers(body, None)
    }

    /// Idem [`ServerCore::dispatch`], avec les en-têtes de routage MCP
    /// (SEP-2243) pour vérifier leur cohérence avec le corps de la requête.
    ///
    /// `Mcp-Method` / `Mcp-Name` sont **optionnels** chez nous (compatibilité
    /// avec les clients qui les n'envoient pas) mais s'ils sont présents ils
    /// doivent correspondre à la méthode et à l'outil du corps — sinon un
    /// gateway qui route sur ces en-têtes désynchronise le routage.
    pub fn dispatch_with_headers(
        &self,
        body: &[u8],
        headers: Option<&McpHeaders>,
    ) -> Option<Vec<u8>> {
        let incoming = match crate::jsonrpc::parse_incoming(body) {
            Ok(incoming) => incoming,
            Err(crate::jsonrpc::ParseFail::Syntax) => {
                return Some(Outgoing::parse_error().to_vec());
            }
            Err(fail) => {
                // L'id est repris quand il a pu être lu (JSON-RPC 2.0 §5).
                let id = fail.id().cloned();
                let message = match fail {
                    crate::jsonrpc::ParseFail::Syntax => String::new(),
                    crate::jsonrpc::ParseFail::Invalid { message, .. } => message,
                };
                return Some(
                    Outgoing::invalid_request(id, format!("requête invalide : {message}")).to_vec(),
                );
            }
        };
        if incoming.is_notification() {
            return None;
        }
        // id réel, ou null explicite : les deux exigent une réponse ; l'id
        // null explicite est rendu tel quel (Value::Null).
        let id = incoming
            .id
            .as_ref()
            .map(RequestId::to_value)
            .unwrap_or(Value::Null);

        let params = incoming.params.as_ref().unwrap_or_else(|| empty_object());

        // Cohérence des en-têtes de routage MCP (SEP-2243) : `Mcp-Method` et
        // `Mcp-Name` doivent refléter le corps de la requête. Un gateway
        // qui route sur ces en-têtes verrait sinon `tools/call` arriver
        // comme `ping`.
        if let Some(headers) = headers {
            if let Some(header_method) = &headers.method {
                if !header_method.eq_ignore_ascii_case(&incoming.method) {
                    return Some(
                        Outgoing::failure(
                            id,
                            ErrorObject::with_data(
                                ErrorObject::HEADER_MISMATCH,
                                "en-tête Mcp-Method incohérent avec le corps de la requête",
                                json!({
                                    "header": header_method,
                                    "body": incoming.method,
                                }),
                            ),
                        )
                        .to_vec(),
                    );
                }
            }
            if let Some(header_name) = &headers.name {
                let body_name = params.get("name").and_then(Value::as_str).unwrap_or("");
                if !body_name.is_empty() && !header_name.eq_ignore_ascii_case(body_name) {
                    return Some(
                        Outgoing::failure(
                            id,
                            ErrorObject::with_data(
                                ErrorObject::HEADER_MISMATCH,
                                "en-tête Mcp-Name incohérent avec le corps de la requête",
                                json!({
                                    "header": header_name,
                                    "body": body_name,
                                }),
                            ),
                        )
                        .to_vec(),
                    );
                }
            }
        }

        // MCP 2026-07-28 (SEP-2575) : la version de protocole voyage dans
        // `_meta` de chaque requête. Une version **explicite** qui n'est pas
        // supportée est refusée en `-32022` ; l'absence de version est
        // tolérée (compatibilité avec les clients legacy `2025-11-25`).
        if let Some(err) = self.check_protocol_version(params) {
            return Some(Outgoing::failure(id, err).to_vec());
        }

        let outgoing = match incoming.method.as_str() {
            protocol::method::DISCOVER => Outgoing::success(
                id,
                serde_json::to_value(self.discover()).unwrap_or(Value::Null),
            ),
            protocol::method::INITIALIZE => Outgoing::success(
                id,
                json!({
                    // `resultType` est obligatoire dans CHAQUE résultat
                    // (SEP-2322) — y compris sur ce résultat legacy.
                    "resultType": "complete",
                    "protocolVersion": protocol::PROTOCOL_VERSION,
                    "capabilities": self.capabilities(),
                    "serverInfo": ServerInfo {
                        name: self.name.clone(),
                        version: self.version.clone(),
                    },
                    "_meta": {
                        protocol::META_SERVER_INFO: ServerInfo {
                            name: self.name.clone(),
                            version: self.version.clone(),
                        }
                    }
                }),
            ),
            // `ping` a été retirée de la spec 2026-07-28 (SEP-2575) mais
            // reste tolérée pour les clients legacy : il ne coûte rien.
            // Le résultat porte néanmoins `resultType` (SEP-2322) et
            // l'identité du serveur (SEP-2575) — « chaque résultat » sans
            // exception.
            protocol::method::PING => Outgoing::success(
                id,
                json!({
                    "resultType": "complete",
                    "_meta": self.meta(),
                }),
            ),
            protocol::method::TOOLS_LIST => Outgoing::success(
                id,
                serde_json::to_value(self.list_tools()).unwrap_or_else(|_| json!({ "tools": [] })),
            ),
            protocol::method::TOOLS_CALL => self.call_tool(&id, params),
            protocol::method::RESOURCES_LIST => Outgoing::success(
                id,
                serde_json::to_value(self.list_resources())
                    .unwrap_or_else(|_| json!({ "resources": [] })),
            ),
            protocol::method::RESOURCES_READ => self.read_resource(&id, params),
            protocol::method::PROMPTS_LIST => Outgoing::success(
                id,
                serde_json::to_value(self.list_prompts())
                    .unwrap_or_else(|_| json!({ "prompts": [] })),
            ),
            protocol::method::PROMPTS_GET => self.get_prompt(&id, params),
            other => Outgoing::failure(
                id,
                ErrorObject::new(
                    ErrorObject::METHOD_NOT_FOUND,
                    format!("Méthode inconnue : {other}"),
                ),
            ),
        };
        Some(outgoing.to_vec())
    }

    /// Vérifie la version de protocole annoncée dans `_meta` (SEP-2575).
    ///
    /// L'absence de version est tolérée (client legacy). Une version
    /// explicite non supportée est refusée en `-32022`
    /// (`UnsupportedProtocolVersionError`) avec la liste des versions
    /// supportées.
    fn check_protocol_version(&self, params: &Value) -> Option<ErrorObject> {
        let requested = params
            .get("_meta")
            .and_then(|meta| meta.get(protocol::META_PROTOCOL_VERSION))
            .and_then(Value::as_str)?;
        if protocol::SUPPORTED_VERSIONS.contains(&requested) {
            return None;
        }
        Some(ErrorObject::with_data(
            ErrorObject::UNSUPPORTED_PROTOCOL_VERSION,
            format!("Version de protocole non supportée : {requested}"),
            json!({
                "supported": protocol::SUPPORTED_VERSIONS,
                "requested": requested,
            }),
        ))
    }

    /// Identité du serveur sous `_meta` (SEP-2575 : les serveurs SHOULD
    /// s'identifier dans chaque résultat).
    fn meta(&self) -> Value {
        json!({
            protocol::META_SERVER_INFO: ServerInfo {
                name: self.name.clone(),
                version: self.version.clone(),
            }
        })
    }

    /// Capacités annoncées par `server/discover` et `initialize` : `tools`
    /// toujours ; `resources`/`prompts` seulement quand leur registre
    /// n'est pas vide (une capacité annoncée est une capacité servie).
    fn capabilities(&self) -> Value {
        let mut capabilities = json!({ "tools": {} });
        if !self.resources.is_empty() {
            capabilities["resources"] = json!({});
        }
        if !self.prompts.is_empty() {
            capabilities["prompts"] = json!({});
        }
        capabilities
    }

    /// Résultat de `server/discover` (**MUST**, SEP-2575).
    fn discover(&self) -> DiscoverResult {
        DiscoverResult {
            result_type: ResultType::Complete,
            supported_versions: protocol::SUPPORTED_VERSIONS.to_vec(),
            capabilities: self.capabilities(),
            meta: json!({
                protocol::META_SERVER_INFO: ServerInfo {
                    name: self.name.clone(),
                    version: self.version.clone(),
                }
            }),
        }
    }

    fn list_tools(&self) -> ToolsListResult {
        let tools = self
            .tools
            .iter()
            .map(|(name, def)| Tool {
                name: name.clone(),
                description: def.description.clone(),
                annotations: def.annotations.clone(),
                input_schema: Some(
                    def.schema
                        .as_ref()
                        .map(|schema| schema.json())
                        .unwrap_or_else(|| json!({ "type": "object" })),
                ),
                output_schema: def.output_schema.as_ref().map(|schema| schema.json()),
            })
            .collect();
        ToolsListResult {
            result_type: ResultType::Complete,
            ttl_ms: DEFAULT_LIST_TTL_MS,
            cache_scope: CacheScope::Private,
            tools,
            meta: self.meta(),
        }
    }

    fn list_resources(&self) -> ResourcesListResult {
        let resources = self
            .resources
            .iter()
            .map(|(uri, def)| Resource {
                uri: uri.clone(),
                name: def.name.clone(),
                description: def.description.clone(),
                mime_type: None,
            })
            .collect();
        ResourcesListResult {
            result_type: ResultType::Complete,
            ttl_ms: DEFAULT_LIST_TTL_MS,
            cache_scope: CacheScope::Private,
            resources,
            meta: Some(self.meta()),
        }
    }

    fn list_prompts(&self) -> PromptsListResult {
        let prompts = self
            .prompts
            .iter()
            .map(|(name, def)| Prompt {
                name: name.clone(),
                description: def.description.clone(),
            })
            .collect();
        PromptsListResult {
            result_type: ResultType::Complete,
            ttl_ms: DEFAULT_LIST_TTL_MS,
            cache_scope: CacheScope::Private,
            prompts,
            meta: Some(self.meta()),
        }
    }

    /// Exécute un handler de ressource, borné par `tool_timeout`.
    fn read_resource(&self, id: &Value, params: &Value) -> Outgoing {
        let Some(uri) = params.get("uri").and_then(Value::as_str) else {
            return Outgoing::failure(
                id.clone(),
                ErrorObject::new(ErrorObject::INVALID_PARAMS, "params.uri manquant"),
            );
        };
        let Some(def) = self.resources.get(uri) else {
            return Outgoing::failure(
                id.clone(),
                ErrorObject::new(
                    ErrorObject::INVALID_PARAMS,
                    format!("Ressource inconnue : {uri}"),
                ),
            );
        };
        let handler = Arc::clone(&def.f);
        let contents = match self.run_bounded(move || handler()) {
            Ok(contents) => contents,
            Err(failure) => return Outgoing::failure(id.clone(), failure),
        };
        match contents {
            Ok(contents) => Outgoing::success(
                id.clone(),
                serde_json::to_value(ReadResourceResult {
                    result_type: ResultType::Complete,
                    ttl_ms: DEFAULT_LIST_TTL_MS,
                    cache_scope: CacheScope::Private,
                    contents: vec![contents],
                    meta: Some(self.meta()),
                })
                .unwrap_or(Value::Null),
            ),
            Err(message) => Outgoing::failure(
                id.clone(),
                ErrorObject::new(RESOURCE_FAILURE, format!("Lecture impossible : {message}")),
            ),
        }
    }

    /// Exécute un handler de prompt, borné par `tool_timeout`.
    fn get_prompt(&self, id: &Value, params: &Value) -> Outgoing {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Outgoing::failure(
                id.clone(),
                ErrorObject::new(ErrorObject::INVALID_PARAMS, "params.name manquant"),
            );
        };
        let Some(def) = self.prompts.get(name) else {
            return Outgoing::failure(
                id.clone(),
                ErrorObject::new(
                    ErrorObject::INVALID_PARAMS,
                    format!("Prompt inconnu : {name}"),
                ),
            );
        };
        // `arguments` : objet ou absent — même contrat que `tools/call`.
        let arguments = match params.get("arguments") {
            None => empty_object(),
            Some(value) if value.is_object() => value,
            Some(_) => {
                return Outgoing::failure(
                    id.clone(),
                    ErrorObject::new(
                        ErrorObject::INVALID_PARAMS,
                        "params.arguments doit être un objet",
                    ),
                );
            }
        };
        let handler = Arc::clone(&def.f);
        let arguments = arguments.clone();
        let messages = match self.run_bounded(move || handler(&arguments)) {
            Ok(messages) => messages,
            Err(failure) => return Outgoing::failure(id.clone(), failure),
        };
        match messages {
            Ok(messages) => Outgoing::success(
                id.clone(),
                serde_json::to_value(GetPromptResult {
                    result_type: ResultType::Complete,
                    description: def.description.clone(),
                    messages,
                    meta: Some(self.meta()),
                })
                .unwrap_or(Value::Null),
            ),
            Err(message) => Outgoing::failure(
                id.clone(),
                ErrorObject::new(RESOURCE_FAILURE, format!("Prompt impossible : {message}")),
            ),
        }
    }

    /// Exécute `f` borné par [`Self::tool_timeout`][core-timeout] quand il
    /// est configuré ; traduit timeout/panique en erreur JSON-RPC.
    ///
    /// [core-timeout]: crate::server::ServerBuilder::tool_timeout
    fn run_bounded<T: Send + 'static>(
        &self,
        f: impl FnOnce() -> Result<T, String> + Send + 'static,
    ) -> Result<Result<T, String>, ErrorObject> {
        match self.tool_timeout {
            None => Ok(f()),
            Some(timeout) => match run_with_timeout(timeout, f) {
                Outcome::Done(result) => Ok(result),
                Outcome::Panicked => Err(ErrorObject::new(
                    ErrorObject::INTERNAL_ERROR,
                    "erreur interne (panique du handler)",
                )),
                Outcome::TimedOut => Err(ErrorObject::new(
                    RESOURCE_FAILURE,
                    format!("délai d'exécution dépassé ({} ms)", timeout.as_millis()),
                )),
            },
        }
    }

    fn call_tool(&self, id: &Value, params: &Value) -> Outgoing {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Outgoing::failure(
                id.clone(),
                ErrorObject::new(ErrorObject::INVALID_PARAMS, "params.name manquant"),
            );
        };
        let Some(tool) = self.tools.get(name) else {
            return Outgoing::failure(
                id.clone(),
                ErrorObject::new(
                    ErrorObject::INVALID_PARAMS,
                    format!("Outil inconnu : {name}"),
                ),
            );
        };
        // `arguments` est un **objet** (MCP : `CallToolRequestParams.arguments`
        // est `{ [key: string]: unknown }`). Un scalaire/tableau/`null` est
        // refusé **même sans schéma** : le contrat de l'API (« la closure
        // reçoit `arguments` (objet JSON) ») doit tenir inconditionnellement.
        let arguments = match params.get("arguments") {
            None => empty_object(),
            Some(value) if value.is_object() => value,
            Some(_) => {
                return Outgoing::failure(
                    id.clone(),
                    ErrorObject::new(
                        ErrorObject::INVALID_PARAMS,
                        "params.arguments doit être un objet",
                    ),
                );
            }
        };
        if let Some(schema) = &tool.schema {
            let mut issues = Issues::new();
            schema.validate("arguments", arguments, &mut issues);
            if !issues.is_empty() {
                let rendered = issues.render();
                return Outgoing::failure(
                    id.clone(),
                    ErrorObject::with_data(
                        ErrorObject::INVALID_PARAMS,
                        format!("Arguments invalides : {}", rendered.join(" ; ")),
                        json!({ "issues": rendered }),
                    ),
                );
            }
        }
        // Exécution du handler, bornée par `tool_timeout` s'il est configuré.
        // En mode borné, les arguments sont clonés une fois (le handler
        // s'exécute dans un thread qui doit les posséder) — coût assumé,
        // uniquement sur ce chemin opt-in. Une panique y est rattrapée dans
        // le thread du handler et rendue en `-32603` (sans timeout, elle
        // remonte au `catch_unwind` du transport — même verdict).
        let execution = match self.tool_timeout {
            None => (tool.f)(arguments),
            Some(timeout) => {
                let owned = arguments.clone();
                let handler = Arc::clone(&tool.f);
                match run_with_timeout(timeout, move || handler(&owned)) {
                    Outcome::Done(result) => result,
                    Outcome::Panicked => Err(ToolFailure::Protocol(
                        ErrorObject::INTERNAL_ERROR,
                        "erreur interne (panique du handler)".to_string(),
                    )),
                    Outcome::TimedOut => Err(ToolFailure::Tool(format!(
                        "l'exécution de l'outil a dépassé le délai imparti ({} ms)",
                        timeout.as_millis()
                    ))),
                }
            }
        };
        let mut result = match execution {
            Ok(value) => {
                let mut call = CallToolResult::from(Ok::<Value, String>(value));
                // `outputSchema` est exécuteur comme l'input : la valeur
                // `structuredContent` réellement envoyée est validée avant
                // de partir — non conforme → résultat `isError` (le handler
                // a exécuté, c'est sa sortie qui viole le contrat publié).
                if let (Some(output), Some(structured)) =
                    (&tool.output_schema, &call.structured_content)
                {
                    let mut issues = Issues::new();
                    output.validate("output", structured, &mut issues);
                    if !issues.is_empty() {
                        let rendered = issues.render().join(" ; ");
                        call = CallToolResult::from(Err::<Value, String>(format!(
                            "sortie non conforme au outputSchema : {rendered}"
                        )));
                    }
                }
                call
            }
            Err(ToolFailure::Tool(message)) => CallToolResult::from(Err::<Value, String>(message)),
            Err(ToolFailure::Protocol(code, message)) => {
                return Outgoing::failure(id.clone(), ErrorObject::new(code, message));
            }
        };
        // SEP-2575 : le serveur s'identifie dans chaque résultat — succès
        // comme échec d'exécution (`isError: true` reste un `result`).
        result.meta = Some(self.meta());
        Outgoing::success(
            id.clone(),
            serde_json::to_value(result).unwrap_or(Value::Null),
        )
    }
}

/// Builder d'un serveur MCP stateless.
#[derive(Clone)]
pub struct ServerBuilder {
    name: String,
    version: String,
    route: String,
    allow_origins: Vec<String>,
    read_timeout: Duration,
    max_connections: usize,
    tools: BTreeMap<String, ToolDef>,
    /// Durée de vie maximale d'une connexion keep-alive.
    connection_ttl: Duration,
    /// Délai maximal d'exécution des handlers — `None` : illimité.
    tool_timeout: Option<Duration>,
    /// Annotations MCP en attente d'association à un outil (SEP-2575).
    ///
    /// Stockées à part pour que l'ordre d'appel n'ait plus d'importance :
    /// `.annotate("x", …)` avant ou après `.tool("x", …)` donne le même
    /// résultat.
    annotations: BTreeMap<String, ToolAnnotations>,
    /// Schémas de sortie en attente — même mécanique indépendante de
    /// l'ordre que [`Self::annotate`].
    output_schemas: BTreeMap<String, Arc<dyn Schema>>,
    resources: BTreeMap<String, ResourceDef>,
    prompts: BTreeMap<String, PromptDef>,
}

impl ServerBuilder {
    /// Nouveau builder — `name`/`version` sont les `serverInfo` MCP.
    #[must_use]
    pub fn new(name: &str, version: &str) -> Self {
        Self {
            name: name.to_string(),
            version: version.to_string(),
            route: "/mcp".to_string(),
            allow_origins: Vec::new(),
            read_timeout: crate::http::READ_TIMEOUT,
            max_connections: crate::http::MAX_CONNECTIONS,
            tools: BTreeMap::new(),
            annotations: BTreeMap::new(),
            output_schemas: BTreeMap::new(),
            resources: BTreeMap::new(),
            prompts: BTreeMap::new(),
            connection_ttl: crate::http::CONNECTION_TTL,
            tool_timeout: None,
        }
    }

    /// Durée de vie maximale d'une connexion keep-alive — défaut 5 min.
    ///
    /// Borne le temps d'occupation d'un slot du plafond de connexions : sans
    /// elle, un client qui envoie une requête valide toutes les 29 s (sous
    /// le `read_timeout`) retient son slot **indéfiniment**, et 256 clients
    /// lents suffisent à renvoyer `503` à tous les clients légitimes.
    ///
    /// Valeur `0` : pas de TTL (comportement historique).
    #[must_use]
    pub fn connection_ttl(mut self, ttl: Duration) -> Self {
        self.connection_ttl = ttl;
        self
    }

    /// Timeout de lecture par opération (anti-slowloris). Défaut : 30 s.
    ///
    /// Un client qui met plus de temps à produire sa requête est déconnecté
    /// sans réponse. Les tests passent une valeur courte.
    #[must_use]
    pub fn read_timeout(mut self, timeout: Duration) -> Self {
        self.read_timeout = timeout;
        self
    }

    /// Plafond de connexions simultanées (au-delà → `503` immédiat).
    /// Défaut : [`crate::http::MAX_CONNECTIONS`] (256).
    #[must_use]
    pub fn max_connections(mut self, max: usize) -> Self {
        self.max_connections = max;
        self
    }

    /// Change la route MCP (par défaut `/mcp`).
    ///
    /// La valeur est **validée au [`ServerBuilder::bind`]** : elle doit
    /// commencer par `/`, ne pas contenir de `?` (la query est ignorée au
    /// routage) et ne pas masquer la sonde de vie
    /// ([`crate::http::HEALTH_ROUTE`]) — sans ce contrôle, un serveur
    /// configurerait silencieusement une route injoignable.
    #[must_use]
    pub fn route(mut self, route: &str) -> Self {
        self.route = route.to_string();
        self
    }

    /// Origines `Origin` autorisées (clients navigateur).
    ///
    /// Par défaut, toute requête portant un en-tête `Origin` est refusée
    /// (garde DNS-rebinding) — les clients CLI n'en envoient pas.
    #[must_use]
    pub fn allow_origins(mut self, origins: &[&str]) -> Self {
        self.allow_origins = origins.iter().map(|o| (*o).to_string()).collect();
        self
    }

    /// Délai maximal d'exécution des **handlers** (outils, ressources,
    /// prompts) — défaut : illimité.
    ///
    /// Un handler qui dépasse le délai rend un résultat d'échec (`isError`
    /// pour un outil, erreur JSON-RPC `-32000` pour une ressource/prompt)
    /// et la requête est servie — le thread de connexion n'est plus retenu
    /// indéfiniment par du code utilisateur bloquant.
    ///
    /// **Limite assumée** : le thread du handler ne peut pas être tué
    /// (Rust n'a pas d'annulation de thread) — il termine à son rythme en
    /// arrière-plan et son résultat est ignoré. N'utilisez pas ce délai
    /// comme mécanisme d'annulation d'effets de bord : il borne la
    /// **réponse**, pas l'exécution. Quand il est configuré, les arguments
    /// d'un `tools/call` sont clonés une fois par appel (le handler
    /// s'exécute dans un thread qui doit les posséder).
    #[must_use]
    pub fn tool_timeout(mut self, timeout: Duration) -> Self {
        self.tool_timeout = Some(timeout);
        self
    }

    /// Attache un **schéma de sortie** à un outil (module [`crate::zod`]).
    ///
    /// Le schéma est publié dans `tools/list` (`outputSchema`) **et
    /// appliqué** : un `structuredContent` non conforme devient un résultat
    /// `isError` — le schéma de sortie est exécuteur comme celui d'entrée.
    /// Comme [`Self::annotate`], l'ordre d'appel n'a pas d'importance.
    #[must_use]
    pub fn output_schema<S: Schema + 'static>(mut self, tool: &str, schema: S) -> Self {
        if let Some(def) = self.tools.get_mut(tool) {
            def.output_schema = Some(Arc::new(schema));
        } else {
            self.output_schemas
                .insert(tool.to_string(), Arc::new(schema));
        }
        self
    }

    /// Enregistre une **ressource** MCP (méthodes `resources/list` /
    /// `resources/read`).
    ///
    /// La clé est l'`uri` ; le handler produit le contenu texte à chaque
    /// lecture (rien n'est mis en cache par la lib). Réenregistrer une
    /// `uri` existante remplace silencieusement la définition.
    /// `resources/list` annonce `uri`/`name`/`description` ; le `mimeType`
    /// vient du [`crate::protocol::ResourceContents`] rendu par le handler.
    #[must_use]
    pub fn resource<F>(mut self, uri: &str, name: &str, description: &str, f: F) -> Self
    where
        F: Fn() -> Result<ResourceContents, String> + Send + Sync + 'static,
    {
        self.resources.insert(
            uri.to_string(),
            ResourceDef {
                name: name.to_string(),
                description: Some(description.to_string()),
                f: Arc::new(f),
            },
        );
        self
    }

    /// Enregistre un **prompt** MCP (méthodes `prompts/list` /
    /// `prompts/get`).
    ///
    /// Le handler reçoit `arguments` (objet JSON — non-objet refusé en
    /// `-32602`) et rend les messages du prompt (voir
    /// [`crate::protocol::PromptMessage::user`]/`::assistant`).
    /// Réenregistrer un nom existant remplace silencieusement la
    /// définition.
    #[must_use]
    pub fn prompt<F>(mut self, name: &str, description: &str, f: F) -> Self
    where
        F: Fn(&Value) -> Result<Vec<PromptMessage>, String> + Send + Sync + 'static,
    {
        self.prompts.insert(
            name.to_string(),
            PromptDef {
                description: Some(description.to_string()),
                f: Arc::new(f),
            },
        );
        self
    }

    /// Enchaîne un enregistrement généré par la macro `#[tool]` (feature
    /// `macros`) — `builder.with_tool(register_add)` où `register_add` est
    /// la fonction générée à côté de votre fonction annotée.
    #[must_use]
    pub fn with_tool(self, register: impl FnOnce(Self) -> Self) -> Self {
        register(self)
    }

    /// Enregistre un outil.
    ///
    /// La closure reçoit `arguments` (objet JSON — un `arguments` non-objet
    /// est refusé en `-32602` avant tout appel du handler) et renvoie la
    /// valeur structurée, ou un message d'erreur lisible par le client.
    /// Réenregistrer un nom existant remplace silencieusement l'outil
    /// précédent (le dernier enregistré gagne).
    pub fn tool<F>(mut self, name: &str, description: &str, f: F) -> Self
    where
        F: Fn(&Value) -> ToolResult + Send + Sync + 'static,
    {
        let wrapped = move |args: &Value| f(args).map_err(ToolFailure::Tool);
        self.tools.insert(
            name.to_string(),
            ToolDef {
                description: Some(description.to_string()),
                schema: None,
                output_schema: self.output_schemas.remove(name),
                annotations: self.annotations.remove(name),
                f: Arc::new(wrapped),
            },
        );
        self
    }

    /// Enregistre un outil avec un **schéma à la Zod** (module [`crate::zod`]).
    ///
    /// Le schéma dérive l'`inputSchema` de `tools/list` **et** valide chaque
    /// appel : un `tools/call` hors schéma reçoit `-32602` avec la liste des
    /// problèmes dans `error.data.issues` et n'exécute jamais le handler.
    /// La closure reçoit les arguments validés.
    /// Réenregistrer un nom existant remplace silencieusement l'outil
    /// précédent.
    pub fn tool_typed<S, F>(mut self, name: &str, description: &str, schema: S, f: F) -> Self
    where
        S: Schema + 'static,
        F: Fn(&Value) -> ToolResult + Send + Sync + 'static,
    {
        let wrapped = move |args: &Value| f(args).map_err(ToolFailure::Tool);
        self.tools.insert(
            name.to_string(),
            ToolDef {
                description: Some(description.to_string()),
                schema: Some(Arc::new(schema)),
                output_schema: self.output_schemas.remove(name),
                annotations: self.annotations.remove(name),
                f: Arc::new(wrapped),
            },
        );
        self
    }

    /// Comme [`ServerBuilder::tool_typed`], avec un handler **typé** :
    /// les arguments validés par le schéma sont désérialisés dans `T`
    /// (généralement un struct `#[derive(Deserialize)]`). Réenregistrer un
    /// nom existant remplace silencieusement l'outil précédent.
    ///
    /// ```no_run
    /// use litemcp::prelude::*;
    /// use serde::Deserialize;
    ///
    /// #[derive(Deserialize)]
    /// struct AddArgs { a: i64, b: i64 }
    ///
    /// # fn main() -> std::io::Result<()> {
    /// ServerBuilder::new("calc", "1.0.0")
    ///     .tool_parse(
    ///         "add",
    ///         "Additionne deux entiers",
    ///         zod::object()
    ///             .field("a", zod::integer())
    ///             .field("b", zod::integer()),
    ///         |args: AddArgs| Ok(serde_json::json!({ "somme": args.a + args.b })),
    ///     )
    ///     .bind("127.0.0.1:8080")?
    ///     .serve()?;
    /// # Ok::<(), std::io::Error>(())
    /// # }
    /// ```
    pub fn tool_parse<S, T, F>(mut self, name: &str, description: &str, schema: S, f: F) -> Self
    where
        S: Schema + 'static,
        T: DeserializeOwned,
        F: Fn(T) -> ToolResult + Send + Sync + 'static,
    {
        // Pas de `value.clone()` : `serde_json::from_value` consomme le
        // `Value`. On sérialise donc une fois vers un buffer, puis on
        // désérialise depuis ce buffer — aucune copie de l'arbre JSON des
        // arguments (le clone coûtait jusqu'à 1 Mio par appel).
        let handler = Arc::new(move |value: &Value| match serde_json::to_vec(value) {
            Err(error) => Err(ToolFailure::Protocol(
                ErrorObject::INVALID_PARAMS,
                format!("arguments illisibles : {error}"),
            )),
            Ok(buffer) => match serde_json::from_slice::<T>(&buffer) {
                Ok(parsed) => f(parsed).map_err(ToolFailure::Tool),
                Err(error) => Err(ToolFailure::Protocol(
                    ErrorObject::INVALID_PARAMS,
                    format!("arguments invalides pour le type attendu : {error}"),
                )),
            },
        });
        self.tools.insert(
            name.to_string(),
            ToolDef {
                description: Some(description.to_string()),
                schema: Some(Arc::new(schema)),
                output_schema: self.output_schemas.remove(name),
                annotations: self.annotations.remove(name),
                f: handler,
            },
        );
        self
    }

    /// Attache des annotations MCP (2026-07-28) à un outil : `title`,
    /// `readOnlyHint`, `destructiveHint`, `idempotentHint`, `openWorldHint`.
    ///
    /// **L'ordre d'appel n'a plus d'importance** : les annotations sont
    /// conservées à part et fusionnées dans l'outil au moment de son
    /// enregistrement. Appeler `.annotate()` avant `.tool()` fonctionne donc
    /// exactement comme l'inverse.
    ///
    /// Si le nom ne correspond à aucun outil enregistré, l'annotation est
    /// **conservée** : elle sera appliquée si l'outil est enregistré plus
    /// tard. (Elle est ignorée si l'outil n'est jamais enregistré.)
    #[must_use]
    pub fn annotate(mut self, tool: &str, annotations: ToolAnnotations) -> Self {
        // Applique immédiatement si l'outil existe déjà, sinon met en attente.
        if let Some(def) = self.tools.get_mut(tool) {
            def.annotations = Some(annotations);
        } else {
            self.annotations.insert(tool.to_string(), annotations);
        }
        self
    }

    /// Construit le cœur de dispatch (tests, intégrations custom).
    ///
    /// **Instantané** : le cœur capture les outils présents à cet instant.
    /// Un outil enregistré *après* l'appel n'y figure pas — utilisez
    /// [`ServerBuilder::bind`] pour un serveur complet.
    #[must_use]
    pub fn core(&self) -> ServerCore {
        let mut tools = self.tools.clone();
        // Fusionne les annotations et schémas de sortie en attente sur les
        // outils déjà présents.
        for (name, def) in tools.iter_mut() {
            if let Some(annotations) = self.annotations.get(name) {
                def.annotations = Some(annotations.clone());
            }
            if let Some(output) = self.output_schemas.get(name) {
                def.output_schema = Some(Arc::clone(output));
            }
        }
        ServerCore {
            name: self.name.clone(),
            version: self.version.clone(),
            tools: Arc::new(tools),
            resources: Arc::new(self.resources.clone()),
            prompts: Arc::new(self.prompts.clone()),
            tool_timeout: self.tool_timeout,
        }
    }

    /// Bind le socket TCP — puis [`crate::http::BoundServer::serve`] bloque.
    ///
    /// La route est validée ici (voir [`ServerBuilder::route`]) : une route
    /// qui ne peut jamais matcher ou qui masque la sonde de vie est refusée
    /// en `InvalidInput` plutôt que de démarrer un serveur injoignable.
    pub fn bind(self, addr: &str) -> std::io::Result<crate::http::BoundServer> {
        if !self.route.starts_with('/') || self.route.contains('?') {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "route invalide {:?} : elle doit commencer par '/' et ne pas contenir de '?'",
                    self.route
                ),
            ));
        }
        if self.route == crate::http::HEALTH_ROUTE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "route invalide {:?} : elle masque la sonde de vie {}",
                    self.route,
                    crate::http::HEALTH_ROUTE
                ),
            ));
        }
        let listener = std::net::TcpListener::bind(addr)?;
        let mut tools = self.tools;
        // Fusionne les annotations et schémas de sortie en attente (outil
        // annoté avant d'être enregistré, ou réenregistré plus tard).
        for (name, def) in tools.iter_mut() {
            if let Some(annotations) = self.annotations.get(name) {
                def.annotations = Some(annotations.clone());
            }
            if let Some(output) = self.output_schemas.get(name) {
                def.output_schema = Some(Arc::clone(output));
            }
        }
        let core = Arc::new(ServerCore {
            name: self.name,
            version: self.version,
            tools: Arc::new(tools),
            resources: Arc::new(self.resources),
            prompts: Arc::new(self.prompts),
            tool_timeout: self.tool_timeout,
        });
        Ok(crate::http::BoundServer {
            listener,
            core,
            route: Arc::new(self.route),
            allow_origins: Arc::new(self.allow_origins),
            read_timeout: self.read_timeout,
            max_connections: self.max_connections,
            connection_ttl: self.connection_ttl,
            live_connections: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            refusals_in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }
}
