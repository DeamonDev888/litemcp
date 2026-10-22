//! JSON-RPC 2.0 — le sous-ensemble utilisé par MCP.

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

/// Message JSON-RPC entrant (requête ou notification).
#[derive(Debug, Clone, Deserialize)]
pub struct Incoming {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
    #[serde(default)]
    pub id: Option<RequestId>,
}

impl Incoming {
    /// Une notification n'a pas d'`id` : aucune réponse ne doit lui être envoyée.
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

/// Réponse JSON-RPC sortante.
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

    pub fn to_vec(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_else(|_| {
            br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"internal"}}"#.to_vec()
        })
    }
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
}
