#![forbid(unsafe_code)]
//! # litemcp
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
//! `format` et **appliqués**), [`nullable`](crate::zod::nullable),
//! [`describe`](crate::zod::describe) et le
//! [`outputSchema`](crate::server::ServerBuilder::output_schema), qui
//! valide la **sortie** structurée avant envoi. La feature `schemars`
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
//!     .bind("127.0.0.1:8080")?   // socket bindé — un thread par connexion
//!     .serve()?;                 // bloque : POST /mcp → JSON ou SSE
//! # Ok::<(), std::io::Error>(())
//! ```

pub mod http;
pub mod jsonrpc;
/// Macros `#[tool]`/`#[resource]`/`#[prompt]` (feature `macros`) — voir
/// [`litemcp_macros`].
#[cfg(feature = "macros")]
pub mod macros {
    pub use litemcp_macros::{prompt, resource, tool};
}
pub mod protocol;
/// Adaptateur schemars (feature `schemars`) — voir [`crate::schemars`].
#[cfg(feature = "schemars")]
pub mod schemars;
pub mod server;
pub mod zod;

pub use server::{ServerBuilder, ServerCore, ToolResult};

/// Version du crate.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Tout ce qu'il faut pour écrire un serveur en quelques lignes.
pub mod prelude {
    pub use crate::VERSION;
    pub use crate::http::ServerHandle;
    #[cfg(feature = "macros")]
    pub use crate::macros::{prompt, resource, tool};
    pub use crate::protocol::{
        CacheScope, PROTOCOL_VERSION, PromptMessage, ResultType, SUPPORTED_VERSIONS,
        ToolAnnotations,
    };
    #[cfg(feature = "schemars")]
    pub use crate::schemars::from_schemars;
    pub use crate::server::{McpHeaders, ServerBuilder, ToolResult};
    pub use crate::zod;
}
