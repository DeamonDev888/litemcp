//! JSON-RPC 2.0 — types de base.
//!
//! Graine de la Phase 1 : `RequestId` est déjà réel et testé ; les messages
//! complets (`Request`, `Response`, `Notification`, `ErrorObject`) arrivent
//! avec le noyau protocole.

use serde::{Deserialize, Serialize};

/// Identifiant de requête JSON-RPC : nombre entier ou chaîne.
///
/// `null` est exclu volontairement (déconseillé par la spec JSON-RPC 2.0
/// côté requête, et jamais utilisé par MCP).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// Identifiant numérique.
    Number(i64),
    /// Identifiant textuel.
    String(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_id_roundtrip_number() {
        let id: RequestId = serde_json::from_str("42").unwrap();
        assert_eq!(id, RequestId::Number(42));
        assert_eq!(serde_json::to_string(&id).unwrap(), "42");
    }

    #[test]
    fn request_id_roundtrip_string() {
        let id: RequestId = serde_json::from_str(r#""abc-123""#).unwrap();
        assert_eq!(id, RequestId::String("abc-123".into()));
        assert_eq!(serde_json::to_string(&id).unwrap(), r#""abc-123""#);
    }
}
