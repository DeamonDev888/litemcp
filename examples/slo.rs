//! Campagne SLO reproductible : démarrage in-process, débit keep-alive et
//! mémoire (VmRSS via `/proc` — Linux ; « n/a » ailleurs).
//!
//! `cargo run --release --example slo`
//!
//! Méthodologie (conditions figées à publier avec les chiffres) : le
//! serveur et le client vivent dans le **même processus** — le VmRSS rendu
//! couvre donc l'ensemble (serveur + client de charge), comme les mesures
//! historiques du README. 200 requêtes de warmup, puis 2 300 mesurées.
//!
//! Les requêtes portent l'enveloppe du **profil strict** (défaut depuis
//! 0.10.6) : `MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Name` pour
//! `tools/call`, et les deux clés `_meta` — le profil tolérant
//! (`.legacy(true)`) n'est pas utilisé, ce qui est mesuré est le chemin réel.

use litemcp::prelude::*;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Instant;

const WARMUP: usize = 200;
const N: usize = 2_300;

/// Révision servie par le profil strict.
const PROTOCOL_VERSION: &str = "2026-07-28";

/// `tools/list` : `Mcp-Name` est interdit sur cette méthode, `_meta` reste exigé.
const LIST_BODY: &str = r#"{"jsonrpc":"2.0","id":0,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}"#;

/// `tools/call` sur `add`, avec l'enveloppe `_meta` exigée en profil strict.
const CALL_BODY: &str = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}},"name":"add","arguments":{"a":2,"b":3}}}"#;

fn vm_rss_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find(|line| line.starts_with("VmRSS:"))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
}

/// Requête HTTP complète du profil strict (mêmes en-têtes que le README).
fn http_request(port: u16, method: &str, name: Option<&str>, body: &str, close: bool) -> String {
    let mut head = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json\r\nContent-Type: application/json\r\nMCP-Protocol-Version: {PROTOCOL_VERSION}\r\nMcp-Method: {method}\r\n"
    );
    if let Some(name) = name {
        head.push_str(&format!("Mcp-Name: {name}\r\n"));
    }
    format!(
        "{head}Content-Length: {}\r\nConnection: {}\r\n\r\n{body}",
        body.len(),
        if close { "close" } else { "keep-alive" }
    )
}

/// Lit une réponse complète d'une connexion `Connection: close` et vérifie
/// le statut — jamais un `400` compté comme une mesure.
fn one_shot(port: u16, method: &str, name: Option<&str>, body: &str) -> std::io::Result<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.write_all(http_request(port, method, name, body, true).as_bytes())?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw)?;
    assert!(
        raw.starts_with("HTTP/1.1 200 OK"),
        "enveloppe stricte refusée ({method}) : {raw:?}"
    );
    Ok(raw)
}

/// Lit une réponse complète délimitée par son `Content-Length`.
fn read_one_response(stream: &mut TcpStream) -> std::io::Result<()> {
    let mut buf = [0u8; 8192];
    let mut received = 0usize;
    let mut expected = 0usize;
    loop {
        let read = stream.read(&mut buf)?;
        if read == 0 {
            return Err(std::io::Error::other("connexion fermée"));
        }
        received += read;
        if expected == 0 {
            if let Ok(text) = std::str::from_utf8(&buf[..read]) {
                if let Some(pos) = text.find("Content-Length: ") {
                    let rest = &text[pos + "Content-Length: ".len()..];
                    let digits: &str = rest
                        .split(|c: char| !c.is_ascii_digit())
                        .next()
                        .unwrap_or("");
                    let head_len = text.find("\r\n\r\n").map(|p| p + 4).unwrap_or(0);
                    expected = head_len + digits.parse::<usize>().unwrap_or(0);
                }
            }
        }
        if expected > 0 && received >= expected {
            return Ok(());
        }
    }
}

fn main() -> std::io::Result<()> {
    println!("=== litemcp — campagne SLO (release, in-process) ===");
    println!("profil strict {PROTOCOL_VERSION} : 3 en-têtes + 2 clés `_meta` par requête");

    // Démarrage in-process : début de main → bind → première réponse 200.
    let started = Instant::now();
    let bound = ServerBuilder::new("slo", "1.0.0")
        .tool("add", "Additionne deux entiers", |args| {
            let a = args["a"].as_i64().unwrap_or(0);
            let b = args["b"].as_i64().unwrap_or(0);
            Ok(serde_json::json!({ "somme": a + b }))
        })
        .bind("127.0.0.1:0")?;
    let addr = bound.local_addr()?;
    std::thread::spawn(move || {
        let _ = bound.serve();
    });

    let _ = one_shot(addr.port(), "tools/list", None, LIST_BODY)?;
    let startup_us = started.elapsed().as_micros();

    // La sonde de démarrage s'est fermée : la charge part sur une connexion
    // neuve, sans reste de réponse à absorber. Le contrôle vérifie que le
    // handler répond vraiment avant de compter 2 300 tours.
    let probe = one_shot(addr.port(), "tools/call", Some("add"), CALL_BODY)?;
    assert!(
        probe.contains("\"somme\":5"),
        "handler non atteint : {probe:?}"
    );

    // Charge keep-alive : warmup puis N mesurées.
    let mut stream = TcpStream::connect(addr)?;
    let call = http_request(addr.port(), "tools/call", Some("add"), CALL_BODY, false);
    for _ in 0..WARMUP {
        stream.write_all(call.as_bytes())?;
        read_one_response(&mut stream)?;
    }
    let rss_avant = vm_rss_kib();
    let load_started = Instant::now();
    for _ in 0..N {
        stream.write_all(call.as_bytes())?;
        read_one_response(&mut stream)?;
    }
    let load_ms = load_started.elapsed().as_millis();
    let rss_apres = vm_rss_kib();

    println!("démarrage in-process (bind + 1re réponse) : {startup_us} µs");
    println!(
        "{N} requêtes keep-alive (warmup {WARMUP} déduit) : {load_ms} ms — {:.3} ms/req",
        load_ms as f64 / N as f64
    );
    match (rss_avant, rss_apres) {
        (Some(avant), Some(apres)) => {
            println!(
                "VmRSS avant charge : {avant} Kio · après : {apres} Kio (processus complet : serveur + client)"
            );
        }
        _ => println!("VmRSS : n/a (hors Linux — /proc/self/status indisponible)"),
    }
    Ok(())
}
