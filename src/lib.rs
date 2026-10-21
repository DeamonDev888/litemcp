#![forbid(unsafe_code)]
//! # litemcp
//!
//! Le framework MCP (Model Context Protocol) le plus rapide et le plus léger
//! pour Rust — zéro runtime async au cœur, `serde` + `serde_json` seulement.
//!
//! Cible protocole : **MCP 2026-07-28** (stateless), avec stdio comme
//! transport natif et Streamable HTTP (POST unique → JSON ou SSE) en Phase 3.
//!
//! ```no_run
//! // Phase 2+ : exemple à venir
//! ```
//!
//! Voir `docs/ROADMAP.md` du dépôt pour les phases et jalons.

pub mod jsonrpc;
pub mod protocol;
pub mod transport;

/// Version du crate (utile aux benchmarks et aux logs de démarrage).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
