//! Serveur MCP minimal en HTTP stateless : `cargo run --example hello`
//! puis `POST http://127.0.0.1:8080/mcp`.

use litemcp::prelude::*;

fn main() -> std::io::Result<()> {
    println!(
        "litemcp {} — MCP stateless sur http://127.0.0.1:8080/mcp",
        VERSION
    );
    ServerBuilder::new("hello", "0.1.0")
        .tool("hello", "Dit bonjour à quelqu'un", |args| {
            let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("monde");
            Ok(serde_json::json!({ "message": format!("Bonjour, {name} !") }))
        })
        .bind("127.0.0.1:8080")?
        .serve()
}
