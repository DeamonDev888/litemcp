//! JSON-RPC 2.0 — le sous-ensemble utilisé par MCP.
//!
//! Parsing conforme : la version `jsonrpc` doit être `"2.0"`, un `id` absent
//! désigne une notification alors qu'un `id: null` **explicite** est une
//! requête à réponse obligatoire (avec `id: null`), et un lot (tableau) est
//! refusé en `-32600` (non supporté par MCP, qui n'envoie jamais de batch).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Identifiant de requête JSON-RPC : nombre ou chaîne.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// Identifiant numérique.
    Number(i64),
    /// Identifiant textuel.
    String(String),
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
#[derive(Debug, Clone, PartialEq)]
pub enum ParseFail {
    Syntax,
    Invalid(String),
}

/// Message JSON-RPC sortant.
#[derive(Debug, Serialize)]
pub struct Outgoing {
    jsonrpc: String,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorObject>,
}

impl Outgoing {
    pub fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn failure(id: Value, error: ErrorObject) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
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
            Some(id) => serde_json::to_value(id).unwrap_or(Value::Null),
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
/// explicite / requête normalement identifiée est exacte, et la version
/// `jsonrpc` est vérifiée.
pub fn parse_incoming(body: &[u8]) -> Result<Incoming, ParseFail> {
    let value: Value = serde_json::from_slice(body).map_err(|_| ParseFail::Syntax)?;
    let Some(object) = value.as_object() else {
        // Lot (tableau) ou primitive : MCP n'envoie jamais de batch — requête invalide.
        return Err(ParseFail::Invalid(
            "objet attendu (les lots ne sont pas supportés)".to_string(),
        ));
    };

    match object.get("jsonrpc") {
        Some(Value::String(version)) if version == "2.0" => {}
        _ => return Err(ParseFail::Invalid("jsonrpc doit être \"2.0\"".to_string())),
    }

    let method = match object.get("method") {
        Some(Value::String(method)) => method.clone(),
        _ => return Err(ParseFail::Invalid("method (chaîne) requis".to_string())),
    };

    // id : absent → notification ; null → requête à id null ; string/number → id.
    let mut id = None;
    let mut null_id = false;
    match object.get("id") {
        None => {}
        Some(Value::Null) => null_id = true,
        Some(raw) => {
            let parsed: RequestId = serde_json::from_value(raw.clone()).map_err(|_| {
                ParseFail::Invalid("id doit être un nombre, une chaîne ou null".to_string())
            })?;
            id = Some(parsed);
        }
    }

    let params = object.get("params").cloned().filter(|p| p.is_object());

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
        assert_eq!(
            fail,
            ParseFail::Invalid("jsonrpc doit être \"2.0\"".to_string())
        );
        let fail = parse_incoming(br#"{"id":1,"method":"ping"}"#).unwrap_err();
        assert!(matches!(fail, ParseFail::Invalid(_)));
    }

    #[test]
    fn batch_is_invalid_request_not_parse_error() {
        let fail = parse_incoming(br#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#).unwrap_err();
        assert!(matches!(fail, ParseFail::Invalid(_)));
        let fail = parse_incoming(b"[]").unwrap_err();
        assert!(matches!(fail, ParseFail::Invalid(_)));
        // un vrai garbage syntaxique reste un parse error
        let fail = parse_incoming(b"not json").unwrap_err();
        assert_eq!(fail, ParseFail::Syntax);
    }

    #[test]
    fn boolean_id_is_invalid_request() {
        let fail = parse_incoming(br#"{"jsonrpc":"2.0","id":true,"method":"ping"}"#).unwrap_err();
        assert!(matches!(fail, ParseFail::Invalid(_)));
    }

    #[test]
    fn non_string_method_is_invalid_request() {
        let fail = parse_incoming(br#"{"jsonrpc":"2.0","id":1,"method":42}"#).unwrap_err();
        assert!(matches!(fail, ParseFail::Invalid(_)));
    }
}
