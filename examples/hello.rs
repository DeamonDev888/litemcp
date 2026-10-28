//! Serveur MCP minimal en HTTP stateless : `cargo run --example hello`,
//! qui affiche le port effectif au démarrage — premier port libre de la
//! tranche LiteMCP (`23080..=23099`), repli éphémère si elle est pleine.
//!
//! Montre les trois façons d'exposer un outil :
//! 1. `tool`       — closure sur `Value` brut (rapide, sans schéma) ;
//! 2. `tool_typed` — schéma à la Zod : `inputSchema` dérivé + validation ;
//! 3. `tool_parse` — schéma + arguments désérialisés dans un struct typé.

use litemcp::prelude::*;
use serde::Deserialize;

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    #[serde(default)]
    limit: Option<u32>,
}

fn main() -> std::io::Result<()> {
    let bound = ServerBuilder::new("hello", "0.1.0")
        // 1. Brut : la closure gère elle-même les arguments.
        .tool("hello", "Dit bonjour à quelqu'un", |args| {
            let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("monde");
            Ok(serde_json::json!({ "message": format!("Bonjour, {name} !") }))
        })
        // 2. Schéma Zod : l'inputSchema de tools/list est dérivé du schéma,
        //    les appels hors schéma sont rejetés en -32602 avec leurs issues.
        .tool_typed(
            "timestamp",
            "Renvoie un horodatage fixe (démo)",
            zod::object()
                .opt("tz", zod::enumeration(&["UTC", "local"]))
                .strict(),
            |args| {
                let tz = args.get("tz").and_then(|v| v.as_str()).unwrap_or("UTC");
                Ok(serde_json::json!({ "tz": tz, "now": "2026-10-22T00:00:00Z" }))
            },
        )
        // 3. Typé : le schéma valide puis désérialise dans le struct.
        .tool_parse(
            "search",
            "Recherche indexée (démo)",
            zod::object()
                .field("query", zod::string().min(2).max(64))
                .opt("limit", zod::integer().min(1).max(100))
                .strict(),
            |args: SearchArgs| {
                let limit = args.limit.unwrap_or(10);
                Ok(serde_json::json!({ "query": args.query, "limit": limit }))
            },
        )
        // Annotations de la spec (title + indices de comportement).
        .annotate(
            "timestamp",
            ToolAnnotations::new()
                .title("Horodatage")
                .read_only(true)
                .idempotent(true),
        )
        .annotate(
            "search",
            ToolAnnotations::new()
                .title("Recherche")
                .read_only(true)
                .open_world(false),
        )
        .bind_default()?;
    println!(
        "litemcp {VERSION} — MCP stateless sur http://{}/mcp",
        bound.local_addr()?
    );
    bound.serve()
}
