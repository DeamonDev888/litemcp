//! Benchmark de la route maison : latence keep-alive et connexions neuves.
//!
//! `cargo run --release --example bench_backends`
//!
//! Le serveur sert le même outil ; le client mesure la latence
//! request/response sur N appels séquentiels en keep-alive, puis en
//! connexions neuves (conn/s).

use litemcp::prelude::*;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Instant;

const N_KEEPALIVE: usize = 2_000;
const N_FRESH: usize = 300;

fn build() -> ServerBuilder {
    ServerBuilder::new("bench", "0.2.0").tool("add", "Additionne deux entiers", |args| {
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

fn bench_keep_alive(port: u16, n: usize) -> std::io::Result<f64> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"add","arguments":{"a":2,"b":3}}}"#;
    let request = http_request(body, false);
    let start = Instant::now();
    for _ in 0..n {
        stream.write_all(request.as_bytes())?;
        // Lire la réponse complète (Content-Length).
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
                if let Ok(text) = std::str::from_utf8(&buf[..received]) {
                    if let Some(pos) = text.find("Content-Length: ") {
                        let rest = &text[pos + "Content-Length: ".len()..];
                        let digits: String =
                            rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                        let head_len = text.find("\r\n\r\n").map(|p| p + 4).unwrap_or(0);
                        expected = head_len + digits.parse::<usize>().unwrap_or(0);
                    }
                }
            }
            if expected > 0 && received >= expected {
                break;
            }
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    Ok(elapsed * 1_000.0 / n as f64) // ms par requête
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
    let ka = bench_keep_alive(port, N_KEEPALIVE)?;
    let fresh = bench_fresh_conn(port, N_FRESH)?;
    println!("keep-alive : {ka:.3} ms/requête ({N_KEEPALIVE} appels)");
    println!("neuves     : {fresh:.0} conn/s ({N_FRESH} connexions)");
    drop(handle);

    Ok(())
}
