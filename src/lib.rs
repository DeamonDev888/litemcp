#![forbid(unsafe_code)]
//! # LiteMCP
//!
//! Le framework MCP (Model Context Protocol) le plus léger pour Rust :
//! **Streamable HTTP stateless sur un socket bindé — zéro runtime async,
//! `serde` + `serde_json` seulement.**
//!
//! Transport unique : Streamable HTTP — chaque message est un `POST /mcp`
//! sur un socket TCP bindé, la réponse arrive en JSON ou en flux SSE.
//! Aucun autre transport n'est implémenté.
//!
//! Schémas à la Zod ([`crate::zod`]) : dérivation de l'`inputSchema` et
//! validation des arguments avant exécution, handlers typés disponibles.
//! Le schéma publié est **exécuteur** : ce qu'il annonce est exactement ce
//! que la validation applique (pas de dérogation possible) — y compris les
//! **formats** (`.email()`, `.uuid()`, `.url()`, `.datetime()` : publiés
//! `format` et **appliqués**), les **valeurs par défaut**
//! ([`.default()`](crate::zod::Defaulted) : publiées **et** injectées),
//! les **dictionnaires** ([`record`](crate::zod::record)),
//! [`nullable`](crate::zod::nullable),
//! [`describe`](crate::zod::describe) et le
//! [`outputSchema`](crate::server::ServerBuilder::output_schema), qui
//! valide la **sortie** structurée avant envoi.
//!
//! Pour les schémas que le DSL n'exprime pas (`oneOf`, `not`,
//! `patternProperties`…), la feature `schemars` fournit
//! [`from_json_schema`](crate::schemars::from_json_schema) : un document
//! JSON Schema écrit à la main, publié tel quel **et** appliqué. La feature `schemars`
//! adapte en plus n'importe quel type `#[derive(JsonSchema)]`
//! ([`crate::schemars`]) avec les sémantiques JSON Schema de bout en bout.
//!
//! Trois surfaces MCP : **outils** (`tools/*`), **ressources**
//! (`resources/*`) et **prompts** (`prompts/*`) — chaque handler peut être
//! borné dans le temps ([`tool_timeout`](crate::server::ServerBuilder::tool_timeout)),
//! et le serveur s'arrête proprement
//! ([`handle`](crate::http::BoundServer::handle)/
//! [`shutdown`](crate::http::ServerHandle::shutdown)).
//! La feature `macros` ajoute `#[tool]` : le schéma d'entrée est dérivé
//! des types des paramètres d'une fonction ordinaire.
//!
//! **Erreurs structurées** ([`ToolError`](crate::error)) : un échec d'outil
//! classé (`rate_limited`, `upstream`, `timeout`…) avec politique de
//! retente (`retryable`, `retryAfterMs`) part sur le fil sous
//! `structuredContent.error` — le client agent réagit programmatiquement
//! au lieu de parser la prose (les `Err(String)` restent acceptés).
//! Tous les messages destinés aux clients (validation, protocole, HTTP)
//! sont **en anglais**.
//!
//! **Observabilité** ([`on_call`](crate::server::ServerBuilder::on_call)) :
//! un callback par message dispatché — méthode, outil, durée, verdict —
//! pour compteurs, latence et taux d'erreur sans instrumenter votre
//! client.
//!
//! **Contrôle d'accès**
//! ([`on_request`](crate::server::ServerBuilder::on_request)) : un verdict
//! par requête HTTP — auth par token, rate-limit par source ou par clé —
//! rejeté en `401`/`403`/`429` **avant la lecture du corps**. TLS et OIDC
//! restent à la périphérie (gateway / mesh) ; le verrou applicatif, lui,
//! vit ici.
//!
//! Spec cible : MCP **2026-07-28** — protocole **stateless**, sans handshake :
//! chaque message est un `POST /mcp`, la réponse arrive en `application/json`
//! (ou en `text/event-stream` si le client ne demande que du SSE).
//!
//! - `server/discover` : implémentée (SEP-2575) — annonce les versions
//!   supportées, les capacités (tools, resources, prompts — selon les
//!   registres) et l'identité du serveur ;
//! - `resultType` : présent dans **chaque** résultat (SEP-2322) ;
//! - `serverInfo` : le serveur s'identifie sous `_meta` de **chaque**
//!   résultat (SEP-2575) ;
//! - `ttlMs` / `cacheScope` : présents sur les résultats de liste et de
//!   lecture — `tools/list`, `resources/list`, `resources/read`,
//!   `prompts/list` (SEP-2549) ;
//! - version de protocole : lue dans `_meta`, refusée en `-32022` si
//!   inconnue ; son absence est tolérée (compatibilité `2025-11-25`) ;
//! - `Mcp-Method` / `Mcp-Name` : acceptés et vérifiés contre le corps
//!   (SEP-2243) ;
//! - `initialize` et `ping` : tolérés (méthodes legacy `2025-11-25`).
//!
//! Tutoriel officiel pas à pas (du premier outil aux schémas, ressources
//! et prompts) : <https://deamondev888.github.io/litemcp-tuto/>.
//!
//! ```no_run
//! use litemcp::prelude::*;
//!
//! ServerBuilder::new("calc", "1.0.0")
//!     .tool("add", "Additionne deux nombres", |args| {
//!         let a = args["a"].as_i64().ok_or("a manquant")?;
//!         let b = args["b"].as_i64().ok_or("b manquant")?;
//!         Ok(serde_json::json!({ "somme": a + b }))
//!     })
//!     .tool("heavy", "Peut manquer de quota", |_args| {
//!         Err(ToolError::rate_limited("quota exhausted").retry_after_ms(500))
//!     })
//!     .on_call(|event| {
//!         eprintln!("{} {} -> {:?} in {:?}", event.method,
//!             event.name.unwrap_or("-"), event.status, event.duration);
//!     })
//!     .bind("127.0.0.1:8080")?   // socket bindé — un thread par connexion
//!     .serve()?;                 // bloque : POST /mcp → JSON ou SSE
//! # Ok::<(), std::io::Error>(())
//! ```

pub mod error;
pub mod http;
pub mod jsonrpc;
/// Macros `#[tool]`/`#[resource]`/`#[prompt]` (feature `macros`) — voir
/// [`litemcp_macros`].
#[cfg(feature = "macros")]
pub mod macros {
    pub use litemcp_macros::{prompt, resource, tool};
}
pub mod protocol;
/// Annonce des serveurs au MCP Control (intégration native) —
/// opt-in via [`ServerBuilder::announce`].
pub mod registry;
/// Adaptateur schemars (feature `schemars`) — voir [`crate::schemars`].
#[cfg(feature = "schemars")]
pub mod schemars;
pub mod server;
pub mod zod;

pub use error::{ErrorCode, ToolError};
pub use http::{RequestContext, RequestRejection};
pub use server::{CallEvent, CallStatus, ServerBuilder, ServerCore, ToolResult};

/// Version du crate.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Tout ce qu'il faut pour écrire un serveur en quelques lignes.
pub mod prelude {
    pub use crate::VERSION;
    pub use crate::error::{ErrorCode, ToolError};
    pub use crate::http::{RequestContext, RequestRejection, ServerHandle};
    #[cfg(feature = "macros")]
    pub use crate::macros::{prompt, resource, tool};
    pub use crate::protocol::{
        CacheScope, PROTOCOL_VERSION, PromptArgument, PromptMessage, ResultType,
        SUPPORTED_VERSIONS, ToolAnnotations,
    };
    #[cfg(feature = "schemars")]
    pub use crate::schemars::{from_json_schema, from_schemars};
    pub use crate::server::{CallEvent, CallStatus, McpHeaders, ServerBuilder, ToolResult};
    pub use crate::zod::{self, Schema};
}
