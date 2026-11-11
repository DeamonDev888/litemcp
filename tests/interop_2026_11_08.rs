//! Tests de régression du 2026-11-08 — deux corrections d'interopérabilité.
//!
//! 1. `server/discover` (et `initialize`) annoncent `tools`, `resources` et
//!    `prompts` dès que leurs méthodes sont servies — même si les registres
//!    sont vides et même avant un `Gateway::edit` — avec cohérence
//!    annonce ↔ appels réels vérifiée.
//! 2. Le filtre `Origin` par défaut accepte une origine loopback
//!    **strictement** cohérente avec le `Host` reçu et le listener local ;
//!    tout le reste reste refusé — matrice exécutée sur de vraies sockets.

use litemcp::protocol::{PromptMessage, ResourceContents};
use litemcp::{Gateway, ServerBuilder};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};

// --- Harnais dispatch (sans transport) ---

fn call(core: &litemcp::ServerCore, body: &str) -> Value {
    let reply = core
        .dispatch(body.as_bytes())
        .expect("une réponse est attendue pour une requête");
    serde_json::from_slice(&reply).unwrap()
}

fn call_ok(core: &litemcp::ServerCore, id: i64, method: &str, params: &str) -> Value {
    let body = if params.is_empty() {
        format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}"}}"#)
    } else {
        format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#)
    };
    let v = call(core, &body);
    assert!(
        v.get("error").is_none(),
        "méthode {method} doit réussir : {v}"
    );
    v
}

fn assert_three_capabilities(v: &Value, context: &str) {
    let caps = &v["result"]["capabilities"];
    assert!(caps["tools"].is_object(), "{context} : tools attendu : {v}");
    assert!(
        caps["resources"].is_object(),
        "{context} : resources attendu : {v}"
    );
    assert!(
        caps["prompts"].is_object(),
        "{context} : prompts attendu : {v}"
    );
    // Aucune sous-capacité non servie (`listChanged`, `subscribe` ne sont
    // pas implémentés : l'annonce ne doit pas les prétendre).
    assert!(
        caps["tools"].get("listChanged").is_none()
            && caps["resources"].get("listChanged").is_none()
            && caps["resources"].get("subscribe").is_none()
            && caps["prompts"].get("listChanged").is_none(),
        "{context} : aucune sous-capacité non servie ne doit être annoncée : {v}"
    );
}

// --- Harnais socket (vraies connexions TCP) ---

fn spawn_on(bind: &str) -> SocketAddr {
    let bound = ServerBuilder::new("interop", "0.1.0")
        .legacy(true)
        .tool("echo", "Echo", |args: &Value| Ok(args.clone()))
        .bind(bind)
        .unwrap();
    let addr = bound.local_addr().unwrap();
    std::thread::spawn(move || bound.serve().unwrap());
    addr
}

fn spawn_allowlisted(origins: &[&str]) -> SocketAddr {
    let bound = ServerBuilder::new("interop", "0.1.0")
        .legacy(true)
        .allow_origins(origins)
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = bound.local_addr().unwrap();
    std::thread::spawn(move || bound.serve().unwrap());
    addr
}

/// Échange brut `POST /mcp` (méthode `ping`) avec `Host`, `Origin` et
/// `Sec-Fetch-Site` contrôlés un à un ; renvoie le statut HTTP.
fn exchange(
    addr: SocketAddr,
    host: &str,
    origin: Option<&str>,
    sec_fetch_site: Option<&str>,
) -> u16 {
    let connect_addr = if addr.ip().is_unspecified() {
        SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            addr.port(),
        )
    } else {
        addr
    };
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    let mut request = format!("POST /mcp HTTP/1.1\r\nHost: {host}\r\n");
    if let Some(o) = origin {
        request.push_str(&format!("Origin: {o}\r\n"));
    }
    if let Some(s) = sec_fetch_site {
        request.push_str(&format!("Sec-Fetch-Site: {s}\r\n"));
    }
    request.push_str(&format!(
        "Accept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    let mut stream = TcpStream::connect(connect_addr).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    raw.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}

// ===========================================================================
// 1. `server/discover` — capacités annoncées même à registres vides
// ===========================================================================

#[test]
fn empty_server_announces_tools_resources_prompts() {
    let core = ServerBuilder::new("empty", "0.1.0").legacy(true).core();

    // `server/discover` : les trois surfaces sont annoncées.
    let v = call_ok(&core, 1, "server/discover", "");
    assert_three_capabilities(&v, "discover serveur vide");

    // Cohérence annonce ↔ méthodes réellement servies : `resources/list` et
    // `prompts/list` répondent avec un catalogue vide (pas de -32601).
    let rl = call_ok(&core, 2, "resources/list", "");
    assert_eq!(rl["result"]["resources"], json!([]), "{rl}");
    let pl = call_ok(&core, 3, "prompts/list", "");
    assert_eq!(pl["result"]["prompts"], json!([]), "{pl}");

    // `initialize` (legacy) partage le même contrat de capacités.
    let init = call_ok(&core, 4, "initialize", "");
    assert_three_capabilities(&init, "initialize serveur vide");
}

#[test]
fn gateway_dynamic_addition_keeps_announcements_and_serves() {
    let gw = Gateway::new(ServerBuilder::new("gw", "0.1.0"));

    // Avant tout `edit` : registres vides, capacités néanmoins annoncées.
    let before = gw.snapshot();
    let v = call_ok(&before, 1, "server/discover", "");
    assert_three_capabilities(&v, "discover avant edit");
    let tl = call_ok(&before, 2, "tools/list", "");
    assert_eq!(tl["result"]["tools"], json!([]), "{tl}");

    // Ajout dynamique d'un outil, d'une ressource et d'un prompt.
    gw.edit(|b| {
        b.tool("echo", "Echo", |args: &Value| Ok(args.clone()))
            .resource("mem://note", "Note", "une note", || {
                Ok(ResourceContents {
                    uri: "mem://note".to_string(),
                    mime_type: None,
                    text: "hello".to_string(),
                })
            })
            .prompt("salut", "Salue", |_args| {
                Ok(vec![PromptMessage::user("salut")])
            })
    });

    let after = gw.snapshot();

    // L'annonce reste la même (trois capacités, pas de sous-capacité).
    let v = call_ok(&after, 3, "server/discover", "");
    assert_three_capabilities(&v, "discover après edit");

    // Et les méthodes servent réellement les entrées ajoutées.
    let tl = call_ok(&after, 4, "tools/list", "");
    assert_eq!(tl["result"]["tools"][0]["name"], "echo", "{tl}");
    let tc = call_ok(
        &after,
        5,
        "tools/call",
        r#"{"name":"echo","arguments":{"ping":true}}"#,
    );
    assert_eq!(tc["result"]["structuredContent"]["ping"], true, "{tc}");

    let rl = call_ok(&after, 6, "resources/list", "");
    assert_eq!(rl["result"]["resources"][0]["uri"], "mem://note", "{rl}");
    let rr = call_ok(&after, 7, "resources/read", r#"{"uri":"mem://note"}"#);
    assert_eq!(rr["result"]["contents"][0]["text"], "hello", "{rr}");

    let pl = call_ok(&after, 8, "prompts/list", "");
    assert_eq!(pl["result"]["prompts"][0]["name"], "salut", "{pl}");
    let pg = call_ok(&after, 9, "prompts/get", r#"{"name":"salut"}"#);
    assert_eq!(
        pg["result"]["messages"][0]["content"]["text"], "salut",
        "{pg}"
    );
}

// ===========================================================================
// 2. Filtre `Origin` — exception loopback stricte (vraies sockets)
// ===========================================================================

#[test]
fn loopback_origin_matrix_on_loopback_bind() {
    let addr = spawn_on("127.0.0.1:0");
    let port = addr.port();
    let self_host = format!("127.0.0.1:{port}");
    let self_origin = format!("http://127.0.0.1:{port}");

    // ACCEPTÉ — origine loopback strictement cohérente avec Host et listener.
    assert_eq!(
        exchange(addr, &self_host, Some(&self_origin), None),
        200,
        "origine loopback identique au Host et au listener doit passer"
    );
    assert_eq!(
        exchange(addr, &self_host, Some(&self_origin), Some("same-origin")),
        200,
        "Sec-Fetch-Site: same-origin doit passer"
    );
    // ACCEPTÉ — pas d'Origin du tout (clients CLI) : inchangé.
    assert_eq!(exchange(addr, &self_host, None, None), 200);
    // ACCEPTÉ — casse de l'hôte : la comparaison d'autorité est insensible
    // à la casse (règle RFC 3986 §3.2.2 sur le reg-name, sans effet sur
    // un littéral IP).
    assert_eq!(
        exchange(
            addr,
            &format!("127.0.0.1:{port}"),
            Some(&format!("HTTP://127.0.0.1:{port}")),
            None
        ),
        200,
        "schéma en majuscules : comparaison insensible à la casse"
    );
}

#[test]
fn loopback_origin_mismatches_are_refused() {
    let addr = spawn_on("127.0.0.1:0");
    let port = addr.port();
    let self_host = format!("127.0.0.1:{port}");
    let self_origin = format!("http://127.0.0.1:{port}");

    // REFUSÉ — paire Host/Origin externe, même identique.
    assert_eq!(
        exchange(
            addr,
            "evil.example.com:80",
            Some("http://evil.example.com:80"),
            None
        ),
        403,
        "Host/Origin externes identiques doivent être refusés"
    );
    assert_eq!(
        exchange(
            addr,
            "evil.example.com",
            Some("http://evil.example.com"),
            None
        ),
        403
    );

    // REFUSÉ — cross-origin : Host interne / Origin externe, et l'inverse.
    assert_eq!(
        exchange(addr, &self_host, Some("http://evil.example.com"), None),
        403
    );
    assert_eq!(
        exchange(addr, "evil.example.com", Some(&self_origin), None),
        403
    );

    // REFUSÉ — divergences de schéma, d'hôte et de port.
    assert_eq!(
        exchange(
            addr,
            &self_host,
            Some(&format!("https://127.0.0.1:{port}")),
            None
        ),
        403,
        "divergence de schéma refusée"
    );
    assert_eq!(
        exchange(
            addr,
            &self_host,
            Some(&format!("http://127.0.0.1:{}", port.wrapping_add(1))),
            None
        ),
        403,
        "divergence de port refusée"
    );
    assert_eq!(
        exchange(addr, &self_host, Some("http://127.0.0.1"), None),
        403,
        "port http par défaut (80) ≠ port du listener : refusé"
    );
    assert_eq!(
        exchange(addr, "127.0.0.1", Some(&self_origin), None),
        403,
        "Host sans port (défaut 80) ≠ autorité explicite du listener : refusé"
    );
    assert_eq!(
        exchange(
            addr,
            &format!("127.0.0.2:{port}"),
            Some(&format!("http://127.0.0.2:{port}")),
            None
        ),
        403,
        "autre IP loopback que celle du listener : refusé"
    );
    assert_eq!(
        exchange(
            addr,
            &format!("localhost:{port}"),
            Some(&format!("http://localhost:{port}")),
            None
        ),
        403,
        "nom résolu (localhost) : pas un littéral IP égal au listener — refusé"
    );

    // REFUSÉ — `Origin: null` (jamais autorisé, même en loopback).
    assert_eq!(exchange(addr, &self_host, Some("null"), None), 403);

    // REFUSÉ — cross-site explicite : l'exception loopback n'ouvre pas
    // d'échappatoire Fetch Metadata (seul `allow_origins` l'ouvre).
    assert_eq!(
        exchange(addr, &self_host, Some(&self_origin), Some("cross-site")),
        403,
        "Sec-Fetch-Site: cross-site doit rester refusé"
    );

    // 400 — port non u16 : Origin syntaxiquement malformé.
    assert_eq!(
        exchange(addr, &self_host, Some("http://127.0.0.1:99999"), None),
        400,
        "port hors u16 : Origin malformé -> 400"
    );
}

#[test]
fn loopback_exception_closed_on_non_local_bind() {
    // Bind non local (toutes interfaces) : l'exception loopback ne s'ouvre
    // pas — `allow_origins` reste la seule porte.
    let addr = spawn_on("0.0.0.0:0");
    let port = addr.port();
    assert_eq!(
        exchange(
            addr,
            &format!("127.0.0.1:{port}"),
            Some(&format!("http://127.0.0.1:{port}")),
            None
        ),
        403,
        "bind non local : l'origine loopback cohérente doit rester refusée"
    );
    assert_eq!(
        exchange(addr, "t", Some("http://evil.example.com"), None),
        403,
        "bind non local : origine externe refusée"
    );
}

#[test]
fn allow_origins_still_works_for_explicit_deployments() {
    let addr = spawn_allowlisted(&["https://app.example"]);
    let port = addr.port();

    // Déploiement explicite : l'origine listée passe (Host externe inclus).
    assert_eq!(
        exchange(addr, "proxy.example:443", Some("https://app.example"), None),
        200,
        "allow_origins doit continuer à autoriser les origines listées"
    );
    // Et l'exception loopback par défaut reste active sur un bind loopback.
    assert_eq!(
        exchange(
            addr,
            &format!("127.0.0.1:{port}"),
            Some(&format!("http://127.0.0.1:{port}")),
            None
        ),
        200,
        "exception loopback sur bind loopback : acceptée"
    );
    // Une origine ni listée ni loopback-cohérente reste refusée.
    assert_eq!(
        exchange(
            addr,
            "proxy.example:443",
            Some("https://other.example"),
            None
        ),
        403
    );
}

#[test]
fn ipv6_loopback_origin_follows_the_same_rule() {
    let addr = spawn_on("[::1]:0");
    let port = addr.port();
    let self_host = format!("[::1]:{port}");
    let self_origin = format!("http://[::1]:{port}");

    assert_eq!(
        exchange(addr, &self_host, Some(&self_origin), None),
        200,
        "origine IPv6 loopback cohérente avec Host et listener : acceptée"
    );
    assert_eq!(
        exchange(
            addr,
            &self_host,
            Some(&format!("http://127.0.0.1:{port}")),
            None
        ),
        403,
        "IPv4 loopback ≠ IP du listener IPv6 : refusé"
    );
}
