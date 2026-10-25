//! Erreurs d'exécution **structurées** pour les outils ([`ToolError`]).
//!
//! Un `Err(String)` force les clients agents à parser de la prose pour
//! savoir s'il faut attendre, reformuler ou abandonner. Une erreur typée
//! leur donne ce verdict programmatique :
//!
//! ```
//! use litemcp::prelude::*;
//!
//! # fn demo() -> ToolResult {
//! # let plan = "free";
//! if plan == "free" {
//!     Err(ToolError::rate_limited("quota exhausted for the `free` plan")
//!         .retry_after_ms(500))
//! } else {
//!     Ok(serde_json::json!({ "status": "ok" }))
//! }
//! # }
//! ```
//!
//! Sur le fil, l'échec d'un outil reste un résultat `isError: true` (HTTP
//! 200, conformément à MCP) mais porte désormais un `structuredContent`
//! machine-lisible :
//!
//! ```json
//! {
//!   "isError": true,
//!   "content": [{ "type": "text", "text": "quota exhausted …" }],
//!   "structuredContent": {
//!     "error": {
//!       "code": "rate_limited",
//!       "message": "quota exhausted …",
//!       "retryable": true,
//!       "retryAfterMs": 500
//!     }
//!   }
//! }
//! ```
//!
//! **Compatibilité** : [`ToolResult`] accepte toujours `Err("message")`
//! (via `From<&str>`/`From<String>`) — l'erreur est alors `unclassified`,
//! non retentable par défaut. Le texte du `content` reste exactement le
//! message.
//!
//! [`ToolResult`]: crate::server::ToolResult

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Classification d'un échec d'outil — le verdict qui guide le client.
///
/// `retryable` par défaut (voir [`ErrorCode::retryable`]) :
/// - `rate_limited`, `upstream`, `timeout` → **retentable** (le client peut
///   rejouer la requête telle quelle, idéalement après `retryAfterMs`) ;
/// - `invalid_input`, `internal` → **non retentable tel quel** (reformuler
///   ou abandonner) ;
/// - `unclassified` — défaut des erreurs construites depuis une `String`
///   ( [`From<&str>`]) : non retentable par prudence, à reclasser quand
///   l'auteur du serveur connaît la cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Limite de débit ou quota atteint — retenter après un délai.
    RateLimited,
    /// Dépendance en amont défaillante (API tierce, base de données…).
    Upstream,
    /// Délai d'exécution dépassé ([`ServerBuilder::tool_timeout`]).
    ///
    /// [`ServerBuilder::tool_timeout`]: crate::server::ServerBuilder::tool_timeout
    Timeout,
    /// Arguments rejetés par le handler lui-même (le schéma est déjà passé).
    InvalidInput,
    /// Échec interne du serveur (contrat de sortie violé, invariant cassé).
    Internal,
    /// Échec non classé — défaut de `From<&str>`/`From<String>`.
    Unclassified,
}

impl ErrorCode {
    /// Ce code conseille-t-il une retente **telle quelle** ?
    ///
    /// C'est la valeur initiale de [`ToolError::retryable`] ; l'auteur peut
    /// la surcharger (`ToolError::retryable(false)`).
    #[must_use]
    pub fn retryable(self) -> bool {
        matches!(self, Self::RateLimited | Self::Upstream | Self::Timeout)
    }

    /// Forme fil (`"rate_limited"`, `"upstream"`…) — identique à la
    /// sérialisation serde.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RateLimited => "rate_limited",
            Self::Upstream => "upstream",
            Self::Timeout => "timeout",
            Self::InvalidInput => "invalid_input",
            Self::Internal => "internal",
            Self::Unclassified => "unclassified",
        }
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Erreur d'exécution d'un outil : code, message, politique de retente.
///
/// Le `message` part tel quel dans le `content` textuel (lisible par un
/// humain ou un LLM) ; le reste alimente `structuredContent.error`
/// (lisible par un programme). Voir le [module](self) pour la forme fil.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolError {
    /// Classification de l'échec.
    pub code: ErrorCode,
    /// Message lisible — texte exact du `content` du résultat.
    pub message: String,
    /// Le client peut-il retenter la requête **telle quelle** ?
    ///
    /// Initialisé à [`ErrorCode::retryable`] puis surchargeable
    /// ([`ToolError::retryable`]).
    pub retryable: bool,
    /// Délai conseillé avant retente, en millisecondes — absent = pas de
    /// conseil.
    #[serde(
        rename = "retryAfterMs",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub retry_after_ms: Option<u64>,
}

impl ToolError {
    /// Erreur classée — `retryable` suit le code.
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: code.retryable(),
            retry_after_ms: None,
        }
    }

    /// Limite de débit / quota — retentable.
    #[must_use]
    pub fn rate_limited(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::RateLimited, message)
    }

    /// Dépendance en amont défaillante — retentable.
    #[must_use]
    pub fn upstream(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Upstream, message)
    }

    /// Délai dépassé — retentable.
    #[must_use]
    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Timeout, message)
    }

    /// Arguments rejetés par le handler — non retentable (reformuler).
    #[must_use]
    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidInput, message)
    }

    /// Échec interne — non retentable.
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }

    /// Échec non classé — défaut de `From<String>`, non retentable.
    #[must_use]
    pub fn unclassified(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unclassified, message)
    }

    /// Conseille un délai avant retente (chaînable, type builder).
    #[must_use]
    pub fn retry_after_ms(mut self, ms: u64) -> Self {
        self.retry_after_ms = Some(ms);
        self
    }

    /// Surcharge la politique de retente dérivée du code (chaînable).
    #[must_use]
    pub fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    /// Forme fil — l'objet de `structuredContent.error`.
    #[must_use]
    pub fn to_json(&self) -> Value {
        // `retryAfterMs` omis quand absent (skip_serializing_if).
        serde_json::to_value(self).unwrap_or_else(|_| {
            json!({
                "code": self.code.as_str(),
                "message": self.message,
                "retryable": self.retryable,
            })
        })
    }
}

impl From<&str> for ToolError {
    fn from(message: &str) -> Self {
        Self::unclassified(message)
    }
}

impl From<String> for ToolError {
    fn from(message: String) -> Self {
        Self::unclassified(message)
    }
}

impl From<&String> for ToolError {
    fn from(message: &String) -> Self {
        Self::unclassified(message.as_str())
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ToolError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_defaults_follow_the_code() {
        assert!(ErrorCode::RateLimited.retryable());
        assert!(ErrorCode::Upstream.retryable());
        assert!(ErrorCode::Timeout.retryable());
        assert!(!ErrorCode::InvalidInput.retryable());
        assert!(!ErrorCode::Internal.retryable());
        assert!(!ErrorCode::Unclassified.retryable());
    }

    #[test]
    fn wire_shape_omits_absent_retry_after() {
        let error = ToolError::rate_limited("quota exhausted").retry_after_ms(500);
        let json = error.to_json();
        assert_eq!(json["code"], "rate_limited");
        assert_eq!(json["message"], "quota exhausted");
        assert_eq!(json["retryable"], true);
        assert_eq!(json["retryAfterMs"], 500);

        let bare = ToolError::unclassified("boom");
        let json = bare.to_json();
        assert_eq!(json["code"], "unclassified");
        assert!(json.get("retryAfterMs").is_none());
    }

    #[test]
    fn as_str_matches_serde_serialization() {
        for code in [
            ErrorCode::RateLimited,
            ErrorCode::Upstream,
            ErrorCode::Timeout,
            ErrorCode::InvalidInput,
            ErrorCode::Internal,
            ErrorCode::Unclassified,
        ] {
            assert_eq!(serde_json::to_value(code).unwrap(), code.as_str());
        }
    }

    #[test]
    fn string_conversions_are_unclassified() {
        assert_eq!(ToolError::from("boom").code, ErrorCode::Unclassified);
        assert_eq!(
            ToolError::from(String::from("boom")).code,
            ErrorCode::Unclassified
        );
        // `ok_or(&message)?` passe par From<&String>.
        let message = String::from("a missing");
        let error: ToolError = (&message).into();
        assert_eq!(error.message, "a missing");
    }
}
