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
//!
//! Spec cible : MCP 2026-07-28 — chaque message est un `POST /mcp`, la
//! réponse arrive en `application/json` (ou en `text/event-stream` si le
//! client ne demande que du SSE).
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
pub mod protocol;
pub mod server;
pub mod zod;

pub use server::{ServerBuilder, ServerCore};

/// Version du crate.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Tout ce qu'il faut pour écrire un serveur en quelques lignes.
pub mod prelude {
    pub use crate::VERSION;
    pub use crate::protocol::{PROTOCOL_VERSION, ToolAnnotations};
    pub use crate::server::{ServerBuilder, ToolResult};
    pub use crate::zod;
}
