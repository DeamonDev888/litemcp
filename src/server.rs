//! Noyau de dispatch JSON-RPC + builder — indépendant du transport.

use crate::error::{ErrorCode, ToolError};
use crate::http::{RequestContext, RequestHook, RequestRejection};
use crate::jsonrpc::{ErrorObject, Outgoing, RequestId};
use crate::protocol;
use crate::protocol::{
    CacheScope, CallToolResult, DiscoverResult, Prompt, PromptMessage, PromptsListResult, Resource,
    ResourceContents, ResourcesListResult, ResultType, ServerInfo, TextContent, Tool,
    ToolAnnotations, ToolsListResult,
};
use crate::zod::{Issues, Path, Schema};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Résultat d'un outil : `Ok(valeur)` ou `Err` d'une erreur structurée.
///
/// À l'ok, la valeur devient `structuredContent` : les objets JSON passent
/// tels quels ; toute autre valeur (scalaire, tableau, `null`) est enveloppée
/// dans `{ "result": … }` pour rester un objet invariant (choix d'API).
///
/// À l'erreur, un [`ToolError`] classifie l'échec (`rate_limited`,
/// `upstream`…) et le noyau publie la machine-lisible sous
/// `structuredContent.error` — le client agent réagit programmatiquement
/// (attendre, reformuler, abandonner) au lieu de parser le texte. Les
/// erreurs « prose » restent acceptées : `Err("a manquant")` compile tel
/// quel (voir [`From<&str> for ToolError`](crate::error::ToolError#impl-From%3C%26str%3E-for-ToolError)),
/// classée `unclassified`.
pub type ToolResult = Result<Value, ToolError>;

/// Statut d'un appel, tel qu'observé par un hook
/// [`ServerBuilder::on_call`](crate::server::ServerBuilder::on_call).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallStatus {
    /// Réponse de succès rendue (le handler — s'il y en avait un — a
    /// réussi).
    Success,
    /// Erreur de protocole JSON-RPC (code inclus) : arguments invalides
    /// (`-32602`), méthode inconnue (`-32601`), version non supportée
    /// (`-32022`), notification d'action refusée (`-32600`)…
    ProtocolError(i64),
    /// Échec d'exécution d'un outil : résultat `isError: true` rendu —
    /// avec la classification de l'erreur.
    ToolFailure(ErrorCode),
    /// Notification de protocole (`notifications/*`) acceptée et ignorée —
    /// aucune réponse rendue.
    Notification,
}

/// Événement d'observation d'un message JSON-RPC dispatché — voir
/// [`ServerBuilder::on_call`](crate::server::ServerBuilder::on_call).
///
/// Émis **une fois par message** après son traitement complet (succès ou
/// échec) : compteurs par outil, latence, taux d'erreur se déduisent d'un
/// unique callback.
#[derive(Debug, Clone)]
pub struct CallEvent<'a> {
    /// Méthode JSON-RPC (`tools/call`, `resources/read`…).
    pub method: &'a str,
    /// Nom de l'outil / du prompt, ou `uri` de la ressource, quand le
    /// message en porte un (extraction best-effort depuis `params`).
    pub name: Option<&'a str>,
    /// Durée totale du traitement du message (dispatch + handlers).
    pub duration: Duration,
    /// Verdict final.
    pub status: CallStatus,
}

type ToolFn = dyn Fn(&Value) -> Result<Value, ToolFailure> + Send + Sync;
type ResourceFn = dyn Fn() -> Result<ResourceContents, String> + Send + Sync;
type PromptFn = dyn Fn(&Value) -> Result<Vec<PromptMessage>, String> + Send + Sync;
type CallHook = dyn Fn(&CallEvent<'_>) + Send + Sync;

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

/// Plafond par défaut de threads de handlers simultanés (en cours ou
/// détachés après un dépassement de [`ServerBuilder::tool_timeout`]).
pub const MAX_INFLIGHT_HANDLERS: usize = 256;

/// Plafond par défaut de taille d'une réponse JSON-RPC sérialisée (16 Mio).
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Plafond de caractères reflétés dans un message d'erreur JSON-RPC pour un
/// identifiant fourni par le client (`method`, `name`, `uri`, version).
const MAX_ECHO_CHARS: usize = 128;

/// Code d'erreur implementation-defined (plage `-32000`..`-32019` réservée
/// aux implémentations par la spec 2026-07-28 ; `-32020`+ est réservé à la
/// spec) : échec de lecture d'une ressource ou d'un prompt.
const RESOURCE_FAILURE: i64 = -32000;

/// Neutralise les caractères de contrôle (ANSI, NUL, CR/LF) et tronque à
/// [`MAX_ECHO_CHARS`] tout identifiant issu du client avant de l'inclure
/// dans un message d'erreur.
fn sanitize_echo(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(MAX_ECHO_CHARS + 4));
    for (idx, ch) in raw.chars().enumerate() {
        if idx >= MAX_ECHO_CHARS {
            out.push('…');
            break;
        }
        if ch.is_control() {
            out.push('\u{FFFD}');
        } else {
            out.push(ch);
        }
    }
    out
}

/// Garde RAII d'un slot de thread de handler détaché : décrémente le compteur
/// uniquement lorsque le thread système se termine réellement (même après un
/// timeout ou une panique).
struct HandlerSlotGuard(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for HandlerSlotGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

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
    /// Plafond de threads de handlers simultanés atteint — refuse de créer
    /// un nouveau thread noyau.
    Overloaded,
}

/// Exécute `f` borné par `timeout` — dans un thread détaché, sous réserve du
/// plafond `max_inflight` de threads de handlers simultanés.
///
/// En cas de timeout, le thread de `f` n'est **pas** interrompu (impossible
/// en Rust) : il termine à son rythme et son résultat est ignoré, tout en
/// retenant son slot `inflight` jusqu'à sa terminaison effective afin
/// d'empêcher l'épuisement de la table de threads du noyau. La panique de `f`
/// est rattrapée dans son thread et rendue comme [`Outcome::Panicked`].
fn run_with_timeout<T: Send + 'static>(
    timeout: Duration,
    inflight: &Arc<std::sync::atomic::AtomicUsize>,
    max_inflight: usize,
    f: impl FnOnce() -> T + Send + 'static,
) -> Outcome<T> {
    let prev = inflight.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    if prev >= max_inflight {
        inflight.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        return Outcome::Overloaded;
    }
    let guard = HandlerSlotGuard(Arc::clone(inflight));
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("litemcp-handler".into())
        .spawn(move || {
            let _guard = guard;
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
/// - `Tool(ToolError)` → échec d'exécution (`isError: true`, HTTP 200) ;
/// - `Protocol(i64, String)` → erreur de protocole (ex. arguments qui ne
///   désérialisent pas dans le struct typé → `-32602`).
#[derive(Debug)]
enum ToolFailure {
    Tool(ToolError),
    Protocol(i64, String),
}

#[derive(Clone)]
struct ToolDef {
    description: Option<String>,
    schema: Option<Arc<dyn Schema>>,
    output_schema: Option<Arc<dyn Schema>>,
    /// Le schéma de sortie attend-il un **objet** à la racine ?
    ///
    /// Calculé **une fois** à l'enregistrement : le savoir est ce qui
    /// décide si le `structuredContent` est envoyé brut ou enveloppé
    /// (`{ "result": … }`) — l'enveloppe doit rester conforme au schéma
    /// publié (spec MCP `2026-07-28` : « Servers MUST provide structured
    /// results that conform to this schema »).
    output_object_rooted: bool,
    annotations: Option<ToolAnnotations>,
    f: Arc<ToolFn>,
}

impl ToolDef {
    /// Recalcule [`Self::output_object_rooted`] après un changement de
    /// `output_schema`.
    fn sync_output_shape(&mut self) {
        self.output_object_rooted = self
            .output_schema
            .as_ref()
            .is_some_and(|schema| output_is_object_rooted(schema.as_ref()));
    }
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
    schema: Option<Arc<dyn Schema>>,
    f: Arc<PromptFn>,
}

/// En-têtes de routage MCP 2026-07-28 (SEP-2243).
///
/// `Mcp-Method` et `Mcp-Name` permettent à un gateway de router et
/// d'autoriser sans analyser le corps JSON. Optionnels chez LiteMCP
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
    /// Identité `_meta` précalculée une seule fois à la construction (SEP-2575).
    meta: Value,
    tools: Arc<BTreeMap<String, ToolDef>>,
    resources: Arc<BTreeMap<String, ResourceDef>>,
    prompts: Arc<BTreeMap<String, PromptDef>>,
    /// Délai maximal d'exécution des handlers (tools, resources, prompts) —
    /// `None` : illimité (comportement historique).
    tool_timeout: Option<Duration>,
    /// Plafond de threads de handlers simultanés (en cours ou détachés après
    /// un timeout).
    max_inflight_handlers: usize,
    /// Compteur de threads de handlers simultanés.
    inflight_handlers: Arc<std::sync::atomic::AtomicUsize>,
    /// Plafond d'octets d'une réponse JSON-RPC sérialisée (`0` : illimité).
    max_response_bytes: usize,
    /// Hooks d'observation ([`ServerBuilder::on_call`]) — appelés après le
    /// traitement complet de chaque message.
    hooks: Arc<Vec<Arc<CallHook>>>,
    /// Catalogues sérialisés — immuables après construction (les registres
    /// ne bougent plus), servis **tels quels** : `tools/list`,
    /// `resources/list`, `prompts/list`, `server/discover`, `initialize` et
    /// `ping` ne resérialisent ni ne clonent leur arbre JSON à chaque requête.
    tools_list_cache: std::sync::OnceLock<Value>,
    resources_list_cache: std::sync::OnceLock<Value>,
    prompts_list_cache: std::sync::OnceLock<Value>,
    discover_cache: std::sync::OnceLock<Value>,
    initialize_cache: std::sync::OnceLock<Value>,
    ping_cache: std::sync::OnceLock<Value>,
}

#[derive(Serialize)]
struct CallToolResultRef<'a> {
    #[serde(rename = "resultType")]
    result_type: ResultType,
    content: &'a [TextContent],
    #[serde(rename = "isError", skip_serializing_if = "Option::is_none")]
    is_error: Option<bool>,
    #[serde(rename = "structuredContent", skip_serializing_if = "Option::is_none")]
    structured_content: Option<&'a Value>,
    #[serde(rename = "_meta")]
    meta: &'a Value,
}

#[derive(Serialize)]
struct ReadResourceResultRef<'a> {
    #[serde(rename = "resultType")]
    result_type: ResultType,
    #[serde(rename = "ttlMs")]
    ttl_ms: u64,
    #[serde(rename = "cacheScope")]
    cache_scope: CacheScope,
    contents: &'a [ResourceContents],
    #[serde(rename = "_meta")]
    meta: &'a Value,
}

#[derive(Serialize)]
struct GetPromptResultRef<'a> {
    #[serde(rename = "resultType")]
    result_type: ResultType,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
    messages: &'a [PromptMessage],
    #[serde(rename = "_meta")]
    meta: &'a Value,
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
    /// gateway qui route sur ces en-têtes désynchronise le routage. Une
    /// **requête** incohérente reçoit `-32020` ; une **notification**
    /// incohérente ne reçoit rien (JSON-RPC 2.0 §4 : jamais de réponse à une
    /// notification) — le refus n'est visible que par les hooks
    /// [`ServerBuilder::on_call`](crate::server::ServerBuilder::on_call).
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
                    Outgoing::invalid_request(id, format!("invalid request: {message}")).to_vec(),
                );
            }
        };
        // L'observation démarre après le parsing : un corps illisible n'a
        // ni méthode ni outil à rapporter.
        let started = Instant::now();
        // id réel, ou null explicite : les deux exigent une réponse ; l'id
        // null explicite est rendu tel quel (Value::Null).
        let id = incoming
            .id
            .as_ref()
            .map(RequestId::to_value)
            .unwrap_or(Value::Null);
        let params = incoming.params.as_ref().unwrap_or_else(|| empty_object());
        // Nom de l'outil/prompt ou uri de la ressource pour l'observation —
        // extraction best-effort, sans coût sur les méthodes sans nom.
        let name = requested_name(&incoming.method, params);

        // Cohérence des en-têtes de routage MCP (SEP-2243) : `Mcp-Method` et
        // `Mcp-Name` doivent refléter **exactement** (sensible à la casse)
        // le corps de la requête (y compris sur une notification). Un
        // gateway qui route ou applique une ACL sur ces en-têtes verrait
        // sinon `tools/call` arriver comme `ping`, `ADMIN_TOOL` contourner
        // une règle sur `admin_tool`, ou `Mcp-Name` passer sur une méthode
        // sans nom (`resources/read`, `tools/list`…).
        //
        // L'incohérence est calculée d'abord, traitée ensuite : une
        // **notification** (pas d'`id`) ne reçoit pas de réponse ici (JSON-RPC
        // 2.0 §4 — hors le refus explicite des notifications d'action, assumé
        // plus bas) : son rejet est seulement signalé aux hooks d'observation,
        // puis le message est laissé tomber (le transport répond `202`). Une
        // requête, elle, reçoit l'erreur `-32020`.
        let mut header_mismatch: Option<ErrorObject> = None;
        if let Some(headers) = headers {
            if let Some(header_method) = &headers.method {
                if header_method.is_empty() || header_method != &incoming.method {
                    header_mismatch = Some(ErrorObject::with_data(
                        ErrorObject::HEADER_MISMATCH,
                        "Mcp-Method header does not match the request body",
                        json!({
                            "header": header_method,
                            "body": incoming.method,
                        }),
                    ));
                }
            }
            if header_mismatch.is_none() {
                if let Some(header_name) = &headers.name {
                    let body_name = params.get("name").and_then(Value::as_str).unwrap_or("");
                    let expects_name = matches!(
                        incoming.method.as_str(),
                        protocol::method::TOOLS_CALL | protocol::method::PROMPTS_GET
                    );
                    if !expects_name
                        || header_name.is_empty()
                        || body_name.is_empty()
                        || header_name != body_name
                    {
                        header_mismatch = Some(ErrorObject::with_data(
                            ErrorObject::HEADER_MISMATCH,
                            "Mcp-Name header does not match the request body",
                            json!({
                                "header": header_name,
                                "body": body_name,
                            }),
                        ));
                    }
                }
            }
        }
        if let Some(error) = header_mismatch {
            if incoming.is_notification() {
                self.emit_call(
                    &incoming.method,
                    name,
                    started.elapsed(),
                    CallStatus::ProtocolError(ErrorObject::HEADER_MISMATCH),
                );
                return None;
            }
            return Some(self.protocol_failure(&id, &incoming, name, started, error));
        }

        if incoming.is_notification() {
            // **Notification d'action** (`tools/call`, `resources/read`,
            // `prompts/get` sans `id`) : JSON-RPC 2.0 §4 impose de traiter
            // la notification, mais le mode stateless ne peut pas livrer
            // le résultat (aucune réponse) — l'effet de bord serait
            // invisible. Refus **explicite** (`-32600`) : le client sait
            // que rien n'a été exécuté (un `202` silencieux le laisserait
            // croire à une exécution).
            if protocol::is_action(&incoming.method) {
                return Some(self.protocol_failure(
                    &Value::Null,
                    &incoming,
                    name,
                    started,
                    ErrorObject::new(
                        ErrorObject::INVALID_REQUEST,
                        format!(
                            "notification rejected: `{}` executes server-side code — \
                             a request (with `id`) is required to obtain the result",
                            sanitize_echo(&incoming.method)
                        ),
                    ),
                ));
            }
            // Notification de protocole (`notifications/*`) : message de
            // contrôle sans effet en mode stateless — acceptée et ignorée.
            //
            // Une notification qui n'est NI une action NI un message
            // `notifications/*` est **inconnue** : la traiter comme une
            // notification de protocole reviendrait à valider par le silence
            // une méthode que le serveur ne connaît pas (et à laisser un
            // futur ajout de méthode d'action non listée s'exécuter en mode
            // notification). Refus explicite, comme pour les actions.
            if !protocol::is_protocol_notification(&incoming.method) {
                return Some(self.protocol_failure(
                    &Value::Null,
                    &incoming,
                    name,
                    started,
                    ErrorObject::new(
                        ErrorObject::INVALID_REQUEST,
                        format!(
                            "unknown notification: `{}` — only `notifications/*` are accepted \
                             (action methods require a request with an `id`)",
                            sanitize_echo(&incoming.method)
                        ),
                    ),
                ));
            }
            self.emit_call(
                &incoming.method,
                None,
                started.elapsed(),
                CallStatus::Notification,
            );
            return None;
        }

        // MCP 2026-07-28 (SEP-2575) : la version de protocole voyage dans
        // `_meta` de chaque requête. Une version **explicite** qui n'est pas
        // supportée est refusée en `-32022` ; l'absence de version est
        // tolérée (compatibilité avec les clients legacy `2025-11-25`).
        if let Some(err) = self.check_protocol_version(params) {
            return Some(self.protocol_failure(&id, &incoming, name, started, err));
        }

        let (mut reply, mut status) = match incoming.method.as_str() {
            protocol::method::DISCOVER => (
                Outgoing::success_vec(&id, self.discover_json()),
                CallStatus::Success,
            ),
            protocol::method::INITIALIZE => (
                Outgoing::success_vec(&id, self.initialize_json()),
                CallStatus::Success,
            ),
            // `ping` a été retirée de la spec 2026-07-28 (SEP-2575) mais
            // reste tolérée pour les clients legacy : il ne coûte rien.
            // Le résultat porte néanmoins `resultType` (SEP-2322) et
            // l'identité du serveur (SEP-2575) — « chaque résultat » sans
            // exception.
            protocol::method::PING => (
                Outgoing::success_vec(&id, self.ping_json()),
                CallStatus::Success,
            ),
            protocol::method::TOOLS_LIST => (
                Outgoing::success_vec(&id, self.tools_list_json()),
                CallStatus::Success,
            ),
            protocol::method::TOOLS_CALL => self.call_tool(&id, params),
            protocol::method::RESOURCES_LIST => (
                Outgoing::success_vec(&id, self.resources_list_json()),
                CallStatus::Success,
            ),
            protocol::method::RESOURCES_READ => self.read_resource(&id, params),
            protocol::method::PROMPTS_LIST => (
                Outgoing::success_vec(&id, self.prompts_list_json()),
                CallStatus::Success,
            ),
            protocol::method::PROMPTS_GET => self.get_prompt(&id, params),
            other => (
                Outgoing::failure(
                    id.clone(),
                    ErrorObject::new(
                        ErrorObject::METHOD_NOT_FOUND,
                        format!("Unknown method: {}", sanitize_echo(other)),
                    ),
                )
                .to_vec(),
                CallStatus::ProtocolError(ErrorObject::METHOD_NOT_FOUND),
            ),
        };
        if self.max_response_bytes > 0 && reply.len() > self.max_response_bytes {
            let actual = reply.len();
            reply = Outgoing::failure(
                id,
                ErrorObject::new(
                    ErrorObject::INTERNAL_ERROR,
                    format!(
                        "response size ({actual} bytes) exceeds maximum ({} bytes)",
                        self.max_response_bytes
                    ),
                ),
            )
            .to_vec();
            status = CallStatus::ProtocolError(ErrorObject::INTERNAL_ERROR);
        }
        self.emit_call(&incoming.method, name, started.elapsed(), status);
        Some(reply)
    }

    /// Construit la réponse d'erreur de protocole **et** émet l'événement
    /// d'observation correspondant — les sorties anticipées de
    /// [`ServerCore::dispatch_with_headers`] passent toutes ici pour que
    /// chaque message produise exactement un événement.
    fn protocol_failure(
        &self,
        id: &Value,
        incoming: &crate::jsonrpc::Incoming,
        name: Option<&str>,
        started: Instant,
        error: ErrorObject,
    ) -> Vec<u8> {
        let code = error.code;
        let outgoing = Outgoing::failure(id.clone(), error);
        self.emit_call(
            &incoming.method,
            name,
            started.elapsed(),
            CallStatus::ProtocolError(code),
        );
        outgoing.to_vec()
    }

    /// Notifie les hooks d'observation — no-op sans hook enregistré.
    ///
    /// Une panique de hook est attrapée et ignorée : l'observation ne doit
    /// jamais interrompre le service (le hook défaillant est silencieusement
    /// passé pour CET événement, il sera rappelé au suivant).
    fn emit_call(&self, method: &str, name: Option<&str>, duration: Duration, status: CallStatus) {
        if self.hooks.is_empty() {
            return;
        }
        let event = CallEvent {
            method,
            name,
            duration,
            status,
        };
        for hook in self.hooks.iter() {
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| hook(&event)));
        }
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
        let safe = sanitize_echo(requested);
        Some(ErrorObject::with_data(
            ErrorObject::UNSUPPORTED_PROTOCOL_VERSION,
            format!("Unsupported protocol version: {safe}"),
            json!({
                "supported": protocol::SUPPORTED_VERSIONS,
                "requested": safe,
            }),
        ))
    }

    /// Identité du serveur sous `_meta` (SEP-2575 : les serveurs SHOULD
    /// s'identifier dans chaque résultat).
    fn meta(&self) -> Value {
        self.meta.clone()
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

    /// `server/discover` sérialisé, calculé une fois puis servi tel quel.
    fn discover_json(&self) -> &Value {
        self.discover_cache
            .get_or_init(|| serde_json::to_value(self.discover()).unwrap_or(Value::Null))
    }

    /// `initialize` sérialisé, calculé une fois puis servi tel quel.
    fn initialize_json(&self) -> &Value {
        self.initialize_cache.get_or_init(|| {
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
                "_meta": self.meta(),
            })
        })
    }

    /// `ping` sérialisé, calculé une fois puis servi tel quel.
    fn ping_json(&self) -> &Value {
        self.ping_cache.get_or_init(|| {
            json!({
                "resultType": "complete",
                "_meta": self.meta(),
            })
        })
    }

    /// `tools/list` sérialisé, calculé une fois puis servi tel quel.
    fn tools_list_json(&self) -> &Value {
        self.tools_list_cache.get_or_init(|| {
            serde_json::to_value(self.list_tools()).unwrap_or_else(|_| json!({ "tools": [] }))
        })
    }

    /// `resources/list` sérialisé, calculé une fois puis servi tel quel.
    fn resources_list_json(&self) -> &Value {
        self.resources_list_cache.get_or_init(|| {
            serde_json::to_value(self.list_resources())
                .unwrap_or_else(|_| json!({ "resources": [] }))
        })
    }

    /// `prompts/list` sérialisé, calculé une fois puis servi tel quel.
    fn prompts_list_json(&self) -> &Value {
        self.prompts_list_cache.get_or_init(|| {
            serde_json::to_value(self.list_prompts()).unwrap_or_else(|_| json!({ "prompts": [] }))
        })
    }

    /// Résultat de `server/discover` (**MUST**, SEP-2575).
    fn discover(&self) -> DiscoverResult {
        DiscoverResult {
            result_type: ResultType::Complete,
            supported_versions: protocol::SUPPORTED_VERSIONS.to_vec(),
            capabilities: self.capabilities(),
            meta: self.meta(),
        }
    }

    /// Noms des outils de CE snapshot, dans l'ordre du `tools/list` servi —
    /// introspection sans sérialisation (journaux, tests, gateways qui
    /// comparent leur registre à leur état détecté).
    #[must_use]
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// Threads de handler encore vivants — mode borné uniquement
    /// ([`ServerBuilder::tool_timeout`]) : sans timeout, les handlers
    /// tournent dans le thread de connexion et ce compteur vaut zéro. Le
    /// module anti-zombie le lit pour son bilan de fin de vie.
    #[must_use]
    pub(crate) fn live_handler_threads(&self) -> usize {
        self.inflight_handlers
            .load(std::sync::atomic::Ordering::Acquire)
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
                arguments: def
                    .schema
                    .as_ref()
                    .and_then(|schema| derive_prompt_arguments(&schema.json())),
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
    fn read_resource(&self, id: &Value, params: &Value) -> (Vec<u8>, CallStatus) {
        let Some(uri) = params.get("uri").and_then(Value::as_str) else {
            return (
                Outgoing::failure(
                    id.clone(),
                    ErrorObject::new(ErrorObject::INVALID_PARAMS, "missing params.uri"),
                )
                .to_vec(),
                CallStatus::ProtocolError(ErrorObject::INVALID_PARAMS),
            );
        };
        let Some(def) = self.resources.get(uri) else {
            return (
                Outgoing::failure(
                    id.clone(),
                    ErrorObject::new(
                        ErrorObject::INVALID_PARAMS,
                        format!("Unknown resource: {}", sanitize_echo(uri)),
                    ),
                )
                .to_vec(),
                CallStatus::ProtocolError(ErrorObject::INVALID_PARAMS),
            );
        };
        let handler = Arc::clone(&def.f);
        let contents = match self.run_bounded(move || handler()) {
            Ok(contents) => contents,
            Err(failure) => {
                let code = failure.code;
                return (
                    Outgoing::failure(id.clone(), failure).to_vec(),
                    CallStatus::ProtocolError(code),
                );
            }
        };
        match contents {
            Ok(contents) => (
                Outgoing::success_vec(
                    id,
                    &ReadResourceResultRef {
                        result_type: ResultType::Complete,
                        ttl_ms: DEFAULT_LIST_TTL_MS,
                        cache_scope: CacheScope::Private,
                        contents: std::slice::from_ref(&contents),
                        meta: &self.meta,
                    },
                ),
                CallStatus::Success,
            ),
            Err(message) => (
                Outgoing::failure(
                    id.clone(),
                    ErrorObject::new(RESOURCE_FAILURE, format!("Resource read failed: {message}")),
                )
                .to_vec(),
                CallStatus::ProtocolError(RESOURCE_FAILURE),
            ),
        }
    }

    /// Exécute un handler de prompt, borné par `tool_timeout`.
    fn get_prompt(&self, id: &Value, params: &Value) -> (Vec<u8>, CallStatus) {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return (
                Outgoing::failure(
                    id.clone(),
                    ErrorObject::new(ErrorObject::INVALID_PARAMS, "missing params.name"),
                )
                .to_vec(),
                CallStatus::ProtocolError(ErrorObject::INVALID_PARAMS),
            );
        };
        let Some(def) = self.prompts.get(name) else {
            return (
                Outgoing::failure(
                    id.clone(),
                    ErrorObject::new(
                        ErrorObject::INVALID_PARAMS,
                        format!("Unknown prompt: {}", sanitize_echo(name)),
                    ),
                )
                .to_vec(),
                CallStatus::ProtocolError(ErrorObject::INVALID_PARAMS),
            );
        };
        // `arguments` : objet ou absent — même contrat que `tools/call`.
        let arguments = match params.get("arguments") {
            None => empty_object(),
            Some(value) if value.is_object() => value,
            Some(_) => {
                return (
                    Outgoing::failure(
                        id.clone(),
                        ErrorObject::new(
                            ErrorObject::INVALID_PARAMS,
                            "params.arguments must be an object",
                        ),
                    )
                    .to_vec(),
                    CallStatus::ProtocolError(ErrorObject::INVALID_PARAMS),
                );
            }
        };
        // Injection des valeurs par défaut (même contrat que `tools/call`).
        let mut injected_arguments;
        let arguments = match &def.schema {
            Some(schema) if schema.has_defaults() => {
                injected_arguments = arguments.clone();
                schema.apply_defaults(&mut injected_arguments);
                &injected_arguments
            }
            _ => arguments,
        };
        // Schéma exécuteur : hors schéma → -32602 avec les issues, le
        // handler ne tourne jamais (même contrat que `tool_typed`).
        if let Some(schema) = &def.schema {
            let mut issues = Issues::new();
            let mut path = Path::root("arguments");
            schema.validate(&mut path, arguments, &mut issues);
            if !issues.is_empty() {
                let rendered = issues.render();
                return (
                    Outgoing::failure(
                        id.clone(),
                        ErrorObject::with_data(
                            ErrorObject::INVALID_PARAMS,
                            format!("Invalid arguments: {}", rendered.join(" ; ")),
                            json!({ "issues": rendered }),
                        ),
                    )
                    .to_vec(),
                    CallStatus::ProtocolError(ErrorObject::INVALID_PARAMS),
                );
            }
        }
        let handler = Arc::clone(&def.f);
        let arguments = arguments.clone();
        let messages = match self.run_bounded(move || handler(&arguments)) {
            Ok(messages) => messages,
            Err(failure) => {
                let code = failure.code;
                return (
                    Outgoing::failure(id.clone(), failure).to_vec(),
                    CallStatus::ProtocolError(code),
                );
            }
        };
        match messages {
            Ok(messages) => (
                Outgoing::success_vec(
                    id,
                    &GetPromptResultRef {
                        result_type: ResultType::Complete,
                        description: def.description.as_deref(),
                        messages: &messages,
                        meta: &self.meta,
                    },
                ),
                CallStatus::Success,
            ),
            Err(message) => (
                Outgoing::failure(
                    id.clone(),
                    ErrorObject::new(RESOURCE_FAILURE, format!("Prompt failed: {message}")),
                )
                .to_vec(),
                CallStatus::ProtocolError(RESOURCE_FAILURE),
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
            None => match std::panic::catch_unwind(AssertUnwindSafe(f)) {
                Ok(result) => Ok(result),
                Err(_) => Err(ErrorObject::new(
                    ErrorObject::INTERNAL_ERROR,
                    "internal error (handler panicked)",
                )),
            },
            Some(timeout) => match run_with_timeout(
                timeout,
                &self.inflight_handlers,
                self.max_inflight_handlers,
                f,
            ) {
                Outcome::Done(result) => Ok(result),
                Outcome::Panicked => Err(ErrorObject::new(
                    ErrorObject::INTERNAL_ERROR,
                    "internal error (handler panicked)",
                )),
                Outcome::TimedOut => Err(ErrorObject::new(
                    RESOURCE_FAILURE,
                    format!("execution timed out after {} ms", timeout.as_millis()),
                )),
                Outcome::Overloaded => Err(ErrorObject::new(
                    RESOURCE_FAILURE,
                    "too many concurrent handlers in flight",
                )),
            },
        }
    }

    fn call_tool(&self, id: &Value, params: &Value) -> (Vec<u8>, CallStatus) {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return (
                Outgoing::failure(
                    id.clone(),
                    ErrorObject::new(ErrorObject::INVALID_PARAMS, "missing params.name"),
                )
                .to_vec(),
                CallStatus::ProtocolError(ErrorObject::INVALID_PARAMS),
            );
        };
        let Some(tool) = self.tools.get(name) else {
            return (
                Outgoing::failure(
                    id.clone(),
                    ErrorObject::new(
                        ErrorObject::INVALID_PARAMS,
                        format!("Unknown tool: {}", sanitize_echo(name)),
                    ),
                )
                .to_vec(),
                CallStatus::ProtocolError(ErrorObject::INVALID_PARAMS),
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
                return (
                    Outgoing::failure(
                        id.clone(),
                        ErrorObject::new(
                            ErrorObject::INVALID_PARAMS,
                            "params.arguments must be an object",
                        ),
                    )
                    .to_vec(),
                    CallStatus::ProtocolError(ErrorObject::INVALID_PARAMS),
                );
            }
        };
        // **Injection des valeurs par défaut** : un champ absent dont le
        // schéma porte un `default` est complété **avant** l'exécution
        // (comme Zod) — le handler reçoit les arguments complets.
        //
        // La copie n'a lieu que si le schéma porte des défauts : sans
        // défaut, le chemin nominal reste **zéro allocation**.
        let mut injected_arguments;
        let arguments = match &tool.schema {
            Some(schema) if schema.has_defaults() => {
                injected_arguments = arguments.clone();
                schema.apply_defaults(&mut injected_arguments);
                &injected_arguments
            }
            _ => arguments,
        };
        if let Some(schema) = &tool.schema {
            let mut issues = Issues::new();
            let mut path = Path::root("arguments");
            schema.validate(&mut path, arguments, &mut issues);
            if !issues.is_empty() {
                let rendered = issues.render();
                return (
                    Outgoing::failure(
                        id.clone(),
                        ErrorObject::with_data(
                            ErrorObject::INVALID_PARAMS,
                            format!("Invalid arguments: {}", rendered.join(" ; ")),
                            json!({ "issues": rendered }),
                        ),
                    )
                    .to_vec(),
                    CallStatus::ProtocolError(ErrorObject::INVALID_PARAMS),
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
            None => match std::panic::catch_unwind(AssertUnwindSafe(|| (tool.f)(arguments))) {
                Ok(result) => result,
                Err(_) => Err(ToolFailure::Protocol(
                    ErrorObject::INTERNAL_ERROR,
                    "internal error (handler panicked)".to_string(),
                )),
            },
            Some(timeout) => {
                let owned = arguments.clone();
                let handler = Arc::clone(&tool.f);
                match run_with_timeout(
                    timeout,
                    &self.inflight_handlers,
                    self.max_inflight_handlers,
                    move || handler(&owned),
                ) {
                    Outcome::Done(result) => result,
                    Outcome::Panicked => Err(ToolFailure::Protocol(
                        ErrorObject::INTERNAL_ERROR,
                        "internal error (handler panicked)".to_string(),
                    )),
                    // Le délai dépassé est classé `timeout` (retentable) :
                    // le client agent sait qu'il peut rejouer la requête.
                    Outcome::TimedOut => Err(ToolFailure::Tool(ToolError::timeout(format!(
                        "tool execution timed out after {} ms",
                        timeout.as_millis()
                    )))),
                    Outcome::Overloaded => Err(ToolFailure::Tool(ToolError::rate_limited(
                        "too many concurrent handlers in flight",
                    ))),
                }
            }
        };
        let (result, status) = match execution {
            Ok(value) => {
                // `outputSchema` est exécuteur comme l'input : la valeur
                // `structuredContent` réellement envoyée est validée avant
                // de partir — non conforme → résultat `isError` (le handler
                // a exécuté, c'est sa sortie qui viole le contrat publié).
                //
                // **Forme de la sortie** : le `structuredContent` doit
                // conformer au schéma **publié**. L'enveloppe
                // `{ "result": … }` (choix d'API : `structuredContent`
                // toujours objet) n'est applicable que si le schéma
                // l'autorise — sinon elle serait elle-même non conforme
                // et aucun outil à `outputSchema` de racine non-objet ne
                // pourrait jamais réussir.
                match &tool.output_schema {
                    None => (
                        CallToolResult::from(Ok::<Value, ToolError>(value)),
                        CallStatus::Success,
                    ),
                    Some(output) => {
                        // Le schéma est-il d'objet à la racine ? Alors
                        // l'enveloppe invariante est candidate ; sinon la
                        // valeur brute est envoyée telle quelle (la spec
                        // 2026-07-28 / SEP-2106 autorise toute valeur JSON
                        // dans `structuredContent`).
                        //
                        // Le drapeau est **précalculé** à l'enregistrement
                        // (`ToolDef::output_object_rooted`) : le schéma
                        // n'est donc pas re-sérialisé à chaque appel.
                        let structured = if tool.output_object_rooted && value.is_object() {
                            value
                        } else if tool.output_object_rooted {
                            serde_json::json!({ "result": value })
                        } else {
                            value
                        };
                        let mut issues = Issues::new();
                        let mut path = Path::root("output");
                        output.validate(&mut path, &structured, &mut issues);
                        if issues.is_empty() {
                            (
                                CallToolResult::from_structured(structured),
                                CallStatus::Success,
                            )
                        } else {
                            // Violation du contrat de sortie publié : échec
                            // interne (non retentable — retenter produirait
                            // la même sortie non conforme).
                            let rendered = issues.render().join(" ; ");
                            let error = ToolError::internal(format!(
                                "structured output does not conform to the declared outputSchema: {rendered}"
                            ));
                            let code = error.code;
                            (
                                CallToolResult::from(Err::<Value, ToolError>(error)),
                                CallStatus::ToolFailure(code),
                            )
                        }
                    }
                }
            }
            Err(ToolFailure::Tool(error)) => {
                let code = error.code;
                (
                    CallToolResult::from(Err::<Value, ToolError>(error)),
                    CallStatus::ToolFailure(code),
                )
            }
            Err(ToolFailure::Protocol(code, message)) => {
                return (
                    Outgoing::failure(id.clone(), ErrorObject::new(code, message)).to_vec(),
                    CallStatus::ProtocolError(code),
                );
            }
        };
        // SEP-2575 : le serveur s'identifie dans chaque résultat — succès
        // comme échec d'exécution (`isError: true` reste un `result`).
        // Sérialisation directe vers `Vec<u8>` par référence sans passer par
        // un arbre `serde_json::Value` intermédiaire ni cloner `self.meta`.
        (
            Outgoing::success_vec(
                id,
                &CallToolResultRef {
                    result_type: result.result_type,
                    content: &result.content,
                    is_error: result.is_error,
                    structured_content: result.structured_content.as_ref(),
                    meta: &self.meta,
                },
            ),
            status,
        )
    }
}

/// Dérive la liste `PromptArgument` de `prompts/list` à partir du JSON Schema
/// d'un prompt typé (`prompt_typed` / `#[prompt]`).
fn derive_prompt_arguments(schema_json: &Value) -> Option<Vec<crate::protocol::PromptArgument>> {
    let properties = schema_json.get("properties")?.as_object()?;
    let required: Vec<&str> = schema_json
        .get("required")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let arguments = properties
        .iter()
        .map(|(name, prop)| crate::protocol::PromptArgument {
            name: name.clone(),
            description: prop
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            required: Some(required.contains(&name.as_str())),
        })
        .collect();
    Some(arguments)
}

/// Nom d'outil/prompt ou `uri` de ressource d'une requête, pour
/// l'observation ([`CallEvent::name`]) — extraction best-effort depuis
/// `params`, `None` si la méthode n'en porte pas.
fn requested_name<'a>(method: &str, params: &'a Value) -> Option<&'a str> {
    let key = if method == protocol::method::RESOURCES_READ {
        "uri"
    } else {
        "name"
    };
    params.get(key).and_then(Value::as_str)
}

/// Le schéma de sortie est-il **d'objet à la racine** ?
///
/// Décrit le `structuredContent` publié : s'il attend un objet, l'enveloppe
/// invariante `{ "result": … }` reste applicable ; s'il attend autre chose
/// (un scalaire, un tableau, un `anyOf`), la valeur brute doit être
/// envoyée telle quelle — l'enveloppe serait elle-même non conforme au
/// schéma publié (la spec `2026-07-28` / SEP-2106 autorise toute valeur
/// JSON dans `structuredContent`).
fn output_is_object_rooted(schema: &dyn Schema) -> bool {
    let json = schema.json();
    let Some(object) = json.as_object() else {
        return false;
    };
    match object.get("type") {
        Some(Value::String(kind)) => kind == "object",
        // Pas de `type` explicite : un objet reste le cas par défaut
        // (convention JSON Schema pour un schéma de propriétés).
        None => object.contains_key("properties") || object.contains_key("required"),
        Some(_) => false,
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
    /// Hooks d'observation enregistrés par [`Self::on_call`].
    hooks: Vec<Arc<CallHook>>,
    /// Hooks de requête enregistrés par [`Self::on_request`] — verdict
    /// d'accès (auth, rate-limit) avant lecture du corps.
    request_hooks: Vec<Arc<RequestHook>>,
    /// Plafond d'octets d'une réponse JSON-RPC sérialisée (`0` : illimité).
    max_response_bytes: usize,
    /// Annonce au MCP Control (voir [`Self::announce`]).
    announce: bool,
    /// Module anti-zombie (voir [`Self::anti_zombie`]) : sweep du registre
    /// au bind, drain borné et bilan de fin de vie.
    anti_zombie: bool,
    /// Durée maximale du drain de fin de vie (voir [`Self::drain_timeout`]).
    drain_timeout: Duration,
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
            hooks: Vec::new(),
            request_hooks: Vec::new(),
            max_response_bytes: MAX_RESPONSE_BYTES,
            announce: false,
            anti_zombie: false,
            drain_timeout: crate::antizombie::DRAIN_TIMEOUT,
        }
    }

    /// Annonce ce serveur au MCP Control : au bind réussi, un JSON
    /// (`%TEMP%/litemcp-registry/<nom>.json`) publie le port effectif, le PID
    /// et la ligne de commande du process — MCP Control peut alors le démarrer,
    /// l'arrêter et le relancer nativement, sans scan de ports ni saisie
    /// manuelle. L'annonce est retirée à la fin de vie du serveur lié —
    /// arrêt gracieux **ou** `drop` sans `serve()` (voir le `Drop` de
    /// `BoundServer`). Opt-in, zéro coût quand il n'est pas appelé.
    #[must_use]
    pub fn announce(mut self) -> Self {
        self.announce = true;
        self
    }

    /// Active le module **anti-zombie** (voir [`crate::antizombie`]) :
    ///
    /// - **au bind**, le registre d'annonce partagé est passé au peigne fin —
    ///   les annonces de process morts (port sans réponse) et les débris
    ///   d'écritures interrompues sont retirés, les instances vivantes et
    ///   les fichiers d'un schéma inconnu sont respectés ;
    /// - **à la fin de vie** du serveur lié (arrêt gracieux ou `drop`), les
    ///   threads de connexion sont drainés — attente bornée par
    ///   [`Self::drain_timeout`] — et le bilan
    ///   ([`crate::antizombie::ZombieReport`]) devient lisible via
    ///   [`crate::http::ServerHandle::zombie_report`].
    ///
    /// Opt-in strict : sans cet appel, aucun sweep, aucun drain, aucun code
    /// de plus sur les chemins chauds.
    #[must_use]
    pub fn anti_zombie(mut self) -> Self {
        self.anti_zombie = true;
        self
    }

    /// Durée maximale du **drain** anti-zombie à la fin de vie
    /// (défaut [`crate::antizombie::DRAIN_TIMEOUT`] — 2 s). Les threads ne
    /// sont jamais tués (Rust ne le permet pas) : au-delà de la durée, le
    /// bilan signale `drained: false` avec le compte de connexions
    /// restantes. `0` : bilan immédiat, sans attente. Sans effet si
    /// [`Self::anti_zombie`] n'est pas actif.
    #[must_use]
    pub fn drain_timeout(mut self, timeout: Duration) -> Self {
        self.drain_timeout = timeout;
        self
    }

    /// Hook d'**observabilité** : un callback appelé après le traitement
    /// complet de chaque message JSON-RPC (succès, échec d'outil, erreur de
    /// protocole, notification).
    ///
    /// Un seul callback donne compteurs par outil, latence et taux d'erreur
    /// — sans `MOLTBOOK_DEBUG` ni `eprintln` dans votre client :
    ///
    /// ```no_run
    /// use litemcp::prelude::*;
    ///
    /// # fn main() -> std::io::Result<()> {
    /// ServerBuilder::new("calc", "1.0.0")
    ///     .tool("add", "Additionne", |_args| Ok(serde_json::json!({})))
    ///     .on_call(|event| {
    ///         eprintln!(
    ///             "{} {} → {:?} ({:?})",
    ///             event.method,
    ///             event.name.unwrap_or("-"),
    ///             event.status,
    ///             event.duration
    ///         );
    ///     })
    ///     .bind("127.0.0.1:23080")?
    ///     .serve()?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// - Un événement par message, **après** son traitement —
    ///   [`CallEvent::duration`] couvre le dispatch complet (validation
    ///   comprise) et l'exécution du handler ;
    /// - les corps JSON illisibles ne produisent **pas** d'événement (il
    ///   n'y a ni méthode ni outil à rapporter) ;
    /// - une panique du hook est attrapée : l'observation n'interrompt
    ///   jamais le service ;
    /// - les appels sont cumulables (chaque `.on_call` ajoute un hook) ;
    ///   le hook ne doit pas bloquer (il s'exécute dans le fil de la
    ///   requête).
    #[must_use]
    pub fn on_call<F>(mut self, f: F) -> Self
    where
        F: Fn(&CallEvent<'_>) + Send + Sync + 'static,
    {
        self.hooks.push(Arc::new(f));
        self
    }

    /// Verrou d'accès par requête — **auth** et **rate-limit** en process,
    /// avant toute exécution : le hook reçoit
    /// [`RequestContext`](crate::http::RequestContext) (méthode, chemin,
    /// query, adresse du pair, en-têtes) et rend `Ok(())` ou un
    /// [`RequestRejection`](crate::http::RequestRejection) (`401`, `403`,
    /// `429` + `Retry-After`, statut arbitraire). Le premier rejet gagne.
    ///
    /// ```no_run
    /// use litemcp::prelude::*;
    ///
    /// const TOKEN: &str = "s3cret";
    ///
    /// # fn main() -> std::io::Result<()> {
    /// ServerBuilder::new("calc", "1.0.0")
    ///     .tool("add", "Additionne", |_args| Ok(serde_json::json!({})))
    ///     .on_request(|ctx| {
    ///         match ctx.header("Authorization").and_then(|v| v.strip_prefix("Bearer ")) {
    ///             Some(token) if token == TOKEN => Ok(()),
    ///             _ => Err(RequestRejection::unauthorized("missing or invalid bearer token")
    ///                 .with_header("WWW-Authenticate", "Bearer")),
    ///         }
    ///     })
    ///     .bind("127.0.0.1:23080")?
    ///     .serve()?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// - le hook s'exécute **avant** la lecture du corps et avant le
    ///   `Expect: 100-continue` : un rejet ne transporte jamais le payload ;
    /// - il ne s'exécute **pas** sur `/health` (les probes k8s ne
    ///   s'authentifient pas) ni sur une route inconnue (`404`) ni sur les
    ///   requêtes déjà refusées par le durcissement de transport (`400`,
    ///   `431`, `403` Origin, `411`, `413`) — le verrou utilisateur ne fait
    ///   qu'autoriser la route MCP ;
    /// - il s'exécute **avant** le verdict de méthode (`405`) : une requête
    ///   non autorisée ne doit pas apprendre que la route existe autrement
    ///   que par un refus d'authentification (`401`/`403` gagne sur `405`) ;
    /// - un rejet répond le statut avec le message en corps JSON et **ferme
    ///   la connexion** (le corps non lu ne peut pas être réinterprété) ;
    /// - les en-têtes supplémentaires ([`RequestRejection::with_header`])
    ///   sont assainis au départ : un CRLF injecté dans une valeur est
    ///   écarté, jamais réémis ;
    /// - une panique du hook est contenue (`500`, connexion fermée, le
    ///   serveur continue) ;
    /// - le hook tourne dans le fil de la connexion : gardez-le rapide
    ///   (vérification locale de signature/JWT) — un appel réseau distant
    ///   (introspection OAuth) doit porter **son propre** timeout, sinon il
    ///   retient le slot de connexion ;
    /// - derrière un reverse proxy, [`RequestContext::peer`](crate::http::RequestContext)
    ///   est l'adresse du proxy — pour un rate-limit par client final,
    ///   lisez `X-Forwarded-For` **après** avoir vérifié que `peer` est bien
    ///   votre proxy ;
    /// - les hooks sont cumulables (chaque `.on_request` ajoute un verdict,
    ///   exécuté dans l'ordre d'enregistrement) ; sans aucun hook, le
    ///   transport ne retient aucun en-tête et le chemin chaud est inchangé.
    #[must_use]
    pub fn on_request<F>(mut self, f: F) -> Self
    where
        F: Fn(&RequestContext<'_>) -> Result<(), RequestRejection> + Send + Sync + 'static,
    {
        self.request_hooks.push(Arc::new(f));
        self
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

    /// Plafond de taille d'une réponse JSON-RPC sérialisée (octets).
    ///
    /// Défaut : [`MAX_RESPONSE_BYTES`] (16 Mio). `0` = illimité (le contrôle
    /// est désactivé).
    ///
    /// Le contrôle est appliqué sur la réponse **après** sérialisation : il
    /// borne le transit, pas l'allocation. Pour borner la mémoire, c'est la
    /// taille de la **valeur rendue par le handler** qu'il faut surveiller
    /// (un handler qui rend 100 Mo consomme 100 Mo avant d'être refusé) —
    /// ce plafond protège le réseau, pas le tas.
    #[must_use]
    pub fn max_response_bytes(mut self, max: usize) -> Self {
        self.max_response_bytes = max;
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
        let schema = Arc::new(schema);
        if let Some(def) = self.tools.get_mut(tool) {
            def.output_schema = Some(schema);
            // La forme du `structuredContent` dépend du schéma de sortie :
            // elle change avec lui.
            def.sync_output_shape();
        } else {
            self.output_schemas.insert(tool.to_string(), schema);
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
                schema: None,
                f: Arc::new(f),
            },
        );
        self
    }

    /// Comme [`ServerBuilder::prompt`], avec un **schéma à la Zod** (module
    /// [`crate::zod`]) : les `arguments` d'un `prompts/get` sont validés
    /// **avant** l'exécution du handler — hors schéma → `-32602` avec la
    /// liste des problèmes dans `error.data.issues`, le handler ne tourne
    /// jamais. C'est la variante utilisée par la macro `#[prompt]` (feature
    /// `macros`).
    pub fn prompt_typed<S, F>(mut self, name: &str, description: &str, schema: S, f: F) -> Self
    where
        S: Schema + 'static,
        F: Fn(&Value) -> Result<Vec<PromptMessage>, String> + Send + Sync + 'static,
    {
        self.prompts.insert(
            name.to_string(),
            PromptDef {
                description: Some(description.to_string()),
                schema: Some(Arc::new(schema)),
                f: Arc::new(f),
            },
        );
        self
    }

    /// Enchaîne un enregistrement généré par une macro (`#[tool]`,
    /// `#[resource]`, `#[prompt]` — feature `macros`) :
    /// `builder.with_tool(register_add)` où `register_add` est la fonction
    /// générée à côté de votre fonction annotée.
    #[must_use]
    pub fn with_tool(self, register: impl FnOnce(Self) -> Self) -> Self {
        register(self)
    }

    /// Récupère le schéma de sortie et les annotations d'un outil.
    ///
    /// Les déclarations **en attente** (`.output_schema()` / `.annotate()`
    /// appelées avant l'enregistrement de l'outil) priment ; à défaut, ce
    /// sont ceux de l'outil **déjà enregistré** qui sont repris.
    ///
    /// Sans cette conservation, un ré-enregistrement
    /// (`.tool("x", …).tool("x", …)`) perdait silencieusement les
    /// annotations et l'`outputSchema` déjà déclarés — le `tools/list`
    /// publié ne décrivait alors plus l'outil réellement servi.
    fn take_output_and_annotations(
        &mut self,
        name: &str,
    ) -> (Option<Arc<dyn Schema>>, Option<ToolAnnotations>) {
        let previous_output = self
            .tools
            .get(name)
            .and_then(|def| def.output_schema.clone());
        let previous_annotations = self.tools.get(name).and_then(|def| def.annotations.clone());
        let output_schema = self.output_schemas.remove(name).or(previous_output);
        let annotations = self.annotations.remove(name).or(previous_annotations);
        (output_schema, annotations)
    }

    /// Retire un outil du registre — le socle des gateways dynamiques
    /// (module [`crate::gateway`]) : un serveur détecté disparaît, son outil
    /// doit disparaître de `tools/list` sans redémarrer le process.
    ///
    /// Retourne `true` si l'outil existait. Les annotations et schémas de
    /// sortie **en attente** pour ce nom sont purgés en même temps : un
    /// ré-enregistrement ultérieur du même nom repart propre, sans
    /// ressusciter des métadonnées que l'opérateur a explicitement retirées.
    /// Un outil absent → `false`, aucun autre changement.
    pub fn remove_tool(&mut self, name: &str) -> bool {
        let existed = self.tools.remove(name).is_some();
        self.annotations.remove(name);
        self.output_schemas.remove(name);
        existed
    }

    /// Retire une **ressource** du registre — le pendant retrait de
    /// [`Self::remove_tool`] pour les ressources : un serveur détecté
    /// disparaît, sa ressource disparaît de `resources/list`.
    ///
    /// Retourne `true` si la ressource existait. Une ressource absente →
    /// `false`, aucun autre changement.
    pub fn remove_resource(&mut self, uri: &str) -> bool {
        self.resources.remove(uri).is_some()
    }

    /// Retire un **prompt** du registre — le pendant retrait de
    /// [`Self::remove_tool`] pour les prompts.
    ///
    /// Retourne `true` si le prompt existait. Un prompt absent → `false`,
    /// aucun autre changement.
    pub fn remove_prompt(&mut self, name: &str) -> bool {
        self.prompts.remove(name).is_some()
    }

    /// Noms des outils enregistrés, dans l'ordre alphabétique (ordre du
    /// `tools/list` servi). Utile aux tests et aux gateways qui veulent
    /// comparer le registre à leur état détecté sans sérialiser.
    #[must_use]
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// Enregistre un outil.
    ///
    /// La closure reçoit `arguments` (objet JSON — un `arguments` non-objet
    /// est refusé en `-32602` avant tout appel du handler) et renvoie la
    /// valeur structurée, ou un message d'erreur lisible par le client.
    /// Réenregistrer un nom existant remplace l'outil précédent (le
    /// dernier enregistré gagne) — **les annotations et l'`outputSchema`
    /// déjà déclarés sont conservés**.
    pub fn tool<F>(mut self, name: &str, description: &str, f: F) -> Self
    where
        F: Fn(&Value) -> ToolResult + Send + Sync + 'static,
    {
        let wrapped = move |args: &Value| f(args).map_err(ToolFailure::Tool);
        let (output_schema, annotations) = self.take_output_and_annotations(name);
        let mut def = ToolDef {
            description: Some(description.to_string()),
            schema: None,
            output_schema,
            output_object_rooted: false,
            annotations,
            f: Arc::new(wrapped),
        };
        def.sync_output_shape();
        self.tools.insert(name.to_string(), def);
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
        let (output_schema, annotations) = self.take_output_and_annotations(name);
        let mut def = ToolDef {
            description: Some(description.to_string()),
            schema: Some(Arc::new(schema)),
            output_schema,
            output_object_rooted: false,
            annotations,
            f: Arc::new(wrapped),
        };
        def.sync_output_shape();
        self.tools.insert(name.to_string(), def);
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
    ///     .bind("127.0.0.1:23080")?
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
                format!("unreadable arguments: {error}"),
            )),
            Ok(buffer) => match serde_json::from_slice::<T>(&buffer) {
                Ok(parsed) => f(parsed).map_err(ToolFailure::Tool),
                Err(error) => Err(ToolFailure::Protocol(
                    ErrorObject::INVALID_PARAMS,
                    format!("arguments do not match the expected type: {error}"),
                )),
            },
        });
        let (output_schema, annotations) = self.take_output_and_annotations(name);
        let mut def = ToolDef {
            description: Some(description.to_string()),
            schema: Some(Arc::new(schema)),
            output_schema,
            output_object_rooted: false,
            annotations,
            f: handler,
        };
        def.sync_output_shape();
        self.tools.insert(name.to_string(), def);
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

    /// Fusionne dans `tools` les annotations et schémas de sortie **en
    /// attente** — déclarés avant l'enregistrement de l'outil, ou liés à un
    /// outil réenregistré — et recalcule la forme du `structuredContent`
    /// associée (elle dépend du schéma de sortie). Utilisé par
    /// [`Self::core`] et [`Self::bind`].
    fn merge_pending(&self, tools: &mut BTreeMap<String, ToolDef>) {
        for (name, def) in tools.iter_mut() {
            if let Some(annotations) = self.annotations.get(name) {
                def.annotations = Some(annotations.clone());
            }
            if let Some(output) = self.output_schemas.get(name) {
                def.output_schema = Some(Arc::clone(output));
            }
            def.sync_output_shape();
        }
    }

    /// Construit le cœur de dispatch (tests, intégrations custom).
    ///
    /// **Instantané** : le cœur capture les outils présents à cet instant.
    /// Un outil enregistré *après* l'appel n'y figure pas — utilisez
    /// [`ServerBuilder::bind`] pour un serveur complet.
    #[must_use]
    pub fn core(&self) -> ServerCore {
        let mut tools = self.tools.clone();
        self.merge_pending(&mut tools);
        let meta = json!({
            protocol::META_SERVER_INFO: ServerInfo {
                name: self.name.clone(),
                version: self.version.clone(),
            }
        });
        ServerCore {
            name: self.name.clone(),
            version: self.version.clone(),
            meta,
            tools: Arc::new(tools),
            resources: Arc::new(self.resources.clone()),
            prompts: Arc::new(self.prompts.clone()),
            tool_timeout: self.tool_timeout,
            max_inflight_handlers: MAX_INFLIGHT_HANDLERS,
            inflight_handlers: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            max_response_bytes: self.max_response_bytes,
            hooks: Arc::new(self.hooks.clone()),
            tools_list_cache: std::sync::OnceLock::new(),
            resources_list_cache: std::sync::OnceLock::new(),
            prompts_list_cache: std::sync::OnceLock::new(),
            discover_cache: std::sync::OnceLock::new(),
            initialize_cache: std::sync::OnceLock::new(),
            ping_cache: std::sync::OnceLock::new(),
        }
    }

    /// Bind le socket TCP — puis [`crate::http::BoundServer::serve`] bloque.
    ///
    /// La route est validée ici (voir [`ServerBuilder::route`]) : une route
    /// qui ne peut jamais matcher ou qui masque la sonde de vie est refusée
    /// en `InvalidInput` plutôt que de démarrer un serveur injoignable.
    pub fn bind(self, addr: &str) -> std::io::Result<crate::http::BoundServer> {
        self.validate_route()?;
        let listener = std::net::TcpListener::bind(addr)?;
        self.finish(listener)
    }

    /// Bind sur `127.0.0.1` en **cherchant un port libre** dans la tranche
    /// LiteMCP ([`crate::DEFAULT_PORT_RANGE`] — `23080..=23099`) : le
    /// premier port disponible gagne, de sorte que le deuxième, le
    /// dixième… serveur LiteMCP d'une machine démarre sans coordination
    /// ni collision. Si la tranche entière est occupée, repli sur un
    /// **port éphémère** alloué par l'OS — jamais d'échec faute de place
    /// dans la tranche. Le port effectif est lisible via
    /// [`crate::http::BoundServer::local_addr`] et publié par
    /// [`ServerBuilder::announce`] si elle est active. La validation de
    /// route est identique à [`ServerBuilder::bind`].
    ///
    /// ```no_run
    /// use litemcp::prelude::*;
    ///
    /// # fn main() -> std::io::Result<()> {
    /// let bound = ServerBuilder::new("calc", "1.0.0")
    ///     .tool("add", "Additionne", |_args| Ok(serde_json::json!({})))
    ///     .bind_default()?;
    /// eprintln!("MCP sur http://{}", bound.local_addr()?);
    /// bound.serve()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn bind_default(self) -> std::io::Result<crate::http::BoundServer> {
        self.validate_route()?;
        for port in crate::DEFAULT_PORT_RANGE {
            match std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)) {
                Ok(listener) => return self.finish(listener),
                // Port pris : le suivant de la tranche — le cas nominal du
                // deuxième serveur LiteMCP sur la même machine.
                Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {}
                Err(error) => return Err(error),
            }
        }
        // Tranche entière occupée : port éphémère (0). L'OS ne peut pas
        // donner un port de la tranche — elle est sous toutes les plages
        // éphémères par défaut et ses vingt ports sont liés ci-dessus.
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        self.finish(listener)
    }

    /// Validation de route partagée par [`Self::bind`] et
    /// [`Self::bind_default`] — voir [`Self::route`].
    fn validate_route(&self) -> std::io::Result<()> {
        if !self.route.starts_with('/')
            || self.route.contains(['?', '#'])
            || self
                .route
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || !c.is_ascii())
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "route invalide {:?} : elle doit commencer par '/', être en ASCII visible sans espace et ne pas contenir '?' ni '#'",
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
        Ok(())
    }

    /// Construit le [`crate::http::BoundServer`] sur un listener **déjà
    /// lié** — corps commun de [`Self::bind`] et [`Self::bind_default`].
    fn finish(
        mut self,
        listener: std::net::TcpListener,
    ) -> std::io::Result<crate::http::BoundServer> {
        // Anti-zombie : le sweep du registre partagé précède notre propre
        // annonce — le dossier est propre au moment où nous y publions.
        let zombie = if self.anti_zombie {
            Some(Arc::new(crate::antizombie::ZombieState::new(
                crate::antizombie::sweep_registry(),
                self.drain_timeout,
            )))
        } else {
            None
        };
        // Annonce MCP Control : le listener est lié — le port effectif est
        // connu même après un bind sur le port 0. Un échec d'écriture est
        // silencieux (commodité d'intégration, jamais une condition).
        let mut announce_name = None;
        if self.announce {
            let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
            if port != 0 {
                crate::registry::announce(&self.name, &self.version, port);
                announce_name = Some((self.name.clone(), port));
            }
        }
        // `mem::take` plutôt qu'un move direct : `merge_pending` emprunte
        // `self` (annotations en attente) — un move partiel de `self.tools`
        // rendrait ce prêt illégal. Le builder n'est plus utilisé ensuite.
        let mut tools = std::mem::take(&mut self.tools);
        // Fusionne les annotations et schémas de sortie en attente (outil
        // annoté avant d'être enregistré, ou réenregistré plus tard).
        self.merge_pending(&mut tools);
        let meta = json!({
            protocol::META_SERVER_INFO: ServerInfo {
                name: self.name.clone(),
                version: self.version.clone(),
            }
        });
        let core = Arc::new(ServerCore {
            name: self.name,
            version: self.version,
            meta,
            tools: Arc::new(tools),
            resources: Arc::new(self.resources),
            prompts: Arc::new(self.prompts),
            tool_timeout: self.tool_timeout,
            max_inflight_handlers: MAX_INFLIGHT_HANDLERS,
            inflight_handlers: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            max_response_bytes: self.max_response_bytes,
            hooks: Arc::new(self.hooks),
            tools_list_cache: std::sync::OnceLock::new(),
            resources_list_cache: std::sync::OnceLock::new(),
            prompts_list_cache: std::sync::OnceLock::new(),
            discover_cache: std::sync::OnceLock::new(),
            initialize_cache: std::sync::OnceLock::new(),
            ping_cache: std::sync::OnceLock::new(),
        });
        Ok(crate::http::BoundServer {
            listener,
            core,
            route: Arc::new(self.route),
            allow_origins: Arc::new(self.allow_origins),
            request_hooks: Arc::new(self.request_hooks),
            read_timeout: self.read_timeout,
            max_connections: self.max_connections,
            connection_ttl: self.connection_ttl,
            live_connections: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            refusals_in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            announce_name,
            zombie,
        })
    }
}

#[cfg(test)]
mod tests_limites_2026_10_25 {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    /// Le plafond de threads de handlers refuse le surplus (`Overloaded`) et
    /// rend le slot dès que le handler détaché termine (garde `Drop`).
    #[test]
    fn inflight_cap_refuses_extra_handler_threads_and_releases_slots() {
        let inflight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let barrier = Arc::new(std::sync::Barrier::new(2));

        let (b, i) = (Arc::clone(&barrier), Arc::clone(&inflight));
        let first = std::thread::spawn(move || {
            run_with_timeout(Duration::from_secs(5), &i, 1, move || {
                b.wait();
                1u8
            })
        });

        let mut tours = 0;
        while inflight.load(Ordering::Acquire) == 0 {
            tours += 1;
            assert!(
                tours < 1_000_000,
                "le premier handler n'a jamais pris son slot"
            );
            std::thread::yield_now();
        }

        let second = run_with_timeout(Duration::from_secs(5), &inflight, 1, || 2u8);
        assert!(
            matches!(second, Outcome::Overloaded),
            "plafond d'inflight non appliqué"
        );

        barrier.wait();
        assert!(matches!(first.join().unwrap(), Outcome::Done(1)));

        let mut tours = 0;
        while inflight.load(Ordering::Acquire) != 0 {
            tours += 1;
            assert!(tours < 1_000_000, "slot inflight jamais rendu");
            std::thread::yield_now();
        }
    }
}
