//! Cible d'audit de sécurité — `cargo run --release --example audit_target`
//!
//! - 127.0.0.1:8099 — configuration **par défaut** : outils `add`, `echo`,
//!   `search` (schéma strict), `boom` (panique volontaire).
//! - 127.0.0.1:8098 — `allow_origins(["https://trusted.example"])` : garde
//!   DNS-rebinding en mode liste d'autorisation.

use litemcp::prelude::*;

fn main() -> std::io::Result<()> {
    std::thread::spawn(|| {
        ServerBuilder::new("audit-origins", "0.1.0")
            .tool("ping", "Répond pong", |_| Ok(serde_json::json!({ "pong": true })))
            .allow_origins(&["https://trusted.example"])
            .bind("127.0.0.1:8098")
            .expect("bind 8098")
            .serve()
            .expect("serve 8098");
    });
    println!("audit_target: 8099 (config défaut) + 8098 (origines autorisées)");
    ServerBuilder::new("audit", "0.1.0")
        .tool("add", "Additionne deux nombres", |args| {
            let a = args["a"].as_i64().ok_or("a manquant")?;
            let b = args["b"].as_i64().ok_or("b manquant")?;
            Ok(serde_json::json!({ "somme": a.wrapping_add(b) }))
        })
        .tool("echo", "Renvoie les arguments tels quels", |args| {
            Ok(args.clone())
        })
        .tool_typed(
            "search",
            "Recherche (schéma strict)",
            zod::object()
                .field("query", zod::string().min(2).max(64))
                .opt("limit", zod::integer().min(1).max(100))
                .strict(),
            |args| Ok(args.clone()),
        )
        .tool("boom", "Panique volontairement", |_| {
            panic!("panique de test");
        })
        .bind("127.0.0.1:8099")?
        .serve()
}
