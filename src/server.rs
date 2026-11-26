//! Noyau de dispatch JSON-RPC + builder — indépendant du transport.

use crate::auth::{self, AuthContext};
use crate::error::{ErrorCode, ToolError};
use crate::http::{RequestContext, RequestHook, RequestRejection};
use crate::jsonrpc::{BoundedReply, ErrorObject, Outgoing, RequestId};
use crate::protocol;
use crate::protocol::{
    CacheScope, CallToolResult, CompleteReference, CompleteResult, Completion, CompletionArgument,
    CompletionRequest, ContentBlock, DiscoverResult, InputRequiredSpec, Prompt, PromptMessage,
    PromptsListResult, Resource, ResourceContents, ResourceData, ResourceTemplate,
    ResourceTemplatesListResult, ResourcesListResult, ResultType, ServerInfo, Tool,
    ToolAnnotations, ToolsListResult,
};
use crate::request_state::{
    DEFAULT_REQUEST_STATE_TTL, MAX_REQUEST_STATE_CHARS, MAX_STATE_PAYLOAD_BYTES, digest_salient,
    harvest_key, mint_token, verify_token,
};
use crate::zod::{Issues, Path, Schema};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod authz;
mod builder;
mod catalog;
mod dispatch;
mod handlers;
mod pool;

pub(crate) use authz::audit_target;
use catalog::Catalogue;
use dispatch::McpParamDef;
use pool::HandlerPool;

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
/// [`ServerBuilder::on_call`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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
/// [`ServerBuilder::on_call`].
///
/// Émis **une fois par message** après son traitement complet (succès ou
/// échec) : compteurs par outil, latence, taux d'erreur se déduisent d'un
/// unique callback.
#[derive(Debug, Clone)]
#[non_exhaustive]
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

/// Sortie d'un handler d'outil — la valeur structurée de l'API historique
/// ([`ToolResult`]), un [`CallToolResult`] complet (contenus riches : image,
/// audio, liens, ressources embarquées, contenus multiples) ou une demande
/// d'entrée MRTR ([`ToolOutcome::InputRequired`]).
#[derive(Debug)]
#[non_exhaustive]
pub enum ToolOutcome {
    /// Chemin historique : la valeur devient `structuredContent` (avec
    /// enveloppe `{ "result": … }` si non-objet) et le texte reflète
    /// exactement la valeur envoyée.
    Value(Value),
    /// Chemin riche : le handler a construit le résultat complet — contenu
    /// mixte, `structuredContent` explicite, `isError` choisi.
    Result(CallToolResult),
    /// Le handler demande une entrée client avant de finir (MRTR — spec
    /// `2026-07-28`) : le noyau valide les requêtes embarquées (méthode
    /// embarquable + capacité client déclarée, sinon `-32021`), signe
    /// `state` en un `requestState` opaque (HMAC-SHA256, TTL, liaison
    /// méthode/paramètres saillants/tour, identité de l'appelant sous
    /// autorisation) et répond `resultType: "input_required"`.
    InputRequired(InputRequiredSpec),
}

type ToolFn = dyn Fn(&Value) -> Result<ToolOutcome, ToolFailure> + Send + Sync;
type ResourceFn = dyn Fn() -> Result<ResourceData, ToolError> + Send + Sync;
type PromptFn = dyn Fn(&Value) -> Result<Vec<PromptMessage>, ToolError> + Send + Sync;

/// Handler d'outil MRTR — reçoit les arguments **et** le contexte du tour
/// ([`InputRound`]) : réponses du client (non fiables) et état vérifié.
type MrtrToolFn = dyn Fn(&Value, &InputRound) -> Result<ToolOutcome, ToolError> + Send + Sync;

/// Handler de ressource MRTR — `resources/read` supporte le MRTR (spec).
type MrtrResourceFn = dyn Fn(&InputRound) -> Result<ResourceOutcome, ToolError> + Send + Sync;

/// Handler de prompt MRTR — `prompts/get` supporte le MRTR (spec).
type MrtrPromptFn = dyn Fn(&Value, &InputRound) -> Result<PromptOutcome, ToolError> + Send + Sync;
type CallHook = dyn Fn(&CallEvent<'_>) + Send + Sync;

/// Sortie d'un passage MRTR d'un handler de prompt : les messages du prompt
/// ou une demande d'entrée (`prompts/get` est supporté par le MRTR).
#[derive(Debug, Clone)]
pub enum PromptOutcome {
    /// Messages du prompt (chemin historique).
    Messages(Vec<PromptMessage>),
    /// Demande d'entrée client — mêmes règles fil que pour les outils.
    InputRequired(InputRequiredSpec),
}

/// Sortie d'un passage MRTR d'un handler de ressource : le contenu lu ou une
/// demande d'entrée (`resources/read` est supporté par le MRTR).
#[derive(Debug, Clone)]
pub enum ResourceOutcome {
    /// Contenu de la ressource (chemin historique).
    Data(ResourceData),
    /// Demande d'entrée client — mêmes règles fil que pour les outils.
    InputRequired(InputRequiredSpec),
}

/// Contexte d'un passage MRTR, remis à un handler `tool_mrtr`/`prompt_mrtr`/
/// `resource_mrtr` à **chaque** tour.
///
/// Entièrement stateless : tout ce qui traverse les tours voyage soit dans
/// les réponses du client (non fiables), soit dans le `requestState` vérifié
/// par le noyau (HMAC + TTL + liaison méthode/paramètres saillants/tour,
/// identité de l'appelant sous autorisation) — jamais dans
/// un état de connexion, de session ou de process. Un rejeu peut atterrir
/// sur une autre instance (même clé configurée) et reprendre.
#[derive(Clone, Debug)]
pub struct InputRound {
    /// Tour courant — `1` au premier passage, `t+1` quand un `requestState`
    /// vérifié a été restauré.
    pub turn: u32,
    /// Réponses du client (`params.inputResponses`) — map **non fiable** dont
    /// le noyau a validé la structure (objet de valeurs-objets ; les clés non
    /// reconnues sont ignorées, comme le permet la spec). `None` si la
    /// requête n'en porte pas.
    pub input_responses: Option<Value>,
    /// Charge utile du `requestState` reçu, **après vérification** — la
    /// charge fournie par le handler au tour précédent via
    /// [`InputRequiredSpec::with_state`]. `None` au premier passage ou si le
    /// client n'a pas renvoyé de jeton.
    pub state: Option<Value>,
}

impl InputRound {
    pub(crate) fn first() -> Self {
        Self {
            turn: 1,
            input_responses: None,
            state: None,
        }
    }

    /// Réponse brute du client pour la clé `key` — toute valeur JSON ; c'est
    /// au handler de décider de l'acceptation (action, contenu…).
    #[must_use]
    pub fn response(&self, key: &str) -> Option<&Value> {
        self.input_responses.as_ref()?.get(key)
    }

    /// Contenu d'une réponse d'élicitation **acceptée** pour `key` — `None`
    /// si absente ou si `action != "accept"` (déclinée/annulée : un refus
    /// d'entrée n'est pas une entrée).
    #[must_use]
    pub fn accepted(&self, key: &str) -> Option<&Value> {
        let response = self.response(key)?;
        (response.get("action").and_then(Value::as_str) == Some("accept"))
            .then(|| response.get("content"))
            .flatten()
    }
}

/// Fournisseur de complétion ([`ServerBuilder::completions`]) — reçoit la
/// requête validée et rend les suggestions (au plus 100 : le noyau tronque
/// au-delà et pose `hasMore`).
type CompletionFn = dyn Fn(&CompletionRequest) -> Result<Completion, ToolError> + Send + Sync;

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

/// Délai d'exécution par défaut des handlers — **30 s**.
///
/// Un handler qui dépasse ce délai rend un échec (`isError` pour un outil,
/// `-32603` avec `data.reason = "timeout"` sinon) au lieu de retenir
/// indéfiniment un thread de connexion : sans borne, `MAX_CONNECTIONS`
/// threads de connexion suffisaient à saturer le serveur avec autant de
/// handlers bloqués. C'est aussi ce qui active [`MAX_INFLIGHT_HANDLERS`].
///
/// Pour revenir à un délai illimité (comportement antérieur, à assumer),
/// voir [`ServerBuilder::no_tool_timeout`].
pub const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(30);

/// Plafond par défaut de taille d'une réponse JSON-RPC sérialisée (16 Mio).
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Borne de la spec sur `completion.values` (au plus 100 suggestions) —
/// le noyau tronque au-delà et pose `hasMore: true` si nécessaire.
pub const MAX_COMPLETION_VALUES: usize = 100;

/// Plafond de caractères reflétés dans un message d'erreur JSON-RPC pour un
/// identifiant fourni par le client (`method`, `name`, `uri`, version).
const MAX_ECHO_CHARS: usize = 128;

/// Défaillance d'exécution serveur — échec d'un handler de
/// ressource/prompt/complétion, dépassement de [`ServerBuilder::tool_timeout`]
/// ou saturation des threads de handlers : code interne JSON-RPC `-32603`
/// avec `data` structuré (`reason`: `handler_failed`/`timeout`/`overloaded`).
///
/// MCP 2026-07-28 déconseille d'introduire de nouveaux codes dans la plage
/// implementation-defined `-32000..-32019` : ces défaillances sont des
/// erreurs internes du serveur et reprennent le code réservé `-32603` — la
/// cause précise reste exprimée dans `data.reason`. Les échecs d'outils,
/// eux, restent des résultats `isError` (jamais des erreurs JSON-RPC).
const HANDLER_FAILURE: i64 = ErrorObject::INTERNAL_ERROR;

/// Échec remonté par un wrapper de handler :
/// - `Tool(ToolError)` → échec d'exécution (`isError: true`, HTTP 200) ;
/// - `Protocol(i64, String)` → erreur de protocole (ex. arguments qui ne
///   désérialisent pas dans le struct typé → `-32602`).
#[derive(Debug)]
enum ToolFailure {
    Tool(ToolError),
    Protocol(i64, String),
}

/// Handler d'outil — historique ([`ToolFn`]) ou MRTR ([`MrtrToolFn`]).
#[derive(Clone)]
enum ToolHandler {
    Plain(Arc<ToolFn>),
    Mrtr(Arc<MrtrToolFn>),
}

#[derive(Clone)]
struct ToolDef {
    description: Option<String>,
    schema: Option<Arc<dyn Schema>>,
    output_schema: Option<Arc<dyn Schema>>,
    output_object_rooted: bool,
    annotations: Option<ToolAnnotations>,
    required_capabilities: Vec<String>,
    /// Droits exigés pour appeler cet outil — **tous** exigés (ET). Vide
    /// alors que l'autorisation est active : l'outil est refusé (échec
    /// fermé) et absent de `tools/list`.
    required_rights: Vec<String>,
    #[allow(dead_code)]
    mcp_params: Vec<McpParamDef>,
    handler: ToolHandler,
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

/// Handler de ressource — historique ([`ResourceFn`]) ou MRTR.
#[derive(Clone)]
enum ResourceHandler {
    Plain(Arc<ResourceFn>),
    Mrtr(Arc<MrtrResourceFn>),
}

/// Définition d'une ressource (clé : `uri`).
#[derive(Clone)]
struct ResourceDef {
    name: String,
    description: Option<String>,
    /// Droits exigés pour lire cette ressource (ET) — vide avec
    /// autorisation active : `resources/read` refusé, absente de
    /// `resources/list`.
    required_rights: Vec<String>,
    handler: ResourceHandler,
}

/// Handler de prompt — historique ([`PromptFn`]) ou MRTR.
#[derive(Clone)]
enum PromptHandler {
    Plain(Arc<PromptFn>),
    Mrtr(Arc<MrtrPromptFn>),
}

/// Définition d'un prompt (clé : `name`).
#[derive(Clone)]
struct PromptDef {
    description: Option<String>,
    schema: Option<Arc<dyn Schema>>,
    /// Droits exigés pour rendre ce prompt (ET) — vide avec autorisation
    /// active : `prompts/get` refusé, absent de `prompts/list`.
    required_rights: Vec<String>,
    handler: PromptHandler,
}

/// Gabarit de ressource **et** le droit qu'il exige.
///
/// Les gabarits n'ont pas de handler de lecture (catalogue pur servi par
/// `resources/templates/list`) : leurs droits ne portent donc que sur leur
/// **présence dans la liste**.
#[derive(Clone)]
struct TemplateDef {
    template: ResourceTemplate,
    /// Droits exigés pour voir ce gabarit (ET).
    required_rights: Vec<String>,
}

/// En-têtes de routage MCP 2026-07-28 (SEP-2243).
///
/// `MCP-Protocol-Version`, `Mcp-Method` et `Mcp-Name` permettent à un gateway
/// ou à un reverse proxy de router et d'autoriser sans analyser le corps JSON.
/// Obligatoires sur le profil moderne [`ProtocolProfile::Strict2026_07_28`],
/// optionnels sur [`ProtocolProfile::Legacy`].
#[derive(Debug, Clone, Default)]
pub struct McpHeaders {
    /// Version MCP annoncée par l'en-tête `MCP-Protocol-Version`.
    pub protocol_version: Option<String>,
    /// Méthode MCP annoncée par l'en-tête `Mcp-Method`.
    pub method: Option<String>,
    /// Nom de l'outil ou du prompt, ou URI de la ressource annoncé(e) par
    /// l'en-tête `Mcp-Name`.
    pub name: Option<String>,
    /// Paramètres personnalisés `Mcp-Param-*` (suffixe en minuscules, valeur brute).
    pub params: Vec<(String, String)>,
}

impl McpHeaders {
    /// Crée un conteneur vide.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Extrait les en-têtes de routage SEP-2243 d'une liste brute
    /// `(nom, valeur)` — l'entrée du contrat C8
    /// ([`ServerCore::dispatch_verified`]).
    ///
    /// La première occurrence gagne (le transport intégré refuse les
    /// doublons en `400` avant d'arriver ici) ; `Mcp-Param-*` est reconnu
    /// sans tenir compte de la casse du suffixe, comme sur le transport.
    #[must_use]
    pub fn from_headers(headers: &[(&str, &str)]) -> Self {
        let mut result = Self::default();
        for (name, value) in headers {
            if name.eq_ignore_ascii_case("mcp-protocol-version") {
                if result.protocol_version.is_none() {
                    result.protocol_version = Some((*value).to_string());
                }
            } else if name.eq_ignore_ascii_case("mcp-method") {
                if result.method.is_none() {
                    result.method = Some((*value).to_string());
                }
            } else if name.eq_ignore_ascii_case("mcp-name") {
                if result.name.is_none() {
                    result.name = Some((*value).to_string());
                }
            } else if name.len() > 10
                && name
                    .as_bytes()
                    .get(..10)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"mcp-param-"))
            {
                let suffix = name
                    .get(10..)
                    .map(str::to_ascii_lowercase)
                    .unwrap_or_default();
                if !result.params.iter().any(|(key, _)| key == &suffix) {
                    result.params.push((suffix, (*value).to_string()));
                }
            }
        }
        result
    }
}

/// Statut HTTP associé à une réponse de dispatch MCP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HttpStatus {
    Ok,
    BadRequest,
    NotFound,
    InternalServerError,
    /// Refus d'autorisation (`403`) — droit manquant ou confirmation
    /// refusée/en attente, code JSON-RPC [`ErrorObject::FORBIDDEN`].
    Forbidden,
}

impl HttpStatus {
    /// Ligne de statut HTTP canonique (ex: `"200 OK"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "200 OK",
            Self::BadRequest => "400 Bad Request",
            Self::NotFound => "404 Not Found",
            Self::InternalServerError => "500 Internal Server Error",
            Self::Forbidden => "403 Forbidden",
        }
    }

    /// Code numérique HTTP canonique (ex: `200`).
    #[must_use]
    pub const fn code(self) -> u16 {
        match self {
            Self::Ok => 200,
            Self::BadRequest => 400,
            Self::NotFound => 404,
            Self::InternalServerError => 500,
            Self::Forbidden => 403,
        }
    }

    /// Dérive le statut HTTP approprié depuis un code d'erreur JSON-RPC.
    #[must_use]
    pub fn from_error_code(code: i64) -> Self {
        match code {
            ErrorObject::METHOD_NOT_FOUND => Self::NotFound,
            ErrorObject::FORBIDDEN => Self::Forbidden,
            ErrorObject::CLIENT_CAPABILITY_REQUIRED
            | ErrorObject::HEADER_MISMATCH
            | ErrorObject::UNSUPPORTED_PROTOCOL_VERSION => Self::BadRequest,
            _ => Self::Ok,
        }
    }
}

/// Résultat structuré d'un dispatch (corps JSON-RPC sérialisé + statut HTTP).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DispatchResponse {
    /// Corps JSON-RPC (réponse succès ou erreur).
    pub body: Vec<u8>,
    /// Statut HTTP associé.
    pub status: HttpStatus,
}

/// Profil de conformité protocolaire du serveur.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProtocolProfile {
    /// Profil strict MCP 2026-07-28 (par défaut) :
    /// - `_meta` avec `protocolVersion` et `clientCapabilities` obligatoires ;
    /// - `id: null` rejeté (-32600, HTTP 400) ;
    /// - Méthodes supprimées (`initialize`, `ping`) et inconnues refusées (-32601, HTTP 404).
    #[default]
    Strict2026_07_28,
    /// Profil legacy `2025-11-25` explicite :
    /// - Méthodes `initialize` et `ping` servies ; `initialize` négocie la
    ///   révision `2025-11-25` ([`protocol::LEGACY_PROTOCOL_VERSION`]) ;
    /// - Tolérance pour l'absence de `_meta` ;
    /// - Tolérance pour `id: null`.
    Legacy,
}

/// Hook d'audit d'une décision d'autorisation — voir
/// [`ServerBuilder::on_auth_decision`].
type AuthHook = dyn Fn(&auth::AuthDecisionEvent<'_>) + Send + Sync;

/// État d'autorisation d'un noyau — présent **seulement** si un
/// [`auth::Verifier`] a été configuré ([`ServerBuilder::verifier`]).
///
/// Sans lui, aucun chemin d'autorisation n'existe : ni filtrage, ni refus,
/// ni allocation — le comportement est exactement celui d'avant le module
/// [`crate::auth`].
struct AuthGate {
    /// Vérificateur d'identité — appelé par le transport via
    /// [`ServerCore::authenticate`], jamais déduit du corps JSON-RPC.
    verifier: Arc<dyn auth::Verifier>,
    /// Confirmation externe ([`ServerBuilder::approval_policy`]).
    approval_policy: Option<Arc<dyn auth::ApprovalPolicy>>,
    /// Outils marqués « approbation requise »
    /// ([`ServerBuilder::require_approval`]).
    approval_required: BTreeSet<String>,
    /// Hooks d'audit, cumulables ([`ServerBuilder::on_auth_decision`]).
    decision_hooks: Vec<Arc<AuthHook>>,
}

/// Refus d'autorisation — motif **précis** (hook d'audit) et motif
/// **réseau** (`data.reason`, limité aux trois valeurs de la spécification).
struct AuthRefusal {
    /// Verdict publié au hook.
    verdict: auth::AuthVerdict,
    /// Motif précis, publié au seul hook (jamais au réseau).
    reason: &'static str,
    /// Motif rendu au client dans `error.data.reason`.
    wire_reason: &'static str,
    /// Délai suggéré pour `approval_pending`.
    retry_after: Option<Duration>,
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
    /// Gabarits de ressources (clé : `uriTemplate`) — servis par
    /// `resources/templates/list`, avec le droit exigé pour les voir.
    templates: Arc<BTreeMap<String, TemplateDef>>,
    /// Fournisseur de complétion — `None` : `completion/complete` répond
    /// `-32602` et la capacité `completions` n'est pas annoncée.
    completions: Option<Arc<CompletionFn>>,
    prompts: Arc<BTreeMap<String, PromptDef>>,
    /// Délai maximal d'exécution des handlers (tools, resources, prompts) —
    /// `None` : illimité (comportement historique).
    tool_timeout: Option<Duration>,
    /// Plafond de threads de handlers simultanés (en cours ou détachés après
    /// un timeout).
    max_inflight_handlers: usize,
    /// Compteur de threads de handlers simultanés.
    inflight_handlers: Arc<std::sync::atomic::AtomicUsize>,
    /// Cache de fils de handlers réutilisés ([`HandlerPool`]) : un appel borné
    /// par `tool_timeout` ne paie plus la création d'un fil système.
    handler_pool: Arc<HandlerPool>,
    /// Plafond d'octets d'une réponse JSON-RPC sérialisée (`0` : illimité).
    max_response_bytes: usize,
    /// Hooks d'observation ([`ServerBuilder::on_call`]) — appelés après le
    /// traitement complet de chaque message.
    hooks: Arc<Vec<Arc<CallHook>>>,
    /// Autorisation — `None` : aucune vérification, aucun filtrage (le
    /// comportement historique). `Some` : [`ServerBuilder::verifier`] a été
    /// appelé, tous les chemins d'autorisation sont actifs.
    auth_gate: Option<Arc<AuthGate>>,
    /// Profil protocolaire (strict MCP 2026-07-28 ou legacy).
    protocol_profile: ProtocolProfile,
    /// Clé HMAC de signature des `requestState` MRTR — récoltée au build
    /// (process-local) ou fournie via [`ServerBuilder::request_state_key`].
    request_state_key: Arc<Vec<u8>>,
    /// Durée de vie des `requestState` (TTL — spec : échéance courte).
    request_state_ttl: Duration,
    /// Catalogues pré-sérialisés — immuables après construction (les registres
    /// ne bougent plus), servis **tels quels** : `tools/list`, `resources/list`,
    /// `prompts/list`, `resources/templates/list` ne resérialisent ni ne
    /// clonent leur arbre JSON à chaque requête, et le chemin filtré ne
    /// réassemble que les éléments visibles. `initialize` a une entrée par
    /// révision legacy servie.
    tools_list_cache: std::sync::OnceLock<Arc<Catalogue>>,
    resources_list_cache: std::sync::OnceLock<Arc<Catalogue>>,
    templates_list_cache: std::sync::OnceLock<Arc<Catalogue>>,
    prompts_list_cache: std::sync::OnceLock<Arc<Catalogue>>,
    discover_cache: std::sync::OnceLock<Box<[u8]>>,
    initialize_cache: std::sync::OnceLock<BTreeMap<&'static str, Value>>,
    ping_cache: std::sync::OnceLock<Value>,
}

#[derive(Serialize)]
struct CallToolResultRef<'a> {
    #[serde(rename = "resultType")]
    result_type: ResultType,
    content: &'a [ContentBlock],
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
    contents: &'a [ResourceData],
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

/// Charge utile signée d'un jeton `requestState` MRTR — le plus petit état
/// stateless compatible avec la spec : version du format, méthode liée,
/// digest des paramètres saillants, identité de l'appelant quand
/// l'autorisation est active, tour, échéance UNIX et charge opaque du
/// handler. Signé (HMAC-SHA256), **pas chiffré** — n'y mettre aucun secret.
///
/// L'`id` JSON-RPC n'y figure pas : la spec exige un id différent au rejeu.
#[derive(Serialize)]
struct MrtrStatePayload<'a> {
    /// Version du format — toute autre valeur est refusée à la lecture.
    v: u8,
    /// Méthode d'origine — un rejeu sur une autre méthode est refusé.
    method: &'a str,
    /// Digest des paramètres saillants d'origine (nom/arguments/uri).
    digest: &'a str,
    /// Identité authentifiée de l'émetteur — présente uniquement quand
    /// l'autorisation est active, et comparée à l'identité courante à la
    /// reprise : un jeton émis pour A ne sert pas à B.
    #[serde(skip_serializing_if = "Option::is_none")]
    sub: Option<&'a str>,
    /// Tour auquel le jeton a été émis — le rejeu reprend à `turn + 1`.
    turn: u32,
    /// Échéance UNIX (secondes) — `now >= exp` ⇒ refus (TTL court, spec).
    exp: u64,
    /// Charge opaque du handler ([`InputRequiredSpec::with_state`]) —
    /// absente si le handler n'en porte pas.
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<&'a Value>,
}

/// Résultat fil `input_required` (MRTR) — `inputRequests` est omis quand le
/// handler n'enporte aucune requête (état seul).
#[derive(Serialize)]
struct InputRequiredResultRef<'a> {
    #[serde(rename = "resultType")]
    result_type: ResultType,
    #[serde(rename = "inputRequests", skip_serializing_if = "Option::is_none")]
    input_requests: Option<&'a serde_json::Map<String, Value>>,
    #[serde(rename = "requestState")]
    request_state: &'a str,
    #[serde(rename = "_meta")]
    meta: &'a Value,
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
    /// En-têtes de requête déclarés par l'utilisateur pour le préflight
    /// CORS ([`Self::allow_preflight_header`]) — s'ajoutent, au bind, à la
    /// liste calculée automatiquement (base, en-têtes de signature du
    /// vérificateur, paramètres promus des outils).
    allow_preflight_headers: Vec<String>,
    /// Hôtes acceptés dans l'en-tête `Host` — vide : règle locale par
    /// défaut, voir [`Self::allowed_hosts`].
    allowed_hosts: Vec<String>,
    read_timeout: Duration,
    /// Plafond explicite de l'échéance globale d'écriture — `None` : défaut
    /// dérivé de `read_timeout`, voir [`Self::write_deadline`].
    write_deadline: Option<Duration>,
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
    /// Capacité client requise par outil, en attente d'association.
    required_capabilities: BTreeMap<String, Vec<String>>,
    /// Droits exigés par outil, en attente d'association — même mécanique
    /// indépendante de l'ordre que [`Self::annotate`] : `.require_right()`
    /// avant ou après `.tool()` donne le même résultat.
    tool_rights: BTreeMap<String, Vec<String>>,
    /// Droits exigés par ressource (clé : `uri`).
    resource_rights: BTreeMap<String, Vec<String>>,
    /// Droits exigés par prompt (clé : nom).
    prompt_rights: BTreeMap<String, Vec<String>>,
    /// Outils marqués « confirmation requise » ([`Self::require_approval`]).
    approval_required: BTreeSet<String>,
    /// Vérificateur d'identité ([`Self::verifier`]) — son **absence** laisse
    /// l'autorisation inactive et le comportement du crate inchangé.
    verifier: Option<Arc<dyn auth::Verifier>>,
    /// Confirmation externe ([`Self::approval_policy`]).
    approval_policy: Option<Arc<dyn auth::ApprovalPolicy>>,
    /// Hooks d'audit des décisions ([`Self::on_auth_decision`]), cumulables.
    auth_hooks: Vec<Arc<AuthHook>>,
    /// Configuration TLS 1.3 ([`Self::tls`], feature `tls`) — `None` :
    /// socket en clair, comportement historique.
    #[cfg(feature = "tls")]
    tls: Option<crate::tls::TlsConfig>,
    resources: BTreeMap<String, ResourceDef>,
    /// Gabarits de ressources (clé : `uriTemplate`) — catalogue pur :
    /// `resources/templates/list`, pas de handler de lecture.
    resource_templates: BTreeMap<String, ResourceTemplate>,
    /// Fournisseur de complétion posé par [`Self::completions`] — le
    /// dernier enregistré gagne.
    completions: Option<Arc<CompletionFn>>,
    prompts: BTreeMap<String, PromptDef>,
    /// Hooks d'observation enregistrés par [`Self::on_call`].
    hooks: Vec<Arc<CallHook>>,
    /// Hooks de requête enregistrés par [`Self::on_request`] — verdict
    /// d'accès (auth, rate-limit) avant lecture du corps.
    request_hooks: Vec<Arc<RequestHook>>,
    /// Plafond d'octets d'une réponse JSON-RPC sérialisée (`0` : illimité).
    max_response_bytes: usize,
    /// Profil protocolaire (strict MCP 2026-07-28 ou legacy).
    protocol_profile: ProtocolProfile,
    /// Clé HMAC de signature des `requestState` MRTR — fournie via
    /// [`Self::request_state_key`] (flotte) ou **tirée paresseusement** au
    /// premier `core()`/`bind()` (mono-instance) : construire un builder ne
    /// consomme aucune entropie tant qu'aucun noyau n'existe.
    request_state_key: Arc<std::sync::OnceLock<Arc<Vec<u8>>>>,
    /// Durée de vie des `requestState` (défaut [`DEFAULT_REQUEST_STATE_TTL`]).
    request_state_ttl: Duration,
    /// Annonce au MCP Control (voir [`Self::announce`]).
    announce: bool,
    /// Module anti-zombie (voir [`Self::anti_zombie`]) : sweep du registre
    /// au bind, drain borné et bilan de fin de vie.
    anti_zombie: bool,
    /// Durée maximale du drain de fin de vie (voir [`Self::drain_timeout`]).
    drain_timeout: Duration,
    /// Compteur d'inflight **injecté** — `None` (défaut) : chaque `core()` /
    /// `bind()` crée le sien (serveurs indépendants). Le
    /// [`Gateway`] en injecte un seul : tous ses
    /// snapshots partagent CE compteur, si bien que les handlers détachés
    /// d'un ancien snapshot restent comptés dans `MAX_INFLIGHT_HANDLERS`
    /// après un `edit` (le clonage du builder propage l'injection — un
    /// même gateway ne peut pas en perdre le fil).
    inflight_handlers: Option<Arc<std::sync::atomic::AtomicUsize>>,
}
