//! Campagne SLO reproductible : démarrage in-process, débit keep-alive et
//! mémoire (VmRSS via `/proc` — Linux ; « n/a » ailleurs).
//!
//! `cargo run --release --example slo`
//!
//! Méthodologie (conditions figées à publier avec les chiffres) : le
//! serveur et le client vivent dans le **même processus** — le VmRSS rendu
//! couvre donc l'ensemble (serveur + client de charge), comme les mesures
//! historiques du README. 200 requêtes de warmup, puis 2 300 mesurées.

use litemcp::prelude::*;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Instant;

const WARMUP: usize = 200;
const N: usize = 2_300;

fn vm_rss_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find(|line| line.starts_with("VmRSS:"))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
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

fn request(body: &str) -> String {
    format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

fn main() -> std::io::Result<()> {
    println!("=== litemcp — campagne SLO (release, in-process) ===");

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

    let mut stream = TcpStream::connect(addr)?;
    let first = request(r#"{"jsonrpc":"2.0","id":0,"method":"tools/list"}"#);
    stream.write_all(first.as_bytes())?;
    let mut buf = [0u8; 8192];
    stream.read(&mut buf)?;
    assert!(
        buf.starts_with(b"HTTP/1.1 200"),
        "première réponse : {buf:?}"
    );
    let startup_us = started.elapsed().as_micros();

    // Charge keep-alive : warmup puis N mesurées.
    let call = request(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"add","arguments":{"a":2,"b":3}}}"#,
    );
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
