//! Benchmark de la route maison : latence keep-alive et connexions neuves.
//!
//! `cargo run --release --example bench_backends`
//!
//! Le serveur sert le même outil ; le client mesure la latence
//! request/response sur N appels séquentiels en keep-alive (moyenne, p50,
//! p99 après warmup), puis en connexions neuves (conn/s).

use litemcp::prelude::*;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Instant;

const WARMUP: usize = 200;
const N_KEEPALIVE: usize = 2_000;
const N_FRESH: usize = 300;

fn build() -> ServerBuilder {
    ServerBuilder::new("bench", "0.5.1").tool("add", "Additionne deux entiers", |args| {
        let a = args["a"].as_i64().unwrap_or(0);
        let b = args["b"].as_i64().unwrap_or(0);
        Ok(serde_json::json!({ "somme": a + b }))
    })
}

fn http_request(body: &str, close: bool) -> String {
    format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n{body}",
        body.len(),
        if close { "close" } else { "keep-alive" }
    )
}

/// Lit une réponse complète (délimitée par son Content-Length) et renvoie
/// sa taille totale.
fn read_one_response(stream: &mut TcpStream) -> std::io::Result<usize> {
    let mut buf = [0u8; 4096];
    let mut received = 0usize;
    let mut expected = 0usize;
    loop {
        let read = stream.read(&mut buf)?;
        if read == 0 {
            return Err(std::io::Error::other("connexion fermée"));
        }
        received += read;
        if expected == 0 {
            // L'en-tête de réponse (~150 o) tient toujours dans le premier
            // segment du keep-alive — on l'analyse au premier passage.
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
            return Ok(received);
        }
    }
}

/// Latences ms (moyenne, p50, p99) sur N requêtes keep-alive, warmup déduit.
fn bench_keep_alive(port: u16, n: usize) -> std::io::Result<(f64, f64, f64)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"add","arguments":{"a":2,"b":3}}}"#;
    let request = http_request(body, false);
    // Warmup : TCP slow start, caches, allocations — pas compté.
    for _ in 0..WARMUP {
        stream.write_all(request.as_bytes())?;
        read_one_response(&mut stream)?;
    }
    let mut durations_us = Vec::with_capacity(n);
    for _ in 0..n {
        let start = Instant::now();
        stream.write_all(request.as_bytes())?;
        read_one_response(&mut stream)?;
        durations_us.push(start.elapsed().as_micros() as u64);
    }
    durations_us.sort_unstable();
    let mean = durations_us.iter().sum::<u64>() as f64 / durations_us.len() as f64 / 1_000.0;
    let p50 = durations_us[durations_us.len() / 2] as f64 / 1_000.0;
    let p99 = durations_us[durations_us.len() * 99 / 100] as f64 / 1_000.0;
    Ok((mean, p50, p99))
}

fn bench_fresh_conn(port: u16, n: usize) -> std::io::Result<f64> {
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"add","arguments":{"a":2,"b":3}}}"#;
    let request = http_request(body, true);
    let start = Instant::now();
    for _ in 0..n {
        let mut stream = TcpStream::connect(("127.0.0.1", port))?;
        stream.write_all(request.as_bytes())?;
        let mut raw = String::new();
        stream.read_to_string(&mut raw)?;
        assert!(raw.contains("HTTP/1.1 200 OK"), "{raw:?}");
    }
    let elapsed = start.elapsed().as_secs_f64();
    Ok(n as f64 / elapsed) // conn/s
}

fn main() -> std::io::Result<()> {
    println!("=== litemcp — bench route maison (release) ===");

    let bound = build().bind("127.0.0.1:0")?;
    let port = bound.local_addr()?.port();
    let handle = std::thread::spawn(move || bound.serve().unwrap());
    let (mean, p50, p99) = bench_keep_alive(port, N_KEEPALIVE)?;
    let fresh = bench_fresh_conn(port, N_FRESH)?;
    println!(
        "keep-alive : moy {mean:.3} ms · p50 {p50:.3} ms · p99 {p99:.3} ms ({N_KEEPALIVE} appels, warmup {WARMUP})"
    );
    println!("neuves     : {fresh:.0} conn/s ({N_FRESH} connexions)");
    drop(handle);

    Ok(())
}
