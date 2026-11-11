//! Noyau de dispatch JSON-RPC + builder — indépendant du transport.

use crate::error::{ErrorCode, ToolError};
use crate::http::{RequestContext, RequestHook, RequestRejection};
use crate::jsonrpc::{ErrorObject, Outgoing, RequestId};
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
use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    /// méthode/paramètres/id) et répond `resultType: "input_required"`.
    InputRequired(InputRequiredSpec),
}

type ToolFn = dyn Fn(&Value) -> Result<ToolOutcome, ToolFailure> + Send + Sync;
type ResourceFn = dyn Fn() -> Result<ResourceData, String> + Send + Sync;
type PromptFn = dyn Fn(&Value) -> Result<Vec<PromptMessage>, String> + Send + Sync;
/// Handler d'outil MRTR — reçoit les arguments **et** le contexte du tour
/// ([`InputRound`]) : réponses du client (non fiables) et état vérifié.
type MrtrToolFn = dyn Fn(&Value, &InputRound) -> Result<ToolOutcome, ToolError> + Send + Sync;
/// Handler de ressource MRTR — `resources/read` supporte le MRTR (spec).
type MrtrResourceFn = dyn Fn(&InputRound) -> Result<ResourceOutcome, String> + Send + Sync;
/// Handler de prompt MRTR — `prompts/get` supporte le MRTR (spec).
type MrtrPromptFn = dyn Fn(&Value, &InputRound) -> Result<PromptOutcome, String> + Send + Sync;
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
/// par le noyau (HMAC + TTL + liaison méthode/paramètres/id) — jamais dans
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
type CompletionFn = dyn Fn(&CompletionRequest) -> Result<Completion, String> + Send + Sync;

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
            let result = std::panic::catch_unwind(AssertUnwindSafe(f));
            // Le slot est rendu **avant** la remise du résultat : quand le
            // thread de connexion observe `Done`, le compteur est déjà
            // décrémenté. Sans cet ordre, le bilan anti-zombie lu juste
            // après le drain pouvait compter vivant un handler qui avait
            // terminé (la remise du résultat précède la fin du thread).
            drop(guard);
            let _ = tx.send(result);
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

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpParamType {
    String,
    Integer,
    Boolean,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
struct McpParamDef {
    property_name: String,
    header_suffix: String,
    param_type: McpParamType,
}

fn extract_mcp_param_defs(schema: &Value) -> Result<Vec<McpParamDef>, String> {
    let mut defs = Vec::new();
    let Some(obj) = schema.as_object() else {
        return Ok(defs);
    };
    let Some(props) = obj.get("properties").and_then(Value::as_object) else {
        return Ok(defs);
    };
    for (prop_name, prop_val) in props {
        if let Some(header_val) = prop_val.get("x-mcp-header") {
            let header_str = header_val
                .as_str()
                .ok_or_else(|| format!("property `{prop_name}`: x-mcp-header must be a string"))?;
            if header_str.is_empty() {
                return Err(format!(
                    "property `{prop_name}`: x-mcp-header cannot be empty"
                ));
            }
            if !header_str
                .bytes()
                .all(|b| b.is_ascii() && b != b' ' && b != b':')
            {
                return Err(format!(
                    "property `{prop_name}`: x-mcp-header `{header_str}` must be valid ASCII without spaces or colons"
                ));
            }
            let type_str = prop_val.get("type").and_then(Value::as_str).unwrap_or("");
            let param_type = match type_str {
                "string" => McpParamType::String,
                "integer" => McpParamType::Integer,
                "boolean" => McpParamType::Boolean,
                other => {
                    return Err(format!(
                        "property `{prop_name}` with x-mcp-header has unsupported type `{other}` (only string, integer, boolean allowed, number is forbidden)"
                    ));
                }
            };
            let lower_suffix = header_str.to_ascii_lowercase();
            if defs
                .iter()
                .any(|d: &McpParamDef| d.header_suffix == lower_suffix)
            {
                return Err(format!(
                    "duplicate x-mcp-header `{header_str}` (case-insensitive) in tool schema"
                ));
            }
            defs.push(McpParamDef {
                property_name: prop_name.clone(),
                header_suffix: lower_suffix,
                param_type,
            });
        }
    }
    Ok(defs)
}

#[allow(dead_code)]
fn decode_base64(input: &str) -> Result<Vec<u8>, &'static str> {
    if input.is_empty() {
        return Ok(Vec::new());
    }
    let bytes = input.as_bytes();
    if bytes.len() % 4 != 0 {
        return Err("base64 length must be a multiple of 4");
    }
    let mut out = Vec::with_capacity((bytes.len() / 4) * 3);
    for chunk in bytes.chunks_exact(4) {
        let mut b = [0u8; 4];
        let mut pad_count = 0;
        for i in 0..4 {
            let c = chunk[i];
            if c == b'=' {
                pad_count += 1;
                if i < 2 {
                    return Err("invalid padding position");
                }
            } else if pad_count > 0 {
                return Err("character after padding");
            } else {
                b[i] = match c {
                    b'A'..=b'Z' => c - b'A',
                    b'a'..=b'z' => c - b'a' + 26,
                    b'0'..=b'9' => c - b'0' + 52,
                    b'+' => 62,
                    b'/' => 63,
                    _ => return Err("invalid base64 character"),
                };
            }
        }
        if pad_count == 0 {
            out.push((b[0] << 2) | (b[1] >> 4));
            out.push((b[1] << 4) | (b[2] >> 2));
            out.push((b[2] << 6) | b[3]);
        } else if pad_count == 1 {
            out.push((b[0] << 2) | (b[1] >> 4));
            out.push((b[1] << 4) | (b[2] >> 2));
        } else if pad_count == 2 {
            out.push((b[0] << 2) | (b[1] >> 4));
        } else {
            return Err("too many padding characters");
        }
    }
    Ok(out)
}

#[allow(dead_code)]
fn decode_mcp_header_value(raw: &str) -> Result<String, &'static str> {
    let raw_trimmed = raw.trim();
    if raw_trimmed.len() >= 11
        && raw_trimmed[..9].eq_ignore_ascii_case("=?base64?")
        && raw_trimmed.ends_with("?=")
    {
        let encoded = &raw_trimmed[9..raw_trimmed.len() - 2];
        let bytes = decode_base64(encoded)?;
        String::from_utf8(bytes).map_err(|_| "invalid utf-8 in base64 header value")
    } else {
        Ok(raw_trimmed.to_string())
    }
}

/// Un `inputSchema` d'outil MCP est nécessairement un **objet** JSON : un
/// schéma racine booléen (`true`/`false` — valide en JSON Schema pour dire
/// « tout passe / rien ne passe ») n'est pas un `inputSchema` MCP valide et
/// est refusé à l'enregistrement (erreur de programmation, même convention
/// que `from_json_schema`). Les schémas booléens restent acceptés partout
/// où JSON Schema les autorise — sous-schémas (`properties.*`, `items`…),
/// `output_schema`, documents bruts — seul l'usage comme inputSchema racine
/// d'un outil est refusé.
fn require_object_input_schema(tool: &str, schema: &dyn crate::zod::Schema) {
    assert!(
        schema.json().is_object(),
        "litemcp/server : l'inputSchema de l'outil `{tool}` doit être un objet JSON — \
         un schéma booléen racine (`true`/`false`) n'est pas un inputSchema MCP valide ; \
         les schémas booléens restent autorisés comme sous-schémas"
    );
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
    handler: PromptHandler,
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
}

/// Statut HTTP associé à une réponse de dispatch MCP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpStatus {
    Ok,
    BadRequest,
    NotFound,
    InternalServerError,
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
        }
    }

    /// Dérive le statut HTTP approprié depuis un code d'erreur JSON-RPC.
    #[must_use]
    pub fn from_error_code(code: i64) -> Self {
        match code {
            ErrorObject::METHOD_NOT_FOUND => Self::NotFound,
            ErrorObject::CLIENT_CAPABILITY_REQUIRED
            | ErrorObject::HEADER_MISMATCH
            | ErrorObject::UNSUPPORTED_PROTOCOL_VERSION => Self::BadRequest,
            _ => Self::Ok,
        }
    }
}

/// Résultat structuré d'un dispatch (corps JSON-RPC sérialisé + statut HTTP).
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// - Méthodes `initialize` et `ping` servies ;
    /// - Tolérance pour l'absence de `_meta` ;
    /// - Tolérance pour `id: null`.
    Legacy,
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
    /// `resources/templates/list`.
    templates: Arc<BTreeMap<String, ResourceTemplate>>,
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
    /// Plafond d'octets d'une réponse JSON-RPC sérialisée (`0` : illimité).
    max_response_bytes: usize,
    /// Hooks d'observation ([`ServerBuilder::on_call`]) — appelés après le
    /// traitement complet de chaque message.
    hooks: Arc<Vec<Arc<CallHook>>>,
    /// Profil protocolaire (strict MCP 2026-07-28 ou legacy).
    protocol_profile: ProtocolProfile,
    /// Clé HMAC de signature des `requestState` MRTR — récoltée au build
    /// (process-local) ou fournie via [`ServerBuilder::request_state_key`].
    request_state_key: Arc<Vec<u8>>,
    /// Durée de vie des `requestState` (TTL — spec : échéance courte).
    request_state_ttl: Duration,
    /// Catalogues sérialisés — immuables après construction (les registres
    /// ne bougent plus), servis **tels quels** : `tools/list`,
    /// `resources/list`, `prompts/list`, `server/discover`, `initialize` et
    /// `ping` ne resérialisent ni ne clonent leur arbre JSON à chaque requête.
    tools_list_cache: std::sync::OnceLock<Value>,
    resources_list_cache: std::sync::OnceLock<Value>,
    templates_list_cache: std::sync::OnceLock<Value>,
    prompts_list_cache: std::sync::OnceLock<Value>,
    discover_cache: std::sync::OnceLock<Value>,
    initialize_cache: std::sync::OnceLock<Value>,
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
/// digest des paramètres saillants, tour, échéance UNIX et charge opaque du
/// handler. Signé (HMAC-SHA256), **pas chiffré** — n'y mettre aucun secret.
#[derive(Serialize)]
struct MrtrStatePayload<'a> {
    /// Version du format — toute autre valeur est refusée à la lecture.
    v: u8,
    /// Méthode d'origine — un rejeu sur une autre méthode est refusé.
    method: &'a str,
    /// Digest des paramètres saillants d'origine (nom/arguments/uri).
    digest: &'a str,
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
    /// Préserve une API de compatibilité pour les utilisateurs directs de `dispatch`
    /// en tolérant l'absence de `_meta` lorsque non fourni.
    pub fn dispatch_with_headers(
        &self,
        body: &[u8],
        headers: Option<&McpHeaders>,
    ) -> Option<Vec<u8>> {
        self.dispatch_internal(body, headers, false).map(|r| r.body)
    }

    /// Traite un corps de requête JSON-RPC et produit une réponse structurée
    /// contenant le corps et le statut HTTP associé.
    ///
    /// Renvoie `None` pour une notification (aucune réponse attendue —
    /// le transport répondra `202 Accepted` sans corps).
    pub fn dispatch_request(
        &self,
        body: &[u8],
        headers: Option<&McpHeaders>,
    ) -> Option<DispatchResponse> {
        self.dispatch_internal(
            body,
            headers,
            self.protocol_profile == ProtocolProfile::Strict2026_07_28,
        )
    }

    fn dispatch_internal(
        &self,
        body: &[u8],
        headers: Option<&McpHeaders>,
        strict_meta: bool,
    ) -> Option<DispatchResponse> {
        let incoming = match crate::jsonrpc::parse_incoming(body) {
            Ok(incoming) => incoming,
            Err(crate::jsonrpc::ParseFail::Syntax) => {
                return Some(DispatchResponse {
                    body: Outgoing::parse_error().to_vec(),
                    status: HttpStatus::BadRequest,
                });
            }
            Err(fail) => {
                // L'id est repris quand il a pu être lu (JSON-RPC 2.0 §5).
                let id = fail.id().cloned();
                let message = match fail {
                    crate::jsonrpc::ParseFail::Syntax => String::new(),
                    crate::jsonrpc::ParseFail::Invalid { message, .. } => message,
                };
                return Some(DispatchResponse {
                    body: Outgoing::invalid_request(id, format!("invalid request: {message}"))
                        .to_vec(),
                    status: HttpStatus::BadRequest,
                });
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

        // Cohérence des en-têtes de routage MCP (SEP-2243) : `MCP-Protocol-Version`,
        // `Mcp-Method`, `Mcp-Name` et `Mcp-Param-*`.
        let mut header_mismatch: Option<ErrorObject> = None;
        let is_modern = strict_meta;

        // 1. Validation de MCP-Protocol-Version
        let body_meta_version = params
            .get("_meta")
            .and_then(|m| m.get(protocol::META_PROTOCOL_VERSION))
            .and_then(Value::as_str);

        if is_modern {
            match headers.and_then(|h| h.protocol_version.as_deref()) {
                None | Some("") => {
                    header_mismatch = Some(ErrorObject::with_data(
                        ErrorObject::HEADER_MISMATCH,
                        "MCP-Protocol-Version header is required in modern MCP profile",
                        json!({}),
                    ));
                }
                Some(ver) => {
                    let ver = ver.trim();
                    if let Some(body_ver) = body_meta_version {
                        if ver != body_ver {
                            header_mismatch = Some(ErrorObject::with_data(
                                ErrorObject::HEADER_MISMATCH,
                                "MCP-Protocol-Version header does not match the request body",
                                json!({
                                    "header": ver,
                                    "body": body_ver,
                                }),
                            ));
                        }
                    } else if !protocol::SUPPORTED_VERSIONS.contains(&ver) {
                        header_mismatch = Some(ErrorObject::with_data(
                            ErrorObject::HEADER_MISMATCH,
                            "MCP-Protocol-Version header specifies an unsupported protocol version",
                            json!({ "header": ver }),
                        ));
                    }
                }
            }
        } else if let Some(ver) = headers.and_then(|h| h.protocol_version.as_deref()) {
            let ver = ver.trim();
            if let Some(body_ver) = body_meta_version {
                if ver != body_ver {
                    header_mismatch = Some(ErrorObject::with_data(
                        ErrorObject::HEADER_MISMATCH,
                        "MCP-Protocol-Version header does not match the request body",
                        json!({
                            "header": ver,
                            "body": body_ver,
                        }),
                    ));
                }
            }
        }

        // 2. Validation de Mcp-Method
        if header_mismatch.is_none() {
            if is_modern {
                match headers.and_then(|h| h.method.as_deref()) {
                    None | Some("") => {
                        header_mismatch = Some(ErrorObject::with_data(
                            ErrorObject::HEADER_MISMATCH,
                            "Mcp-Method header is required in modern MCP profile",
                            json!({}),
                        ));
                    }
                    Some(m) => {
                        let m = m.trim();
                        if m != incoming.method {
                            header_mismatch = Some(ErrorObject::with_data(
                                ErrorObject::HEADER_MISMATCH,
                                "Mcp-Method header does not match the request body",
                                json!({
                                    "header": m,
                                    "body": incoming.method,
                                }),
                            ));
                        }
                    }
                }
            } else if let Some(m) = headers.and_then(|h| h.method.as_deref()) {
                let m = m.trim();
                if m.is_empty() || m != incoming.method {
                    header_mismatch = Some(ErrorObject::with_data(
                        ErrorObject::HEADER_MISMATCH,
                        "Mcp-Method header does not match the request body",
                        json!({
                            "header": m,
                            "body": incoming.method,
                        }),
                    ));
                }
            }
        }

        // 3. Validation de Mcp-Name
        let (expects_name, body_name) = match incoming.method.as_str() {
            protocol::method::TOOLS_CALL | protocol::method::PROMPTS_GET => (
                true,
                params.get("name").and_then(Value::as_str).unwrap_or(""),
            ),
            protocol::method::RESOURCES_READ => (
                true,
                params.get("uri").and_then(Value::as_str).unwrap_or(""),
            ),
            _ => (
                false,
                params
                    .get("name")
                    .or_else(|| params.get("uri"))
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            ),
        };

        if header_mismatch.is_none() {
            if is_modern && expects_name {
                match headers.and_then(|h| h.name.as_deref()) {
                    None | Some("") => {
                        header_mismatch = Some(ErrorObject::with_data(
                            ErrorObject::HEADER_MISMATCH,
                            "Mcp-Name header is required for this method in modern MCP profile",
                            json!({ "method": incoming.method }),
                        ));
                    }
                    Some(raw_name) => match decode_mcp_header_value(raw_name) {
                        Err(err) => {
                            header_mismatch = Some(ErrorObject::with_data(
                                ErrorObject::HEADER_MISMATCH,
                                format!("Malformed Mcp-Name header: {err}"),
                                json!({ "header": raw_name }),
                            ));
                        }
                        Ok(decoded) => {
                            if body_name.is_empty() || decoded != body_name {
                                header_mismatch = Some(ErrorObject::with_data(
                                    ErrorObject::HEADER_MISMATCH,
                                    "Mcp-Name header does not match the request body",
                                    json!({
                                        "header": decoded,
                                        "body": body_name,
                                    }),
                                ));
                            }
                        }
                    },
                }
            } else if let Some(raw_name) = headers.and_then(|h| h.name.as_deref()) {
                let trimmed = raw_name.trim();
                if trimmed.is_empty() {
                    header_mismatch = Some(ErrorObject::with_data(
                        ErrorObject::HEADER_MISMATCH,
                        "Mcp-Name header cannot be empty",
                        json!({ "header": raw_name }),
                    ));
                } else if !expects_name {
                    header_mismatch = Some(ErrorObject::with_data(
                        ErrorObject::HEADER_MISMATCH,
                        "Mcp-Name header is not permitted for this method",
                        json!({ "method": incoming.method }),
                    ));
                } else {
                    match decode_mcp_header_value(trimmed) {
                        Err(err) => {
                            header_mismatch = Some(ErrorObject::with_data(
                                ErrorObject::HEADER_MISMATCH,
                                format!("Malformed Mcp-Name header: {err}"),
                                json!({ "header": raw_name }),
                            ));
                        }
                        Ok(decoded) => {
                            if body_name.is_empty() || decoded != body_name {
                                header_mismatch = Some(ErrorObject::with_data(
                                    ErrorObject::HEADER_MISMATCH,
                                    "Mcp-Name header does not match the request body",
                                    json!({
                                        "header": decoded,
                                        "body": body_name,
                                    }),
                                ));
                            }
                        }
                    }
                }
            }
        }

        // 4. Validation des Mcp-Param-*
        if header_mismatch.is_none() {
            if incoming.method.as_str() == protocol::method::TOOLS_CALL {
                if let Some(tool) = self.tools.get(body_name) {
                    let empty_map = serde_json::Map::new();
                    let args_obj = params
                        .get("arguments")
                        .and_then(Value::as_object)
                        .unwrap_or(&empty_map);

                    for param_def in &tool.mcp_params {
                        let arg_val = args_obj.get(&param_def.property_name);
                        let arg_present = arg_val.is_some_and(|v| !v.is_null());
                        let found_header = headers.and_then(|h| {
                            h.params
                                .iter()
                                .find(|(k, _)| k == &param_def.header_suffix)
                                .map(|(_, v)| v.as_str())
                        });

                        match (arg_present, found_header) {
                            (false, Some(_)) => {
                                header_mismatch = Some(ErrorObject::with_data(
                                    ErrorObject::HEADER_MISMATCH,
                                    format!(
                                        "Mcp-Param-{} header must be absent when argument `{}` is absent or null",
                                        param_def.header_suffix, param_def.property_name
                                    ),
                                    json!({ "param": param_def.property_name }),
                                ));
                                break;
                            }
                            (true, None) => {
                                header_mismatch = Some(ErrorObject::with_data(
                                    ErrorObject::HEADER_MISMATCH,
                                    format!(
                                        "Mcp-Param-{} header is required when argument `{}` is present",
                                        param_def.header_suffix, param_def.property_name
                                    ),
                                    json!({ "param": param_def.property_name }),
                                ));
                                break;
                            }
                            (true, Some(raw_val)) => match decode_mcp_header_value(raw_val) {
                                Err(err) => {
                                    header_mismatch = Some(ErrorObject::with_data(
                                        ErrorObject::HEADER_MISMATCH,
                                        format!(
                                            "Malformed Mcp-Param-{} header: {err}",
                                            param_def.header_suffix
                                        ),
                                        json!({ "header": raw_val }),
                                    ));
                                    break;
                                }
                                Ok(decoded) => {
                                    let val = arg_val.unwrap();
                                    let matched = match param_def.param_type {
                                        McpParamType::String => val.as_str() == Some(&decoded),
                                        McpParamType::Integer => {
                                            if let Ok(parsed_int) = decoded.trim().parse::<i64>() {
                                                if let Some(arg_i) = val.as_i64() {
                                                    arg_i == parsed_int
                                                } else if let Some(arg_f) = val.as_f64() {
                                                    arg_f.fract() == 0.0
                                                        && arg_f == parsed_int as f64
                                                } else {
                                                    false
                                                }
                                            } else {
                                                false
                                            }
                                        }
                                        McpParamType::Boolean => match decoded.trim() {
                                            "true" => val.as_bool() == Some(true),
                                            "false" => val.as_bool() == Some(false),
                                            _ => false,
                                        },
                                    };
                                    if !matched {
                                        header_mismatch = Some(ErrorObject::with_data(
                                            ErrorObject::HEADER_MISMATCH,
                                            format!(
                                                "Mcp-Param-{} header value does not match argument `{}`",
                                                param_def.header_suffix, param_def.property_name
                                            ),
                                            json!({
                                                "header": decoded,
                                                "body": val,
                                            }),
                                        ));
                                        break;
                                    }
                                }
                            },
                            (false, None) => {}
                        }
                    }

                    if header_mismatch.is_none() {
                        if let Some(headers) = headers {
                            for (k, _) in &headers.params {
                                if !tool.mcp_params.iter().any(|d| &d.header_suffix == k) {
                                    header_mismatch = Some(ErrorObject::with_data(
                                        ErrorObject::HEADER_MISMATCH,
                                        format!("Unexpected Mcp-Param-{k} header"),
                                        json!({ "header": k }),
                                    ));
                                    break;
                                }
                            }
                        }
                    }
                } else if let Some(headers) = headers {
                    if !headers.params.is_empty() {
                        header_mismatch = Some(ErrorObject::with_data(
                            ErrorObject::HEADER_MISMATCH,
                            "Mcp-Param-* headers are unexpected for unknown tool",
                            json!({}),
                        ));
                    }
                }
            } else if let Some(headers) = headers {
                if !headers.params.is_empty() {
                    header_mismatch = Some(ErrorObject::with_data(
                        ErrorObject::HEADER_MISMATCH,
                        "Mcp-Param-* headers are only permitted on tools/call",
                        json!({ "method": incoming.method }),
                    ));
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
            return Some(self.protocol_failure(
                &id,
                &incoming,
                name,
                started,
                error,
                HttpStatus::BadRequest,
            ));
        }

        if incoming.is_notification() {
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
                    HttpStatus::BadRequest,
                ));
            }
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
                    HttpStatus::BadRequest,
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

        // Requête (non-notification) :
        // 1. Validation de l'id null en mode strict (rejet en -32600 + HTTP 400)
        if incoming.null_id && self.protocol_profile == ProtocolProfile::Strict2026_07_28 {
            return Some(self.protocol_failure(
                &Value::Null,
                &incoming,
                name,
                started,
                ErrorObject::new(
                    ErrorObject::INVALID_REQUEST,
                    "request id must be a string or a number (null is not permitted in modern MCP 2026-07-28)",
                ),
                HttpStatus::BadRequest,
            ));
        }

        // 2. Validation des métadonnées obligatoires en mode strict MCP 2026-07-28
        if strict_meta {
            if incoming.params.is_none() {
                return Some(self.protocol_failure(
                    &id,
                    &incoming,
                    name,
                    started,
                    ErrorObject::new(
                        ErrorObject::INVALID_PARAMS,
                        "params object is required and must contain `_meta`",
                    ),
                    HttpStatus::BadRequest,
                ));
            }
            let Some(meta) = params.get("_meta") else {
                return Some(self.protocol_failure(
                    &id,
                    &incoming,
                    name,
                    started,
                    ErrorObject::new(
                        ErrorObject::INVALID_PARAMS,
                        "missing required `_meta` object in params",
                    ),
                    HttpStatus::BadRequest,
                ));
            };
            let Some(meta_obj) = meta.as_object() else {
                return Some(self.protocol_failure(
                    &id,
                    &incoming,
                    name,
                    started,
                    ErrorObject::new(ErrorObject::INVALID_PARAMS, "`_meta` must be a JSON object"),
                    HttpStatus::BadRequest,
                ));
            };

            let Some(ver_val) = meta_obj.get(protocol::META_PROTOCOL_VERSION) else {
                return Some(self.protocol_failure(
                    &id,
                    &incoming,
                    name,
                    started,
                    ErrorObject::new(
                        ErrorObject::INVALID_PARAMS,
                        "missing required `io.modelcontextprotocol/protocolVersion` in `_meta`",
                    ),
                    HttpStatus::BadRequest,
                ));
            };
            let Some(version_str) = ver_val.as_str() else {
                return Some(self.protocol_failure(
                    &id,
                    &incoming,
                    name,
                    started,
                    ErrorObject::new(
                        ErrorObject::INVALID_PARAMS,
                        "`io.modelcontextprotocol/protocolVersion` must be a string",
                    ),
                    HttpStatus::BadRequest,
                ));
            };
            if !protocol::SUPPORTED_VERSIONS.contains(&version_str) {
                let safe = sanitize_echo(version_str);
                return Some(self.protocol_failure(
                    &id,
                    &incoming,
                    name,
                    started,
                    ErrorObject::with_data(
                        ErrorObject::UNSUPPORTED_PROTOCOL_VERSION,
                        format!("Unsupported protocol version: {safe}"),
                        json!({
                            "supported": protocol::SUPPORTED_VERSIONS,
                            "requested": safe,
                        }),
                    ),
                    HttpStatus::BadRequest,
                ));
            }

            let Some(caps_val) = meta_obj.get(protocol::META_CLIENT_CAPABILITIES) else {
                return Some(self.protocol_failure(
                    &id,
                    &incoming,
                    name,
                    started,
                    ErrorObject::new(
                        ErrorObject::INVALID_PARAMS,
                        "missing required `io.modelcontextprotocol/clientCapabilities` in `_meta`",
                    ),
                    HttpStatus::BadRequest,
                ));
            };
            if !caps_val.is_object() {
                return Some(self.protocol_failure(
                    &id,
                    &incoming,
                    name,
                    started,
                    ErrorObject::new(
                        ErrorObject::INVALID_PARAMS,
                        "`io.modelcontextprotocol/clientCapabilities` must be a JSON object",
                    ),
                    HttpStatus::BadRequest,
                ));
            }
        } else {
            // Mode legacy : check_protocol_version tolère l'absence mais refuse une version non supportée
            if let Some(err) = self.check_protocol_version(params) {
                return Some(self.protocol_failure(
                    &id,
                    &incoming,
                    name,
                    started,
                    err,
                    HttpStatus::BadRequest,
                ));
            }
        }

        let is_legacy = self.protocol_profile == ProtocolProfile::Legacy;
        // Cycle MRTR (multi round-trip, spec 2026-07-28) : `requestState` et
        // `inputResponses` sont vérifiés AVANT tout handler — uniquement sur
        // les trois méthodes supportées par la spec (`tools/call`,
        // `prompts/get`, `resources/read`) ; sur toute autre méthode, les
        // champs sont ignorés (le serveur ne répond jamais `input_required`
        // ailleurs et ne dépend d'aucun état de connexion). Un état qui
        // échoue à la vérification est refusé en `-32602` sans atteindre le
        // handler.
        let mrtr_round = if matches!(
            incoming.method.as_str(),
            protocol::method::TOOLS_CALL
                | protocol::method::RESOURCES_READ
                | protocol::method::PROMPTS_GET
        ) {
            match self.verify_mrtr(&id, &incoming.method, params) {
                Ok(round) => round,
                Err(error) => {
                    return Some(self.protocol_failure(
                        &id,
                        &incoming,
                        name,
                        started,
                        error,
                        HttpStatus::BadRequest,
                    ));
                }
            }
        } else {
            InputRound::first()
        };
        let (mut reply, mut status) = match incoming.method.as_str() {
            protocol::method::DISCOVER => (
                Outgoing::success_vec(&id, self.discover_json()),
                CallStatus::Success,
            ),
            protocol::method::INITIALIZE if is_legacy => (
                Outgoing::success_vec(&id, self.initialize_json()),
                CallStatus::Success,
            ),
            protocol::method::PING if is_legacy => (
                Outgoing::success_vec(&id, self.ping_json()),
                CallStatus::Success,
            ),
            protocol::method::TOOLS_LIST => (
                Outgoing::success_vec(&id, self.tools_list_json()),
                CallStatus::Success,
            ),
            protocol::method::TOOLS_CALL => self.call_tool(&id, params, &mrtr_round),
            protocol::method::RESOURCES_LIST => (
                Outgoing::success_vec(&id, self.resources_list_json()),
                CallStatus::Success,
            ),
            protocol::method::RESOURCES_TEMPLATES_LIST => (
                Outgoing::success_vec(&id, self.templates_list_json()),
                CallStatus::Success,
            ),
            protocol::method::RESOURCES_READ => self.read_resource(&id, params, &mrtr_round),
            protocol::method::PROMPTS_LIST => (
                Outgoing::success_vec(&id, self.prompts_list_json()),
                CallStatus::Success,
            ),
            protocol::method::PROMPTS_GET => self.get_prompt(&id, params, &mrtr_round),
            protocol::method::COMPLETION_COMPLETE => self.complete(&id, params),
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

        let http_status = match status {
            CallStatus::Success | CallStatus::ToolFailure(_) | CallStatus::Notification => {
                HttpStatus::Ok
            }
            CallStatus::ProtocolError(code) => HttpStatus::from_error_code(code),
        };

        Some(DispatchResponse {
            body: reply,
            status: http_status,
        })
    }

    /// Construit la réponse d'erreur de protocole **et** émet l'événement
    /// d'observation correspondant — les sorties anticipées de
    /// [`ServerCore::dispatch_request`] passent toutes ici pour que
    /// chaque message produise exactement un événement.
    fn protocol_failure(
        &self,
        id: &Value,
        incoming: &crate::jsonrpc::Incoming,
        name: Option<&str>,
        started: Instant,
        error: ErrorObject,
        http_status: HttpStatus,
    ) -> DispatchResponse {
        let code = error.code;
        let outgoing = Outgoing::failure(id.clone(), error);
        self.emit_call(
            &incoming.method,
            name,
            started.elapsed(),
            CallStatus::ProtocolError(code),
        );
        DispatchResponse {
            body: outgoing.to_vec(),
            status: http_status,
        }
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

    /// Vérifie le cycle MRTR d'une requête (`requestState` +
    /// `inputResponses`) **avant tout handler** — uniquement sur les trois
    /// méthodes supportées par la spec (`tools/call`, `prompts/get`,
    /// `resources/read`) ; toute autre méthode ignore ces champs (le noyau
    /// ne répond jamais `input_required` ailleurs et ne dépend d'aucun
    /// état de connexion).
    ///
    /// * Absence des deux champs → tour 1 ([`InputRound::first`]).
    /// * `inputResponses` sans `requestState` → `-32602` (rien n'est
    ///   vérifiable : refus net, le handler n'est pas atteint).
    /// * `requestState` présent → jeton vérifié (taille, structure
    ///   base64url, HMAC-SHA256, version, méthode liée, digest des
    ///   paramètres saillants, tour, échéance) ou refus `-32602` — un état
    ///   altéré, expiré, émis sous une autre clé ou lié à une autre requête
    ///   n'atteint jamais le handler.
    /// * `inputResponses` présent → structure validée (objet de
    ///   valeurs-objets ; les clés non reconnues sont ignorées, comme le
    ///   permet la spec), sinon `-32602`.
    ///
    /// Le jeton est **porteur** et **non consommé** : la consommation unique
    /// doit être imposée par le handler (aucun état serveur ici). L'`id`
    /// JSON-RPC est volontairement **non lié** (`_id` inutilisé) : la spec
    /// exige que le rejeu utilise un id différent.
    fn verify_mrtr(
        &self,
        _id: &Value,
        method: &str,
        params: &Value,
    ) -> Result<InputRound, ErrorObject> {
        let has_state = params.get(protocol::mrtr::REQUEST_STATE).is_some();
        let has_responses = params.get(protocol::mrtr::INPUT_RESPONSES).is_some();
        if !has_state && !has_responses {
            return Ok(InputRound::first());
        }
        if !has_state {
            return Err(ErrorObject::new(
                ErrorObject::INVALID_PARAMS,
                "params.requestState is required when params.inputResponses is present",
            ));
        }
        let Some(token) = params
            .get(protocol::mrtr::REQUEST_STATE)
            .and_then(Value::as_str)
        else {
            return Err(ErrorObject::new(
                ErrorObject::INVALID_PARAMS,
                "params.requestState must be a string",
            ));
        };
        if token.len() > MAX_REQUEST_STATE_CHARS {
            return Err(ErrorObject::new(
                ErrorObject::INVALID_PARAMS,
                format!(
                    "params.requestState exceeds the maximum size ({MAX_REQUEST_STATE_CHARS} characters)"
                ),
            ));
        }
        let payload = verify_token(&self.request_state_key, token)
            .map_err(|error| ErrorObject::new(ErrorObject::INVALID_PARAMS, error.message()))?;
        let Some(payload_obj) = payload.as_object() else {
            return Err(ErrorObject::new(
                ErrorObject::INVALID_PARAMS,
                "requestState payload is malformed",
            ));
        };
        let invalid = |detail: &'static str| ErrorObject::new(ErrorObject::INVALID_PARAMS, detail);
        if payload_obj.get("v").and_then(Value::as_u64) != Some(1) {
            return Err(invalid("unsupported requestState payload version"));
        }
        if payload_obj.get("method").and_then(Value::as_str) != Some(method) {
            return Err(invalid(
                "requestState is bound to another method — replay it on the original method",
            ));
        }
        let Some(bound_digest) = payload_obj.get("digest").and_then(Value::as_str) else {
            return Err(invalid(
                "requestState payload is missing the request digest",
            ));
        };
        if bound_digest != digest_salient(&salient_params(method, params)) {
            return Err(invalid(
                "requestState is bound to the original request — replay it with unchanged parameters",
            ));
        }
        let turn = match payload_obj.get("turn").and_then(Value::as_u64) {
            Some(turn) if (1..=u64::from(u32::MAX)).contains(&turn) => turn as u32,
            _ => return Err(invalid("requestState payload turn is invalid")),
        };
        let Some(exp) = payload_obj.get("exp").and_then(Value::as_u64) else {
            return Err(invalid("requestState payload expiry is invalid"));
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if now >= exp {
            return Err(invalid("requestState has expired (short TTL)"));
        }
        let input_responses = match params.get(protocol::mrtr::INPUT_RESPONSES) {
            None => None,
            Some(responses) => {
                let Some(map) = responses.as_object() else {
                    return Err(invalid("params.inputResponses must be an object"));
                };
                for (key, value) in map {
                    if !value.is_object() {
                        return Err(ErrorObject::new(
                            ErrorObject::INVALID_PARAMS,
                            format!(
                                "params.inputResponses[\"{}\"] must be an object",
                                sanitize_echo(key)
                            ),
                        ));
                    }
                }
                Some(responses.clone())
            }
        };
        Ok(InputRound {
            turn: turn.saturating_add(1),
            input_responses,
            state: payload_obj.get("state").cloned(),
        })
    }

    /// Émet un résultat `input_required` (MRTR) produit par un handler
    /// d'outil, de prompt ou de ressource.
    ///
    /// Valide la spécification (au moins une requête embarquée ou un état —
    /// sinon `-32603` : bogue de handler, rien ne part sur le fil), chaque
    /// requête embarquée (méthode embarquable + forme fil, sinon `-32603`)
    /// et les capacités client déclarées dans `_meta` de la requête
    /// courante (`elicitation/create` → `elicitation`,
    /// `sampling/createMessage` → `sampling`, `roots/list` → `roots` ;
    /// capacité absente → `-32021` avec la forme officielle
    /// `data.requiredCapabilities`). Frappe ensuite un `requestState`
    /// opaque signé (HMAC-SHA256, TTL, liaison méthode/paramètres
    /// saillants/tour) portant l'état du handler — **aucun état de
    /// connexion** n'est retenu : le rejeu peut atterrir sur toute instance
    /// partageant la clé.
    ///
    /// Forme fil : `{ resultType: "input_required", inputRequests?,
    /// requestState, _meta }` en HTTP 200.
    fn emit_input_required(
        &self,
        id: &Value,
        method: &str,
        params: &Value,
        spec: InputRequiredSpec,
        round: &InputRound,
    ) -> (Vec<u8>, CallStatus) {
        let internal = |message: String| {
            (
                Outgoing::failure(
                    id.clone(),
                    ErrorObject::new(ErrorObject::INTERNAL_ERROR, message),
                )
                .to_vec(),
                CallStatus::ProtocolError(ErrorObject::INTERNAL_ERROR),
            )
        };
        // 1. Une spécification vide est un bogue de handler (spec : au
        //    moins un des deux champs dans chaque résultat).
        if spec.input_requests.is_empty() && spec.state.is_none() {
            return internal(
                "handler produced an input_required result with neither inputRequests nor state"
                    .to_string(),
            );
        }
        // 2. Requêtes embarquées : forme fil + méthodes embarquables —
        //    une requête mal formée ne doit jamais atteindre le client.
        for (key, request) in &spec.input_requests {
            if key.is_empty() {
                return internal(
                    "handler produced an input_required result with an empty request key"
                        .to_string(),
                );
            }
            if let Err(detail) = validate_embedded_request(request) {
                return internal(format!(
                    "handler produced an invalid embedded request `{key}`: {detail}"
                ));
            }
        }
        // 3. Capacités client déclarées — la spec interdit d'embarquer une
        //    requête que le client n'a pas annoncée dans ses capacités.
        let client_caps = params
            .get("_meta")
            .and_then(|meta| meta.get(protocol::META_CLIENT_CAPABILITIES));
        let mut missing: BTreeMap<&str, Value> = BTreeMap::new();
        for request in spec.input_requests.values() {
            let embedded_method = request
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if let Some(cap) = protocol::embedded_request::client_capability(embedded_method) {
                if !client_caps.is_some_and(|caps| has_client_capability(caps, cap)) {
                    missing.insert(cap, json!({}));
                }
            }
        }
        if !missing.is_empty() {
            let list = missing.keys().copied().collect::<Vec<_>>().join(", ");
            return (
                Outgoing::failure(
                    id.clone(),
                    ErrorObject::with_data(
                        ErrorObject::CLIENT_CAPABILITY_REQUIRED,
                        format!("missing required client capabilities: {list}"),
                        json!({ "requiredCapabilities": missing }),
                    ),
                )
                .to_vec(),
                CallStatus::ProtocolError(ErrorObject::CLIENT_CAPABILITY_REQUIRED),
            );
        }
        // 4. Borne de taille de l'état opaque — un handler qui la dépasse
        //    est un bogue serveur (`-32603`), jamais une troncature.
        if let Some(state) = &spec.state {
            let size =
                serde_json::to_vec(state).map_or(MAX_STATE_PAYLOAD_BYTES + 1, |bytes| bytes.len());
            if size > MAX_STATE_PAYLOAD_BYTES {
                return internal(format!(
                    "handler state ({size} bytes) exceeds the maximum ({MAX_STATE_PAYLOAD_BYTES} bytes)"
                ));
            }
        }
        // 5. Jeton signé — le plus petit état stateless compatible : le
        //    jeton porte tout ce qu'il faut pour reprendre, rien ne vit en
        //    dehors de lui (ni session, ni connexion, ni process).
        let digest = digest_salient(&salient_params(method, params));
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let payload = MrtrStatePayload {
            v: 1,
            method,
            digest: &digest,
            turn: round.turn,
            exp: now.saturating_add(self.request_state_ttl.as_secs()),
            state: spec.state.as_ref(),
        };
        let token = mint_token(
            &self.request_state_key,
            &serde_json::to_value(&payload).expect("payload serialization is infallible"),
        );
        (
            Outgoing::success_vec(
                id,
                &InputRequiredResultRef {
                    result_type: ResultType::InputRequired,
                    input_requests: if spec.input_requests.is_empty() {
                        None
                    } else {
                        Some(&spec.input_requests)
                    },
                    request_state: &token,
                    meta: &self.meta,
                },
            ),
            CallStatus::Success,
        )
    }

    /// Identité du serveur sous `_meta` (SEP-2575 : les serveurs SHOULD
    /// s'identifier dans chaque résultat).
    fn meta(&self) -> Value {
        self.meta.clone()
    }

    /// Capacités annoncées par `server/discover` et `initialize` : les trois
    /// capacités (`tools`, `resources`, `prompts`) sont toujours annoncées car
    /// leurs méthodes sont servies même à registre vide (catalogue vide).
    ///
    /// `completions` fait exception : elle n'est annoncée que si un
    /// fournisseur est enregistré — sans lui, `completion/complete` répond
    /// `-32602`, la capacité ne doit pas prétendre le contraire.
    fn capabilities(&self) -> Value {
        let mut capabilities = json!({
            "tools": {},
            "resources": {},
            "prompts": {},
        });
        if self.completions.is_some() {
            capabilities["completions"] = json!({});
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

    /// `resources/templates/list` sérialisé, calculé une fois puis servi tel
    /// quel — comme les autres catalogues, même sans gabarit enregistré.
    fn templates_list_json(&self) -> &Value {
        self.templates_list_cache.get_or_init(|| {
            serde_json::to_value(self.list_resource_templates())
                .unwrap_or_else(|_| json!({ "resourceTemplates": [] }))
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

    fn list_resource_templates(&self) -> ResourceTemplatesListResult {
        ResourceTemplatesListResult {
            result_type: ResultType::Complete,
            ttl_ms: DEFAULT_LIST_TTL_MS,
            cache_scope: CacheScope::Private,
            // Ordre déterministe : le registre est un BTreeMap clé sur
            // l'uriTemplate.
            resource_templates: self.templates.values().cloned().collect(),
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

    /// Exécute un handler de ressource, borné par `tool_timeout` — le tour
    /// MRTR ([`InputRound`]) est remis au handler à chaque passage.
    fn read_resource(
        &self,
        id: &Value,
        params: &Value,
        round: &InputRound,
    ) -> (Vec<u8>, CallStatus) {
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
        let handler = def.handler.clone();
        let owned_round = round.clone();
        let contents = match self.run_bounded(move || match &handler {
            ResourceHandler::Plain(f) => f().map(ResourceOutcome::Data),
            ResourceHandler::Mrtr(f) => f(&owned_round),
        }) {
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
            Ok(ResourceOutcome::InputRequired(spec)) => {
                // MRTR sur `resources/read` (méthode supportée par la spec).
                self.emit_input_required(id, protocol::method::RESOURCES_READ, params, spec, round)
            }
            Ok(ResourceOutcome::Data(contents)) => (
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
                    ErrorObject::with_data(
                        HANDLER_FAILURE,
                        format!("Resource read failed: {message}"),
                        json!({ "reason": "handler_failed" }),
                    ),
                )
                .to_vec(),
                CallStatus::ProtocolError(HANDLER_FAILURE),
            ),
        }
    }

    /// Exécute un handler de prompt, borné par `tool_timeout` — le tour
    /// MRTR ([`InputRound`]) est remis au handler à chaque passage.
    fn get_prompt(&self, id: &Value, params: &Value, round: &InputRound) -> (Vec<u8>, CallStatus) {
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
        let mut prepared: Option<Value> = match &def.schema {
            Some(schema) if schema.has_defaults() || schema.has_integers() => {
                let mut value = arguments.clone();
                if schema.has_defaults() {
                    schema.apply_defaults(&mut value);
                }
                Some(value)
            }
            _ => None,
        };
        let arguments_for_validation = prepared.as_ref().unwrap_or(arguments);
        // Schéma exécuteur : hors schéma → -32602 avec les issues, le
        // handler ne tourne jamais (même contrat que `tool_typed`).
        if let Some(schema) = &def.schema {
            let mut issues = Issues::new();
            let mut path = Path::root("arguments");
            schema.validate(&mut path, arguments_for_validation, &mut issues);
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
        // **Normalisation des entiers intégraux** (même contrat que
        // `tools/call`) : après validation réussie uniquement.
        if let (Some(schema), Some(value)) = (&def.schema, prepared.as_mut()) {
            if schema.has_integers() {
                schema.normalize_integers(value);
            }
        }
        let arguments = prepared.as_ref().unwrap_or(arguments);
        let handler = def.handler.clone();
        let arguments = arguments.clone();
        let owned_round = round.clone();
        let messages = match self.run_bounded(move || match &handler {
            PromptHandler::Plain(f) => f(&arguments).map(PromptOutcome::Messages),
            PromptHandler::Mrtr(f) => f(&arguments, &owned_round),
        }) {
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
            Ok(PromptOutcome::InputRequired(spec)) => {
                // MRTR sur `prompts/get` (méthode supportée par la spec).
                self.emit_input_required(id, protocol::method::PROMPTS_GET, params, spec, round)
            }
            Ok(PromptOutcome::Messages(messages)) => (
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
                    ErrorObject::with_data(
                        HANDLER_FAILURE,
                        format!("Prompt failed: {message}"),
                        json!({ "reason": "handler_failed" }),
                    ),
                )
                .to_vec(),
                CallStatus::ProtocolError(HANDLER_FAILURE),
            ),
        }
    }

    /// Exécute le fournisseur de complétion, borné par `tool_timeout` —
    /// mêmes conventions que prompts/ressources : params mal formés ou
    /// fournisseur absent → `-32602`, échec du fournisseur → `-32603`
    /// (data `handler_failed`), timeout/saturation → erreur noyau `-32603`.
    fn complete(&self, id: &Value, params: &Value) -> (Vec<u8>, CallStatus) {
        let Some(provider) = &self.completions else {
            return (
                Outgoing::failure(
                    id.clone(),
                    ErrorObject::new(
                        ErrorObject::INVALID_PARAMS,
                        "completion is not supported: no completion provider is registered",
                    ),
                )
                .to_vec(),
                CallStatus::ProtocolError(ErrorObject::INVALID_PARAMS),
            );
        };
        let reference = match params.get("ref") {
            Some(Value::Object(r)) => match r.get("type").and_then(Value::as_str) {
                Some("ref/prompt") => match r.get("name").and_then(Value::as_str) {
                    Some(name) => CompleteReference::Prompt {
                        name: name.to_string(),
                        title: r.get("title").and_then(Value::as_str).map(str::to_string),
                    },
                    None => {
                        return self.complete_invalid_params(
                            id,
                            "params.ref.name is required for ref/prompt",
                        );
                    }
                },
                Some("ref/resource") => match r.get("uri").and_then(Value::as_str) {
                    Some(uri) => CompleteReference::Resource {
                        uri: uri.to_string(),
                    },
                    None => {
                        return self.complete_invalid_params(
                            id,
                            "params.ref.uri is required for ref/resource",
                        );
                    }
                },
                Some(other) => {
                    return self.complete_invalid_params(
                        id,
                        format!(
                            "unknown params.ref.type: {} — expected `ref/prompt` or `ref/resource`",
                            sanitize_echo(other)
                        ),
                    );
                }
                None => {
                    return self.complete_invalid_params(id, "params.ref.type is required");
                }
            },
            Some(_) => {
                return self.complete_invalid_params(id, "params.ref must be an object");
            }
            None => {
                return self.complete_invalid_params(id, "missing params.ref");
            }
        };
        let argument = match params.get("argument") {
            Some(Value::Object(a)) => {
                match (
                    a.get("name").and_then(Value::as_str),
                    a.get("value").and_then(Value::as_str),
                ) {
                    (Some(name), Some(value)) => CompletionArgument {
                        name: name.to_string(),
                        value: value.to_string(),
                    },
                    (Some(_), None) => {
                        return self.complete_invalid_params(
                            id,
                            "params.argument.value (string) is required",
                        );
                    }
                    (None, _) => {
                        return self.complete_invalid_params(
                            id,
                            "params.argument.name (string) is required",
                        );
                    }
                }
            }
            Some(_) => {
                return self.complete_invalid_params(id, "params.argument must be an object");
            }
            None => {
                return self.complete_invalid_params(id, "missing params.argument");
            }
        };
        // `context.arguments` : carte optionnelle de chaînes (valeurs
        // non-chaînes refusées — la spec attend un map de strings).
        let mut context = BTreeMap::new();
        if let Some(map) = params
            .get("context")
            .and_then(|c| c.get("arguments"))
            .and_then(Value::as_object)
        {
            for (key, value) in map {
                match value.as_str() {
                    Some(text) => {
                        context.insert(key.clone(), text.to_string());
                    }
                    None => {
                        return self.complete_invalid_params(
                            id,
                            format!(
                                "params.context.arguments values must be strings (offending key: {})",
                                sanitize_echo(key)
                            ),
                        );
                    }
                }
            }
        }
        let request = CompletionRequest {
            reference,
            argument,
            context,
        };
        let handler = Arc::clone(provider);
        let completion = match self.run_bounded(move || handler(&request)) {
            Ok(completion) => completion,
            Err(failure) => {
                let code = failure.code;
                return (
                    Outgoing::failure(id.clone(), failure).to_vec(),
                    CallStatus::ProtocolError(code),
                );
            }
        };
        let completion = match completion {
            Ok(completion) => completion,
            Err(message) => {
                return (
                    Outgoing::failure(
                        id.clone(),
                        ErrorObject::with_data(
                            HANDLER_FAILURE,
                            format!("Completion failed: {message}"),
                            json!({ "reason": "handler_failed" }),
                        ),
                    )
                    .to_vec(),
                    CallStatus::ProtocolError(HANDLER_FAILURE),
                );
            }
        };
        // Garde-fou spec : au plus 100 valeurs. Au-delà, troncature et
        // `hasMore: true` (sauf si le fournisseur l'a déjà posé).
        let mut completion = completion;
        if completion.values.len() > MAX_COMPLETION_VALUES {
            completion.values.truncate(MAX_COMPLETION_VALUES);
            if completion.has_more.is_none() {
                completion.has_more = Some(true);
            }
        }
        (
            Outgoing::success_vec(
                id,
                &CompleteResult {
                    result_type: ResultType::Complete,
                    completion,
                    meta: Some(self.meta.clone()),
                },
            ),
            CallStatus::Success,
        )
    }

    /// Réponse `-32602` de `completion/complete` — même convention que
    /// outils/ressources/prompts (cible ou forme de params invalide).
    fn complete_invalid_params(
        &self,
        id: &Value,
        message: impl Into<String>,
    ) -> (Vec<u8>, CallStatus) {
        (
            Outgoing::failure(
                id.clone(),
                ErrorObject::new(ErrorObject::INVALID_PARAMS, message),
            )
            .to_vec(),
            CallStatus::ProtocolError(ErrorObject::INVALID_PARAMS),
        )
    }

    /// Exécute `f` borné par [`Self::tool_timeout`][core-timeout] quand il
    /// est configuré ; traduit timeout/panique en erreur JSON-RPC.
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
                Outcome::TimedOut => Err(ErrorObject::with_data(
                    HANDLER_FAILURE,
                    format!("execution timed out after {} ms", timeout.as_millis()),
                    json!({ "reason": "timeout", "timeout_ms": timeout.as_millis() as u64 }),
                )),
                Outcome::Overloaded => Err(ErrorObject::with_data(
                    HANDLER_FAILURE,
                    "too many concurrent handlers in flight",
                    json!({ "reason": "overloaded", "max_in_flight": self.max_inflight_handlers }),
                )),
            },
        }
    }

    fn call_tool(&self, id: &Value, params: &Value, round: &InputRound) -> (Vec<u8>, CallStatus) {
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
        if !tool.required_capabilities.is_empty() {
            let client_caps = params
                .get("_meta")
                .and_then(|m| m.get(protocol::META_CLIENT_CAPABILITIES));
            for cap in &tool.required_capabilities {
                let satisfied = client_caps.is_some_and(|caps| has_client_capability(caps, cap));
                if !satisfied {
                    // SEP-2575 : `MissingRequiredClientCapabilityError` porte
                    // `error.data.requiredCapabilities` en **objet** indexé par
                    // la capacité manquante (ex. `{ "sampling": {} }`) — pas une
                    // liste de noms (le scénario officiel `server-stateless`
                    // valide cette forme exacte).
                    let mut required = serde_json::Map::new();
                    required.insert(cap.clone(), json!({}));
                    return (
                        Outgoing::failure(
                            id.clone(),
                            ErrorObject::with_data(
                                ErrorObject::CLIENT_CAPABILITY_REQUIRED,
                                format!("missing required client capability: {cap}"),
                                json!({ "requiredCapabilities": required }),
                            ),
                        )
                        .to_vec(),
                        CallStatus::ProtocolError(ErrorObject::CLIENT_CAPABILITY_REQUIRED),
                    );
                }
            }
        }
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
        // Préparation des arguments — **une seule copie** quand le schéma
        // porte des défauts à injecter ou des entiers à normaliser, zéro
        // copie sinon (chemin nominal). La normalisation des entiers
        // intégraux n'a lieu qu'après une validation réussie (tout intégral
        // présent est donc dans la plage exacte publiée) : le handler reçoit
        // des entiers exacts, `as_i64()` réussit toujours quand le schéma
        // passe.
        let mut prepared: Option<Value> = match &tool.schema {
            Some(schema) if schema.has_defaults() || schema.has_integers() => {
                let mut value = arguments.clone();
                if schema.has_defaults() {
                    schema.apply_defaults(&mut value);
                }
                Some(value)
            }
            _ => None,
        };
        let arguments_for_validation = prepared.as_ref().unwrap_or(arguments);
        if let Some(schema) = &tool.schema {
            let mut issues = Issues::new();
            let mut path = Path::root("arguments");
            schema.validate(&mut path, arguments_for_validation, &mut issues);
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
        // **Normalisation des entiers intégraux** — après une validation
        // réussie (tout intégral présent est donc dans la plage exacte
        // publiée) : `3.0`/`1e2` deviennent des entiers exacts, le handler
        // reçoit ce que `as_i64()` sait lire. Aucune conversion hors plage,
        // aucune valeur non entière touchée.
        if let (Some(schema), Some(value)) = (&tool.schema, prepared.as_mut()) {
            if schema.has_integers() {
                schema.normalize_integers(value);
            }
        }
        let arguments = prepared.as_ref().unwrap_or(arguments);
        // Exécution du handler, bornée par `tool_timeout` s'il est configuré.
        // En mode borné, les arguments sont clonés une fois (le handler
        // s'exécute dans un thread qui doit les posséder) — coût assumé,
        // uniquement sur ce chemin opt-in. Une panique y est rattrapée dans
        // le thread du handler et rendue en `-32603` (sans timeout, elle
        // remonte au `catch_unwind` du transport — même verdict).
        let execution = match self.tool_timeout {
            None => match std::panic::catch_unwind(AssertUnwindSafe(|| match &tool.handler {
                ToolHandler::Plain(f) => f(arguments),
                ToolHandler::Mrtr(f) => f(arguments, round).map_err(ToolFailure::Tool),
            })) {
                Ok(result) => result,
                Err(_) => Err(ToolFailure::Protocol(
                    ErrorObject::INTERNAL_ERROR,
                    "internal error (handler panicked)".to_string(),
                )),
            },
            Some(timeout) => {
                let owned = arguments.clone();
                let owned_round = round.clone();
                let handler = tool.handler.clone();
                match run_with_timeout(
                    timeout,
                    &self.inflight_handlers,
                    self.max_inflight_handlers,
                    move || match &handler {
                        ToolHandler::Plain(f) => f(&owned),
                        ToolHandler::Mrtr(f) => f(&owned, &owned_round).map_err(ToolFailure::Tool),
                    },
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
            Ok(ToolOutcome::InputRequired(spec)) => {
                // MRTR : le handler demande une entrée client — le noyau
                // valide les requêtes embarquées, frappe le `requestState`
                // signé et répond `input_required` (HTTP 200) ; aucun état
                // implicite n'est retenu de ce côté.
                return self.emit_input_required(
                    id,
                    protocol::method::TOOLS_CALL,
                    params,
                    spec,
                    round,
                );
            }
            Ok(ToolOutcome::Value(value)) => {
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
            Ok(ToolOutcome::Result(result)) => {
                // Chemin riche : le handler a construit le résultat complet
                // (contenus mixtes, `structuredContent` explicite). Deux
                // seuls ajustements noyau :
                // 1. l'`outputSchema` reste exécuteur sur un
                //    `structuredContent` explicite — validé **tel quel**
                //    (sans enveloppe : le handler a choisi la forme finale,
                //    l'envelopper la trahirait) ;
                // 2. un `isError: true` explicite est rapporté aux hooks
                //    comme échec d'exécution (classification inconnue du
                //    noyau : le handler n'a pas fourni de `ToolError`).
                if let (Some(output), Some(structured)) =
                    (&tool.output_schema, &result.structured_content)
                {
                    let mut issues = Issues::new();
                    let mut path = Path::root("output");
                    output.validate(&mut path, structured, &mut issues);
                    if issues.is_empty() {
                        (result, CallStatus::Success)
                    } else {
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
                } else if result.is_error == Some(true) {
                    (result, CallStatus::ToolFailure(ErrorCode::Unclassified))
                } else {
                    (result, CallStatus::Success)
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

/// Paramètres saillants liés au `requestState` MRTR — forme **canonique**
/// (indépendante de la présence/absence des champs optionnels) pour que le
/// digest du rejeu soit calculé sur exactement les mêmes octets qu'à
/// l'émission. `_meta` et les champs superflus n'y participent pas : les
/// capacités déclarées peuvent évoluer entre les tours, le contrat lie la
/// requête métier (méthode, nom/uri, arguments), pas son habillage.
fn salient_params(method: &str, params: &Value) -> Value {
    match method {
        protocol::method::TOOLS_CALL | protocol::method::PROMPTS_GET => json!({
            "method": method,
            "name": params.get("name").cloned().unwrap_or(Value::Null),
            "arguments": params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({})),
        }),
        protocol::method::RESOURCES_READ => json!({
            "method": method,
            "uri": params.get("uri").cloned().unwrap_or(Value::Null),
        }),
        _ => json!({ "method": method }),
    }
}

/// Validation de forme d'une requête embarquée produite par un handler —
/// l'émission d'une requête mal formée est un bogue serveur (`-32603`) :
/// elle ne doit jamais atteindre le fil (le client planterait dessus).
/// L'élicitation vérifie `message` (chaîne), `mode` (`"form"`/`"url"`,
/// défaut `"form"`), `requestedSchema` (objet, mode form) et `url`
/// (chaîne non vide, mode url) ; l'échantillonnage et les racines ne
/// vérifient que la forme objet (leurs champs applicatifs restent du
/// ressort du handler et du client).
fn validate_embedded_request(request: &Value) -> Result<(), String> {
    let Some(request) = request.as_object() else {
        return Err("must be an object".to_string());
    };
    let Some(method) = request.get("method").and_then(Value::as_str) else {
        return Err("`method` must be a string".to_string());
    };
    let Some(params) = request.get("params").and_then(Value::as_object) else {
        return Err("`params` must be an object".to_string());
    };
    match method {
        protocol::embedded_request::ELICIT_CREATE => {
            let mode = match params.get("mode") {
                None => "form",
                Some(Value::String(mode)) if mode == "form" => "form",
                Some(Value::String(mode)) if mode == "url" => "url",
                Some(_) => {
                    return Err("elicitation `mode` must be \"form\" or \"url\"".to_string());
                }
            };
            if !params.get("message").is_some_and(Value::is_string) {
                return Err("elicitation `message` must be a string".to_string());
            }
            match mode {
                "form" if !params.get("requestedSchema").is_some_and(Value::is_object) => {
                    Err("form elicitation requires an object `requestedSchema`".to_string())
                }
                "url"
                    if !params
                        .get("url")
                        .and_then(Value::as_str)
                        .is_some_and(|url| !url.is_empty()) =>
                {
                    Err("url elicitation requires a non-empty string `url`".to_string())
                }
                _ => Ok(()),
            }
        }
        protocol::embedded_request::SAMPLING_CREATE_MESSAGE
        | protocol::embedded_request::ROOTS_LIST => Ok(()),
        other => Err(format!("`{other}` is not an embeddable MCP method")),
    }
}

/// Vérifie si l'objet `clientCapabilities` déclare la capacité demandée.
///
/// Accepte les clés directes (`"sampling"`) ou les chemins pointés (`"experimental.feature"`).
fn has_client_capability(client_caps: &Value, cap: &str) -> bool {
    let Some(obj) = client_caps.as_object() else {
        return false;
    };
    if let Some(val) = obj.get(cap) {
        return !val.is_null() && val != false;
    }
    let mut current = client_caps;
    for part in cap.split('.') {
        if let Some(next) = current.get(part) {
            current = next;
        } else {
            return false;
        }
    }
    !current.is_null() && current != false
}

/// Nom d'outil/prompt ou `uri` de ressource d'une requête, pour
/// l'observation ([`CallEvent::name`]) — extraction best-effort depuis
/// `params`, `None` si la méthode n'en porte pas. Pour
/// `completion/complete`, la référence (`ref.name` ou `ref.uri`) sert de
/// nom.
fn requested_name<'a>(method: &str, params: &'a Value) -> Option<&'a str> {
    if method == protocol::method::COMPLETION_COMPLETE {
        return params
            .get("ref")
            .and_then(|r| r.get("name").or_else(|| r.get("uri")))
            .and_then(Value::as_str);
    }
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
    /// Capacité client requise par outil, en attente d'association.
    required_capabilities: BTreeMap<String, Vec<String>>,
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
    /// Clé HMAC de signature des `requestState` MRTR — récoltée au build
    /// (mono-instance) ou fournie via [`Self::request_state_key`] (flotte).
    request_state_key: Arc<Vec<u8>>,
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
            required_capabilities: BTreeMap::new(),
            resources: BTreeMap::new(),
            resource_templates: BTreeMap::new(),
            completions: None,
            prompts: BTreeMap::new(),
            connection_ttl: crate::http::CONNECTION_TTL,
            tool_timeout: None,
            hooks: Vec::new(),
            request_hooks: Vec::new(),
            max_response_bytes: MAX_RESPONSE_BYTES,
            protocol_profile: ProtocolProfile::Strict2026_07_28,
            request_state_key: Arc::new(harvest_key()),
            request_state_ttl: DEFAULT_REQUEST_STATE_TTL,
            announce: false,
            anti_zombie: false,
            drain_timeout: crate::antizombie::DRAIN_TIMEOUT,
            inflight_handlers: None,
        }
    }

    /// Injecte le compteur d'inflight partagé — réservé au
    /// [`Gateway`] : tous les noyaux d'un même
    /// gateway partagent CE compteur (un par gateway, jamais entre deux
    /// gateways ni avec un serveur indépendant, qui garde son compteur
    /// frais par `core()`/`bind()`).
    pub(crate) fn share_inflight_counter(
        mut self,
        counter: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Self {
        self.inflight_handlers = Some(counter);
        self
    }

    /// Compteur d'inflight pour le noyau en construction : celui injecté
    /// par le [`Gateway`] s'il existe, sinon un
    /// compteur frais (chaque noyau indépendant compte ses propres threads).
    fn inflight_counter(&self) -> Arc<std::sync::atomic::AtomicUsize> {
        self.inflight_handlers
            .clone()
            .unwrap_or_else(|| Arc::new(std::sync::atomic::AtomicUsize::new(0)))
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

    /// Définit le profil de protocole du serveur ([`ProtocolProfile::Strict2026_07_28`] par défaut).
    #[must_use]
    pub fn protocol_profile(mut self, profile: ProtocolProfile) -> Self {
        self.protocol_profile = profile;
        self
    }

    /// Active ou désactive le profil legacy 2025-11-25.
    #[must_use]
    pub fn legacy(mut self, enabled: bool) -> Self {
        self.protocol_profile = if enabled {
            ProtocolProfile::Legacy
        } else {
            ProtocolProfile::Strict2026_07_28
        };
        self
    }

    /// Exige qu'un client déclare la capacité `capability` dans
    /// `_meta["io.modelcontextprotocol/clientCapabilities"]` pour appeler
    /// l'outil `tool_name`.
    ///
    /// Si la capacité est absente ou non déclarée, l'appel est rejeté avec
    /// [`ErrorObject::CLIENT_CAPABILITY_REQUIRED`] (`-32021`) et HTTP 400.
    #[must_use]
    pub fn require_client_capability(mut self, tool_name: &str, capability: &str) -> Self {
        self.required_capabilities
            .entry(tool_name.to_string())
            .or_default()
            .push(capability.to_string());
        if let Some(tool) = self.tools.get_mut(tool_name) {
            if !tool.required_capabilities.contains(&capability.to_string()) {
                tool.required_capabilities.push(capability.to_string());
            }
        }
        self
    }

    /// Clé HMAC-SHA256 qui signe les `requestState` MRTR.
    ///
    /// Par défaut, une clé est récoltée au démarrage du builder : elle ne
    /// survit ni au redémarrage ni à une seconde instance. Un déploiement en
    /// flotte (gateways, répliques, reprises après redémarrage) doit fournir
    /// la **même** clé partout, sinon les jetons émis avant ne se vérifient
    /// plus (`-32602`) — c'est le seul mécanisme de reprise, aucun état
    /// serveur n'existe par ailleurs. Clé recommandée : ≥ 32 octets.
    ///
    /// # Panique
    ///
    /// Une clé vide est un bogue de configuration : panique explicite.
    ///
    /// ```no_run
    /// # use litemcp::prelude::*;
    /// let builder = ServerBuilder::new("mrtr", "1.0.0")
    ///     .request_state_key(b"une cle partagee par toute la flotte 32+");
    /// ```
    #[must_use]
    pub fn request_state_key(mut self, key: &[u8]) -> Self {
        assert!(
            !key.is_empty(),
            "request_state_key: la clé HMAC ne doit pas être vide"
        );
        self.request_state_key = Arc::new(key.to_vec());
        self
    }

    /// Durée de vie des `requestState` MRTR — un jeton présenté après son
    /// expiration est refusé en `-32602` (spec : « a short expiry (TTL) »).
    /// Défaut : 10 minutes.
    #[must_use]
    pub fn request_state_ttl(mut self, ttl: Duration) -> Self {
        self.request_state_ttl = ttl;
        self
    }

    /// Enregistre un outil **MRTR** (multi round-trip) : le handler reçoit
    /// [`ToolOutcome::InputRequired`] pour demander une entrée client et est
    /// **réappelé** au rejeu avec les `inputResponses` (non fiables) et
    /// l'état vérifié ([`InputRound`]).
    ///
    /// Le cycle est entièrement stateless : tout ce qui traverse les tours
    /// tient dans le `requestState` signé (HMAC-SHA256, TTL, liaison
    /// méthode/paramètres/id) que le client renvoie octet pour octet avec un
    /// **id JSON-RPC différent** — aucune dépendance à une connexion, une
    /// session ou un process ; le rejeu peut atterrir sur une autre instance
    /// partageant la clé. Les autres arguments (`tool`, `tool_typed`…) ne
    /// changent pas ; `Mcp-Param-*` et `require_client_capability` restent
    /// applicables avant l'enregistrement.
    pub fn tool_mrtr<F>(mut self, name: &str, description: &str, f: F) -> Self
    where
        F: Fn(&Value, &InputRound) -> Result<ToolOutcome, ToolError> + Send + Sync + 'static,
    {
        let (output_schema, annotations, required_capabilities) = self.take_tool_pending(name);
        let mut def = ToolDef {
            description: Some(description.to_string()),
            schema: None,
            output_schema,
            output_object_rooted: false,
            annotations,
            required_capabilities,
            mcp_params: Vec::new(),
            handler: ToolHandler::Mrtr(Arc::new(f)),
        };
        def.sync_output_shape();
        self.tools.insert(name.to_string(), def);
        self
    }

    /// Enregistre un prompt **MRTR** — même cycle que [`Self::tool_mrtr`]
    /// sur `prompts/get` (méthode supportée par la spec) : le handler rend
    /// [`PromptOutcome::Messages`] ou [`PromptOutcome::InputRequired`].
    pub fn prompt_mrtr<F>(mut self, name: &str, description: &str, f: F) -> Self
    where
        F: Fn(&Value, &InputRound) -> Result<PromptOutcome, String> + Send + Sync + 'static,
    {
        self.prompts.insert(
            name.to_string(),
            PromptDef {
                description: Some(description.to_string()),
                schema: None,
                handler: PromptHandler::Mrtr(Arc::new(f)),
            },
        );
        self
    }

    /// Enregistre une ressource **MRTR** — même cycle que [`Self::tool_mrtr`]
    /// sur `resources/read` (méthode supportée par la spec) : le handler
    /// rend [`ResourceOutcome::Data`] ou [`ResourceOutcome::InputRequired`].
    /// L'entrée apparaît dans `resources/list` comme les autres ressources.
    pub fn resource_mrtr<F>(mut self, uri: &str, name: &str, description: &str, f: F) -> Self
    where
        F: Fn(&InputRound) -> Result<ResourceOutcome, String> + Send + Sync + 'static,
    {
        self.resources.insert(
            uri.to_string(),
            ResourceDef {
                name: name.to_string(),
                description: Some(description.to_string()),
                handler: ResourceHandler::Mrtr(Arc::new(f)),
            },
        );
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
    /// [`RequestContext`] (méthode, chemin,
    /// query, adresse du pair, en-têtes) et rend `Ok(())` ou un
    /// [`RequestRejection`] (`401`, `403`,
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
    /// Par défaut, toute requête portant un en-tête `Origin` non listé ici
    /// est refusée (garde DNS-rebinding) — les clients CLI n'en envoient
    /// pas. Exception par défaut : sur un listener lié à une adresse de
    /// boucle spécifique (`127.0.0.0/8`, `::1`), une origine `http://` dont
    /// l'autorité est identique au `Host` reçu **et** au littéral `ip:port`
    /// du listener est acceptée (client navigateur même-origine) — toute
    /// divergence de schéma/hôte/port, `Origin: null` et
    /// `Sec-Fetch-Site: cross-site` restent refusés. Sur un bind non local,
    /// cette liste est la seule porte (déploiements explicites/proxys).
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
    /// pour un outil, erreur JSON-RPC `-32603` avec data `timeout` pour une
    /// ressource/prompt/complétion) et la requête est servie — le thread de
    /// connexion n'est plus retenu indéfiniment par du code utilisateur
    /// bloquant.
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
    /// La clé est l'`uri` ; le handler produit le contenu **texte** à chaque
    /// lecture (rien n'est mis en cache par la lib). Pour du **binaire**
    /// (`blob` base64), utilisez [`Self::resource_data`] — jamais un texte
    /// qui simulerait le binaire. Réenregistrer une `uri` existante remplace
    /// silencieusement la définition. `resources/list` annonce
    /// `uri`/`name`/`description` ; le `mimeType` vient du
    /// [`crate::protocol::ResourceContents`] rendu par le handler.
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
                handler: ResourceHandler::Plain(Arc::new(move || f().map(ResourceData::Text))),
            },
        );
        self
    }

    /// Enregistre une **ressource MCP** dont le handler choisit à chaque
    /// lecture entre contenu texte ([`ResourceData::text`]) et contenu
    /// **binaire** ([`ResourceData::binary`] / [`ResourceData::blob`] —
    /// `blob` base64, la forme `BlobResourceContents` de la spec).
    ///
    /// Même contrat que [`Self::resource`] (clé = `uri`, lecture à chaque
    /// appel, échec = `String`) ; l'entrée apparaît dans `resources/list`
    /// de la même façon. Réenregistrer une `uri` existante remplace la
    /// définition.
    ///
    /// ```no_run
    /// use litemcp::prelude::*;
    ///
    /// # fn main() -> std::io::Result<()> {
    /// ServerBuilder::new("assets", "1.0.0")
    ///     .resource_data("app://icon", "Icône", "Icône PNG de l'app", || {
    ///         Ok(ResourceData::binary(
    ///             "app://icon",
    ///             Some("image/png".into()),
    ///             &[0x89, 0x50, 0x4E, 0x47],
    ///         ))
    ///     })
    ///     .bind("127.0.0.1:23080")?
    ///     .serve()?;
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn resource_data<F>(mut self, uri: &str, name: &str, description: &str, f: F) -> Self
    where
        F: Fn() -> Result<ResourceData, String> + Send + Sync + 'static,
    {
        self.resources.insert(
            uri.to_string(),
            ResourceDef {
                name: name.to_string(),
                description: Some(description.to_string()),
                handler: ResourceHandler::Plain(Arc::new(f)),
            },
        );
        self
    }

    /// Annonce un **gabarit de ressource** (méthode
    /// `resources/templates/list`) — une URI template RFC 6570
    /// (`test://template/{id}/data`) décrit une famille de ressources.
    ///
    /// Le catalogue est un `BTreeMap` clé sur l'`uriTemplate` : réenregistrer
    /// un gabarit existant le remplace. La **lecture** reste celle de
    /// `resources/read` : le noyau matche des URIs exactes — la substitution
    /// de gabarit côté noyau n'est pas implémentée, un client qui lit une
    /// URI concrète doit trouver cette URI dans les ressources enregistrées
    /// ([`Self::resource`] / [`Self::resource_data`]).
    #[must_use]
    pub fn resource_template(mut self, uri_template: &str, name: &str, description: &str) -> Self {
        self.resource_templates.insert(
            uri_template.to_string(),
            ResourceTemplate {
                uri_template: uri_template.to_string(),
                name: name.to_string(),
                title: None,
                description: Some(description.to_string()),
                mime_type: None,
            },
        );
        self
    }

    /// Retire un **gabarit de ressource** du registre — le pendant retrait
    /// de [`Self::resource_template`] pour les gateways dynamiques.
    /// Retourne `true` si le gabarit existait.
    pub fn remove_resource_template(&mut self, uri_template: &str) -> bool {
        self.resource_templates.remove(uri_template).is_some()
    }

    /// Enregistre le **fournisseur de complétion** (`completion/complete`) :
    /// suggestions d'arguments pour les prompts (`ref/prompt`) et les
    /// gabarits de ressources (`ref/resource`).
    ///
    /// Le fournisseur reçoit une [`CompletionRequest`] validée (référence,
    /// argument en cours, variables déjà résolues) et rend au plus 100
    /// suggestions ([`Completion`] — la borne de la spec : au-delà, le
    /// noyau tronque et pose `hasMore: true`). Un réenregistrement remplace
    /// le fournisseur précédent. La capacité `completions` n'est annoncée
    /// (`server/discover`) que si un fournisseur est enregistré — une
    /// capacité annoncée est une capacité servie.
    ///
    /// ```no_run
    /// use litemcp::prelude::*;
    ///
    /// # fn main() -> std::io::Result<()> {
    /// ServerBuilder::new("calc", "1.0.0")
    ///     .completions(|request: &CompletionRequest| {
    ///         match &request.reference {
    ///             CompleteReference::Prompt { name, .. } if name == "saluer" => {
    ///                 Ok(Completion::new(vec!["Bonjour".into(), "Salut".into()]))
    ///             }
    ///             _ => Ok(Completion::new(vec![])),
    ///         }
    ///     })
    ///     .bind("127.0.0.1:23080")?
    ///     .serve()?;
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn completions<F>(mut self, f: F) -> Self
    where
        F: Fn(&CompletionRequest) -> Result<Completion, String> + Send + Sync + 'static,
    {
        self.completions = Some(Arc::new(f));
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
                handler: PromptHandler::Plain(Arc::new(f)),
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
                handler: PromptHandler::Plain(Arc::new(f)),
            },
        );
        self
    }

    /// Enchaîne un enregistrement généré par une macro (`#[tool]`,
    /// `#[resource]`, `#[prompt]` — macros incluses par défaut) :
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
    fn take_tool_pending(
        &mut self,
        name: &str,
    ) -> (
        Option<Arc<dyn Schema>>,
        Option<ToolAnnotations>,
        Vec<String>,
    ) {
        let previous_output = self
            .tools
            .get(name)
            .and_then(|def| def.output_schema.clone());
        let previous_annotations = self.tools.get(name).and_then(|def| def.annotations.clone());
        let previous_caps = self
            .tools
            .get(name)
            .map(|def| def.required_capabilities.clone())
            .unwrap_or_default();
        let output_schema = self.output_schemas.remove(name).or(previous_output);
        let annotations = self.annotations.remove(name).or(previous_annotations);
        let mut required_capabilities = self.required_capabilities.remove(name).unwrap_or_default();
        for cap in previous_caps {
            if !required_capabilities.contains(&cap) {
                required_capabilities.push(cap);
            }
        }
        (output_schema, annotations, required_capabilities)
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
        self.required_capabilities.remove(name);
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
        let wrapped =
            move |args: &Value| f(args).map(ToolOutcome::Value).map_err(ToolFailure::Tool);
        let (output_schema, annotations, required_capabilities) = self.take_tool_pending(name);
        let mut def = ToolDef {
            description: Some(description.to_string()),
            schema: None,
            output_schema,
            output_object_rooted: false,
            annotations,
            required_capabilities,
            mcp_params: Vec::new(),
            handler: ToolHandler::Plain(Arc::new(wrapped)),
        };
        def.sync_output_shape();
        self.tools.insert(name.to_string(), def);
        self
    }

    /// Enregistre un outil dont le handler produit **le résultat d'appel
    /// complet** — le chemin des contenus riches MCP : texte, image, audio,
    /// `resource_link`, ressource embarquée, en nombre et mélange libres
    /// ([`CallToolResult::from_content`]), avec `structuredContent`
    /// explicite si besoin.
    ///
    /// Le chemin historique ([`Self::tool`], [`ToolResult`]) reste inchangé
    /// : la valeur devient `structuredContent` avec l'enveloppe habituelle.
    /// Ici, au contraire, la sortie part **telle que construite** — seuls
    /// l'`outputSchema` (validé sur un `structuredContent` explicite) et le
    /// `_meta` d'identité serveur (SEP-2575) sont posés par le noyau.
    ///
    /// ```no_run
    /// use litemcp::prelude::*;
    ///
    /// # fn main() -> std::io::Result<()> {
    /// ServerBuilder::new("ecran", "1.0.0")
    ///     .tool_result("screenshot", "Capture d'écran", |_args| {
    ///         Ok(CallToolResult::from_content([
    ///             ContentBlock::text("Voici la capture :"),
    ///             ContentBlock::image_bytes(&[0x89, 0x50, 0x4E, 0x47], "image/png"),
    ///         ]))
    ///     })
    ///     .bind("127.0.0.1:23080")?
    ///     .serve()?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Réenregistrer un nom existant remplace l'outil précédent — les
    /// annotations et l'`outputSchema` déjà déclarés sont conservés, comme
    /// pour [`Self::tool`].
    pub fn tool_result<F>(mut self, name: &str, description: &str, f: F) -> Self
    where
        F: Fn(&Value) -> Result<CallToolResult, ToolError> + Send + Sync + 'static,
    {
        let wrapped =
            move |args: &Value| f(args).map(ToolOutcome::Result).map_err(ToolFailure::Tool);
        let (output_schema, annotations, required_capabilities) = self.take_tool_pending(name);
        let mut def = ToolDef {
            description: Some(description.to_string()),
            schema: None,
            output_schema,
            output_object_rooted: false,
            annotations,
            required_capabilities,
            mcp_params: Vec::new(),
            handler: ToolHandler::Plain(Arc::new(wrapped)),
        };
        def.sync_output_shape();
        self.tools.insert(name.to_string(), def);
        self
    }
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
        let wrapped =
            move |args: &Value| f(args).map(ToolOutcome::Value).map_err(ToolFailure::Tool);
        let schema_arc = Arc::new(schema);
        require_object_input_schema(name, schema_arc.as_ref());
        let mcp_params = extract_mcp_param_defs(&schema_arc.json())
            .expect("invalid x-mcp-header in tool schema");
        let (output_schema, annotations, required_capabilities) = self.take_tool_pending(name);
        let mut def = ToolDef {
            description: Some(description.to_string()),
            schema: Some(schema_arc),
            output_schema,
            output_object_rooted: false,
            annotations,
            required_capabilities,
            mcp_params,
            handler: ToolHandler::Plain(Arc::new(wrapped)),
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
                Ok(parsed) => f(parsed).map(ToolOutcome::Value).map_err(ToolFailure::Tool),
                Err(error) => Err(ToolFailure::Protocol(
                    ErrorObject::INVALID_PARAMS,
                    format!("arguments do not match the expected type: {error}"),
                )),
            },
        });
        let schema_arc = Arc::new(schema);
        require_object_input_schema(name, schema_arc.as_ref());
        let mcp_params = extract_mcp_param_defs(&schema_arc.json())
            .expect("invalid x-mcp-header in tool schema");
        let (output_schema, annotations, required_capabilities) = self.take_tool_pending(name);
        let mut def = ToolDef {
            description: Some(description.to_string()),
            schema: Some(schema_arc),
            output_schema,
            output_object_rooted: false,
            annotations,
            required_capabilities,
            mcp_params,
            handler: ToolHandler::Plain(handler),
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
            if let Some(caps) = self.required_capabilities.get(name) {
                for cap in caps {
                    if !def.required_capabilities.contains(cap) {
                        def.required_capabilities.push(cap.clone());
                    }
                }
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
            templates: Arc::new(self.resource_templates.clone()),
            completions: self.completions.clone(),
            prompts: Arc::new(self.prompts.clone()),
            tool_timeout: self.tool_timeout,
            max_inflight_handlers: MAX_INFLIGHT_HANDLERS,
            inflight_handlers: self.inflight_counter(),
            max_response_bytes: self.max_response_bytes,
            hooks: Arc::new(self.hooks.clone()),
            protocol_profile: self.protocol_profile,
            request_state_key: Arc::clone(&self.request_state_key),
            request_state_ttl: self.request_state_ttl,
            tools_list_cache: std::sync::OnceLock::new(),
            resources_list_cache: std::sync::OnceLock::new(),
            templates_list_cache: std::sync::OnceLock::new(),
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
        // Identité d'instance : générée pour TOUT serveur lié (exposée par
        // `/health`), publiée dans l'annonce quand `.announce()` est actif.
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
        let instance_id = crate::registry::generate_instance_id(port);
        // Annonce MCP Control : le listener est lié — l'adresse effective
        // **connectable** est publiée (`addr`) avec l'identité (`instance`) :
        // le sweep juge l'adresse publiée (un bind multi-interface est
        // publié par son bouclage, un bind spécifique tel quel), jamais un
        // bouclage figé, et une identité vérifiable prouve quelle instance
        // occupe le port. Un échec d'écriture est silencieux (commodité
        // d'intégration, jamais une condition).
        let mut announce_name = None;
        if self.announce {
            if let Some(mut addr) = listener.local_addr().ok().filter(|a| a.port() != 0) {
                addr.set_ip(match addr.ip() {
                    std::net::IpAddr::V4(ip) if ip.is_unspecified() => {
                        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
                    }
                    std::net::IpAddr::V6(ip) if ip.is_unspecified() => {
                        std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
                    }
                    other => other,
                });
                crate::registry::announce_at(&self.name, &self.version, addr, instance_id);
                announce_name = Some((self.name.clone(), addr.port()));
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
        // Compteur hissé AVANT les déplacements partiels de `self` ci-dessous.
        let inflight_handlers = self.inflight_counter();
        let core = Arc::new(ServerCore {
            name: self.name,
            version: self.version,
            meta,
            tools: Arc::new(tools),
            resources: Arc::new(self.resources),
            templates: Arc::new(self.resource_templates),
            completions: self.completions,
            prompts: Arc::new(self.prompts),
            tool_timeout: self.tool_timeout,
            max_inflight_handlers: MAX_INFLIGHT_HANDLERS,
            inflight_handlers,
            max_response_bytes: self.max_response_bytes,
            hooks: Arc::new(self.hooks),
            protocol_profile: self.protocol_profile,
            request_state_key: Arc::clone(&self.request_state_key),
            request_state_ttl: self.request_state_ttl,
            tools_list_cache: std::sync::OnceLock::new(),
            resources_list_cache: std::sync::OnceLock::new(),
            templates_list_cache: std::sync::OnceLock::new(),
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
            instance_id,
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

    /// Un handler qui a terminé ne retient plus son slot quand
    /// `run_with_timeout` rend `Done` : le bilan anti-zombie lu juste après
    /// le drain ne peut plus compter vivant un handler qui y a terminé.
    #[test]
    fn un_handler_termine_ne_reste_pas_compte_apres_la_remise_du_resultat() {
        let inflight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for _ in 0..512 {
            let outcome = run_with_timeout(Duration::from_secs(5), &inflight, 4, || 7u8);
            assert!(matches!(outcome, Outcome::Done(7)));
            assert_eq!(
                inflight.load(Ordering::Acquire),
                0,
                "`Done` doit impliquer un slot déjà rendu (lecture post-drain fiable)"
            );
        }
    }
}

#[cfg(test)]
mod tests_compteur_inflight_2026_11_09 {
    use super::*;
    use crate::gateway::Gateway;

    /// Identité du compteur d'inflight — le contrat d'isolation du
    /// partage : UN compteur par gateway (tous ses snapshots), un compteur
    /// propre par noyau indépendant. Sans partage accidentel entre deux
    /// gateways ni entre deux serveurs indépendants.

    #[test]
    fn deux_noyaux_independants_deux_compteurs() {
        let builder = ServerBuilder::new("iso", "1.0.0");
        let c1 = builder.core();
        let c2 = builder.core();
        assert!(
            !Arc::ptr_eq(&c1.inflight_handlers, &c2.inflight_handlers),
            "deux `core()` du même builder doivent rester indépendants"
        );
    }

    #[test]
    fn builder_clone_serveurs_independants_compteurs_independants() {
        let b1 = ServerBuilder::new("s1", "1.0.0");
        let b2 = b1.clone();
        let c1 = b1.core();
        let c2 = b2.core();
        assert!(
            !Arc::ptr_eq(&c1.inflight_handlers, &c2.inflight_handlers),
            "cloner un builder pour deux serveurs ne doit pas partager le compteur"
        );
    }

    #[test]
    fn deux_gateways_deux_compteurs() {
        let g1 = Gateway::new(ServerBuilder::new("g1", "1.0.0"));
        let g2 = Gateway::new(ServerBuilder::new("g2", "1.0.0"));
        assert!(
            !Arc::ptr_eq(
                &g1.snapshot().inflight_handlers,
                &g2.snapshot().inflight_handlers
            ),
            "deux gateways ne doivent jamais partager leur compteur"
        );
    }

    #[test]
    fn un_gateway_un_compteur_partage_tous_ses_snapshots() {
        let gw = Gateway::new(ServerBuilder::new("g", "1.0.0").tool("t", "d", |_a| Ok(json!({}))));
        let initial = gw.snapshot();
        gw.edit(|b| b.tool("t2", "d", |_a| Ok(json!({}))));
        let apres_edit1 = gw.snapshot();
        gw.edit(|b| b);
        let apres_edit2 = gw.snapshot();
        assert!(
            Arc::ptr_eq(&initial.inflight_handlers, &apres_edit1.inflight_handlers),
            "l'edit doit conserver le compteur du snapshot initial"
        );
        assert!(
            Arc::ptr_eq(&initial.inflight_handlers, &apres_edit2.inflight_handlers),
            "chaque edit doit partager le MÊME compteur (anciens détachés comptés)"
        );
    }
}

#[cfg(test)]
mod tests_codes_erreurs_2026_11_10 {
    use super::*;

    /// La surcharge (`run_bounded` → `Outcome::Overloaded`) répond `-32603`
    /// avec `data` structuré — plus la plage legacy `-32000` (MCP 2026-07-28
    /// déconseille d'introduire de nouveaux codes dedans). Le compteur est
    /// forcé au plafond : le handler n'est jamais exécuté, aucun thread
    /// spawné — le test reste instantané.
    #[test]
    fn surcharge_ressource_repond_32603_avec_data() {
        let core = ServerBuilder::new("sat", "1.0.0")
            .tool_timeout(Duration::from_millis(50))
            .resource("mem://x", "X", "jamais exécuté", || {
                unreachable!("la surcharge doit court-circuiter le handler")
            })
            .core();
        core.inflight_handlers.store(
            core.max_inflight_handlers,
            std::sync::atomic::Ordering::Release,
        );
        let reply = core
            .dispatch(
                br#"{"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":"mem://x"}}"#,
            )
            .expect("resources/read doit répondre");
        let v: Value = serde_json::from_slice(&reply).unwrap();
        assert_eq!(v["error"]["code"], -32603, "{v}");
        assert_eq!(v["error"]["data"]["reason"], "overloaded", "{v}");
        assert_eq!(
            v["error"]["data"]["max_in_flight"], MAX_INFLIGHT_HANDLERS as u64,
            "{v}"
        );
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("too many concurrent handlers in flight"),
            "{v}"
        );
    }
}
