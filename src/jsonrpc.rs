//! JSON-RPC 2.0 — le sous-ensemble utilisé par MCP.
//!
//! Parsing conforme : la version `jsonrpc` doit être `"2.0"`, un `id` absent
//! désigne une notification alors qu'un `id: null` **explicite** est une
//! requête à réponse obligatoire (avec `id: null`), et un lot (tableau) est
//! refusé en `-32600` (non supporté par MCP, qui n'envoie jamais de batch).
//!
//! **L'`id` est toujours préservé.** Il est extrait *best-effort* **avant**
//! toute validation du reste du message : une requête invalide reçoit une
//! réponse d'erreur portant **le même `id`** (JSON-RPC 2.0 §5 : *« The
//! Response object MUST contain the same id »*). Seul un `id` lui-même
//! illisible (booléen, tableau, objet) reste `null`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Identifiant de requête JSON-RPC : nombre ou chaîne.
///
/// Choix assumés :
/// - un nombre à partie fractionnaire (`1.5`) est refusé en `-32600` — la
///   spec JSON-RPC déconseille les ids fractionnaires (« SHOULD NOT ») et
///   MCP ne les emploie jamais ;
/// - un entier au-delà de `i64::MAX` est accepté via [`RequestId::Unsigned`]
///   plutôt que rejeté : JSON-RPC autorise tout `Number`, et refuser
///   `u64::MAX` écartait des identifiants légitimes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// Identifiant numérique signé.
    Number(i64),
    /// Identifiant numérique non signé (au-delà de `i64::MAX`).
    Unsigned(u64),
    /// Identifiant textuel.
    String(String),
}

impl RequestId {
    /// Rendu JSON de l'identifiant (pour le champ `id` d'une réponse).
    #[must_use]
    pub fn to_value(&self) -> Value {
        match self {
            Self::Number(n) => Value::from(*n),
            Self::Unsigned(n) => Value::from(*n),
            Self::String(s) => Value::from(s.as_str()),
        }
    }
}

/// Objet d'erreur JSON-RPC.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ErrorObject {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    /// Version de protocole non supportée (MCP 2026-07-28, SEP-2575).
    pub const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;
    /// Incohérence entre les en-têtes `Mcp-Method`/`Mcp-Name` et le corps de
    /// la requête (MCP 2026-07-28, SEP-2243).
    pub const HEADER_MISMATCH: i64 = -32020;

    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// Erreur avec `data` structuré (ex. liste de problèmes de validation).
    pub fn with_data(code: i64, message: impl Into<String>, data: Value) -> Self {
        Self {
            code,
            message: message.into(),
            data: Some(data),
        }
    }
}

/// Message JSON-RPC entrant (requête ou notification), déjà validé.
#[derive(Debug, Clone)]
pub struct Incoming {
    pub method: String,
    pub params: Option<Value>,
    /// Identifiant de la requête (`None` pour une notification ou un
    /// `id: null` explicite).
    pub id: Option<RequestId>,
    /// `id: null` **explicite** : requête à réponse obligatoire rendue avec
    /// `id: null` (une notification, elle, n'a pas de champ `id` du tout).
    pub null_id: bool,
}

impl Incoming {
    /// Une notification n'a pas d'`id` : aucune réponse ne doit lui être envoyée.
    /// Un `id: null` explicite n'est **pas** une notification.
    pub fn is_notification(&self) -> bool {
        self.id.is_none() && !self.null_id
    }
}

/// Échec de parsing : erreur de syntaxe (`-32700`) ou requête invalide
/// (`-32600` — version absente/incorrecte, lot, `id` de type interdit).
///
/// `Invalid` transporte l'`id` **best-effort** : lorsqu'il a pu être lu, la
/// réponse d'erreur doit le reprendre (JSON-RPC 2.0 §5).
#[derive(Debug, Clone, PartialEq)]
pub enum ParseFail {
    Syntax,
    Invalid {
        message: String,
        id: Option<RequestId>,
    },
}

impl ParseFail {
    /// Requête invalide sans `id` exploitable.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid {
            message: message.into(),
            id: None,
        }
    }

    /// Requête invalide avec l'`id` récupéré (à reprendre dans la réponse).
    pub fn invalid_with_id(message: impl Into<String>, id: Option<RequestId>) -> Self {
        Self::Invalid {
            message: message.into(),
            id,
        }
    }

    /// L'`id` récupéré, s'il y en a un d'exploitable.
    #[must_use]
    pub fn id(&self) -> Option<&RequestId> {
        match self {
            Self::Syntax => None,
            Self::Invalid { id, .. } => id.as_ref(),
        }
    }
}

/// Message JSON-RPC sortant.
#[derive(Debug, Serialize)]
pub struct Outgoing {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorObject>,
}

impl Outgoing {
    pub fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn failure(id: Value, error: ErrorObject) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(error),
        }
    }

    /// Réponse à une requête non décodable (`id` nul, comme l'exige la spec).
    pub fn parse_error() -> Self {
        Self::failure(
            Value::Null,
            ErrorObject::new(ErrorObject::PARSE_ERROR, "JSON invalide"),
        )
    }

    /// Réponse à une requête syntaxiquement invalide dont on connaît l'id.
    pub fn invalid_request(id: Option<RequestId>, message: impl Into<String>) -> Self {
        let id = match id {
            Some(id) => id.to_value(),
            None => Value::Null,
        };
        Self::failure(id, ErrorObject::new(ErrorObject::INVALID_REQUEST, message))
    }

    pub fn to_vec(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_else(|_| {
            br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"internal"}}"#.to_vec()
        })
    }
}

/// Parse un corps en [`Incoming`] — la séparation notification / `id: null`
/// explicite / requête normalement identifiée est exacte, la version
/// `jsonrpc` est vérifiée, et des `params` présents mais non-objets sont
/// refusés (`-32600`).
///
/// **L'`id` est extrait en premier**, avant la validation de `jsonrpc`,
/// `method` et `params` : une requête invalide reçoit une réponse portant
/// **le même `id`**. Seul un `id` lui-même illisible (booléen, tableau,
/// objet) est laissé à `null`.
///
/// `params` et `id` sont **déplacés** hors de l'objet parsé (jamais clonés) :
/// le chemin chaud d'un `tools/call` ne duplique pas les arguments.
pub fn parse_incoming(body: &[u8]) -> Result<Incoming, ParseFail> {
    let mut value: Value = serde_json::from_slice(body).map_err(|_| ParseFail::Syntax)?;
    let Some(object) = value.as_object_mut() else {
        // Lot (tableau) ou primitive : MCP n'envoie jamais de batch — requête invalide.
        return Err(ParseFail::invalid(
            "objet attendu (les lots ne sont pas supportés)",
        ));
    };

    // id : absent → notification ; null → requête à id null ; string/number → id.
    // Extrait EN PREMIER pour que toute erreur ultérieure puisse le reprendre.
    let mut id = None;
    let mut null_id = false;
    match object.remove("id") {
        None => {}
        Some(Value::Null) => null_id = true,
        Some(raw) => match serde_json::from_value::<RequestId>(raw) {
            Ok(parsed) => id = Some(parsed),
            Err(_) => {
                return Err(ParseFail::invalid(
                    "id doit être un nombre, une chaîne ou null",
                ));
            }
        },
    }

    match object.get("jsonrpc") {
        Some(Value::String(version)) if version == "2.0" => {}
        _ => {
            return Err(ParseFail::invalid_with_id(
                "jsonrpc doit être \"2.0\"",
                id.clone(),
            ));
        }
    }

    let method = match object.get("method") {
        Some(Value::String(method)) => method.clone(),
        _ => {
            return Err(ParseFail::invalid_with_id(
                "method (chaîne) requis",
                id.clone(),
            ));
        }
    };

    // JSON-RPC 2.0 : `params`, s'il est présent, DOIT être structuré — un
    // objet pour MCP (la forme positionnelle en tableau n'y est jamais
    // employée). Ignorer silencieusement un type interdit ferait exécuter
    // la requête sans ses paramètres : refus explicite en -32600.
    let params = match object.remove("params") {
        None => None,
        Some(params) if params.is_object() => Some(params),
        Some(_) => {
            return Err(ParseFail::invalid_with_id(
                "params doit être un objet",
                id.clone(),
            ));
        }
    };

    Ok(Incoming {
        method,
        params,
        id,
        null_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_id_roundtrip() {
        let id: RequestId = serde_json::from_str("42").unwrap();
        assert_eq!(id, RequestId::Number(42));
        let id: RequestId = serde_json::from_str(r#""abc""#).unwrap();
        assert_eq!(id, RequestId::String("abc".into()));
    }

    #[test]
    fn request_id_accepts_u64_beyond_i64_max() {
        let id: RequestId = serde_json::from_str("18446744073709551615").unwrap();
        assert_eq!(id, RequestId::Unsigned(u64::MAX));
        assert_eq!(id.to_value(), serde_json::json!(u64::MAX));
    }

    #[test]
    fn parse_error_carries_null_id() {
        let out = Outgoing::parse_error().to_vec();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["id"], Value::Null);
        assert_eq!(v["error"]["code"], -32700);
    }

    #[test]
    fn success_omits_error_field() {
        let out = Outgoing::success(Value::from(7), serde_json::json!({ "ok": true })).to_vec();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["id"], 7);
        assert!(v.get("error").is_none());
        assert_eq!(v["result"]["ok"], true);
    }

    #[test]
    fn absent_id_is_notification_explicit_null_is_not() {
        let absent = parse_incoming(br#"{"jsonrpc":"2.0","method":"ping"}"#).unwrap();
        assert!(absent.is_notification());
        assert!(!absent.null_id);

        let null = parse_incoming(br#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#).unwrap();
        assert!(!null.is_notification());
        assert!(null.null_id);
        assert!(null.id.is_none());
    }

    #[test]
    fn wrong_jsonrpc_version_is_invalid_request() {
        let fail = parse_incoming(br#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#).unwrap_err();
        assert!(matches!(fail, ParseFail::Invalid { .. }));
        assert_eq!(
            fail.id(),
            Some(&RequestId::Number(1)),
            "l'id doit être préservé pour la réponse d'erreur"
        );
        let fail = parse_incoming(br#"{"id":1,"method":"ping"}"#).unwrap_err();
        assert!(matches!(fail, ParseFail::Invalid { .. }));
    }

    #[test]
    fn batch_is_invalid_request_not_parse_error() {
        let fail = parse_incoming(br#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#).unwrap_err();
        assert!(matches!(fail, ParseFail::Invalid { .. }));
        let fail = parse_incoming(b"[]").unwrap_err();
        assert!(matches!(fail, ParseFail::Invalid { .. }));
        // un vrai garbage syntaxique reste un parse error
        let fail = parse_incoming(b"not json").unwrap_err();
        assert_eq!(fail, ParseFail::Syntax);
    }

    #[test]
    fn boolean_id_is_invalid_request() {
        let fail = parse_incoming(br#"{"jsonrpc":"2.0","id":true,"method":"ping"}"#).unwrap_err();
        assert!(matches!(fail, ParseFail::Invalid { .. }));
    }

    #[test]
    fn non_string_method_is_invalid_request() {
        let fail = parse_incoming(br#"{"jsonrpc":"2.0","id":1,"method":42}"#).unwrap_err();
        assert!(matches!(fail, ParseFail::Invalid { .. }));
    }

    #[test]
    fn non_object_params_are_invalid_request() {
        // JSON-RPC 2.0 : params, s'il est présent, doit être structuré.
        // MCP n'emploie jamais la forme positionnelle (tableau).
        let fail =
            parse_incoming(br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":[1,2]}"#)
                .unwrap_err();
        assert!(matches!(fail, ParseFail::Invalid { .. }));
        let fail = parse_incoming(br#"{"jsonrpc":"2.0","id":1,"method":"ping","params":"x"}"#)
            .unwrap_err();
        assert!(matches!(fail, ParseFail::Invalid { .. }));
        // Objet ou absent : valides.
        assert!(parse_incoming(br#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}"#).is_ok());
        assert!(parse_incoming(br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).is_ok());
    }

    #[test]
    fn invalid_request_keeps_the_id_in_the_response() {
        // JSON-RPC 2.0 §5 : la réponse d'erreur porte le même id.
        let fail = parse_incoming(br#"{"jsonrpc":"1.0","id":"abc","method":"ping"}"#).unwrap_err();
        let out = Outgoing::invalid_request(fail.id().cloned(), "requête invalide").to_vec();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["id"], "abc");
        assert_eq!(v["error"]["code"], -32600);
    }

    #[test]
    fn every_invalid_request_keeps_its_id() {
        // Toutes les causes d'invalidité doivent préserver l'id lisible.
        let fail = parse_incoming(br#"{"jsonrpc":"1.0","id":7,"method":"ping"}"#).unwrap_err();
        assert_eq!(fail.id(), Some(&RequestId::Number(7)));
        let fail = parse_incoming(br#"{"jsonrpc":"2.0","id":7,"method":42}"#).unwrap_err();
        assert_eq!(fail.id(), Some(&RequestId::Number(7)));
        let fail = parse_incoming(br#"{"jsonrpc":"2.0","id":7,"method":"ping","params":[1]}"#)
            .unwrap_err();
        assert_eq!(fail.id(), Some(&RequestId::Number(7)));
        let fail = parse_incoming(br#"{"jsonrpc":"2.0","id":"x","method":"ping","params":"s"}"#)
            .unwrap_err();
        assert_eq!(fail.id(), Some(&RequestId::String("x".into())));
    }
}
