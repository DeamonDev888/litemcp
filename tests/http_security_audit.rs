//! Tests d'intégration réels (sockets TCP) de l'audit de sécurité maximale
//! du transport HTTP maison (`src/http.rs`) et de ses garde-fous MCP (`src/server.rs`).

use litemcp::prelude::*;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

fn spawn_audit_server() -> (SocketAddr, Arc<AtomicUsize>) {
    let executed = Arc::new(AtomicUsize::new(0));
    let exec_clone = Arc::clone(&executed);
    let bound = ServerBuilder::new("sec-audit", "1.0.0")
        .legacy(true)
        .tool("admin_action", "Action sensible", move |_args| {
            exec_clone.fetch_add(1, Ordering::SeqCst);
            Ok(json!({ "executed": true }))
        })
        .tool("echo", "Echo", |args| Ok(args.clone()))
        .resource("sec://secret", "secret", "Secret resource", || {
            Ok(litemcp::protocol::ResourceContents {
                uri: "sec://secret".to_string(),
                mime_type: Some("text/plain".to_string()),
                text: "top-secret".to_string(),
            })
        })
        .read_timeout(Duration::from_millis(300))
        .bind("127.0.0.1:0")
        .expect("bind");
    let addr = bound.local_addr().expect("local_addr");
    std::thread::spawn(move || {
        let _ = bound.serve();
    });
    (addr, executed)
}

fn raw_exchange(addr: SocketAddr, request: &[u8]) -> (u16, String, String) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set_read_timeout");
    let _ = stream.write_all(request);
    let _ = stream.flush();
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (status, head.to_string(), body.to_string())
}

// ============================================================================
// 1. P0 — Anti-Request-Smuggling : ligne d'en-tête composée d'espaces (" \r\n")
// ============================================================================

#[test]
fn whitespace_only_header_line_is_rejected_and_never_ends_headers_early() {
    // Vulnérabilité corrigée : `line.trim_end()` transformait `" \r\n"` ou
    // `"\t\r\n"` en `""` AVANT le contrôle obs-fold, terminant prématurément
    // les en-têtes côté LiteMCP alors qu'un proxy amont continuait à lire les
    // en-têtes suivants (ex. `Transfer-Encoding: chunked` ou `Origin` caché).
    let (addr, executed) = spawn_audit_server();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"admin_action","arguments":{}}}"#;

    for ws_line in [" \r\n", "\t\r\n", "   \t  \r\n", "\u{00a0}\r\n"] {
        let smuggled = format!(
            "POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{ws_line}Transfer-Encoding: chunked\r\n\r\n{body}",
            body.len() + "Transfer-Encoding: chunked\r\n\r\n".len()
        );
        let (status, _head, resp_body) = raw_exchange(addr, smuggled.as_bytes());
        assert_eq!(
            status, 400,
            "la ligne d'espaces {ws_line:?} doit être refusée en 400 : {resp_body}"
        );
    }
    assert_eq!(
        executed.load(Ordering::SeqCst),
        0,
        "aucun appel ne doit passer via une fin d'en-têtes forgée"
    );
}

// ============================================================================
// 2. P1 — Anti-Smuggling : CR nu (`\r`), `\r\r\n` et caractères de contrôle
// ============================================================================

#[test]
fn bare_cr_and_double_cr_in_headers_are_rejected_400() {
    // Un `\r` nu au milieu d'un en-tête (`X-Pad: a\rTransfer-Encoding: chunked`)
    // ou une fin de ligne `\r\r\n` ne doivent jamais être acceptés.
    let (addr, _) = spawn_audit_server();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;

    for bad_header in [
        "X-Ignore: abc\rTransfer-Encoding: chunked\r\n",
        "X-Ignore: abc\rOrigin: https://evil.example\r\n",
        "Host: t\r\r\n",
        "X-Custom: val\r\r\n",
    ] {
        let host_line = if bad_header.starts_with("Host:") {
            ""
        } else {
            "Host: t\r\n"
        };
        let req = format!(
            "POST /mcp HTTP/1.1\r\n{host_line}{bad_header}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (status, _, resp) = raw_exchange(addr, req.as_bytes());
        assert_eq!(
            status, 400,
            "en-tête {bad_header:?} doit être refusé : {resp}"
        );
    }
}

#[test]
fn control_chars_and_non_ows_whitespace_in_headers_are_rejected_400() {
    let (addr, _) = spawn_audit_server();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;

    // 1. NUL, VT (\x0b), FF (\x0c), DEL (\x7f) dans une valeur d'en-tête -> 400.
    for bad_val in ["a\x00b", "a\x0bb", "a\x0cb", "a\x7fb", "a\u{0085}b"] {
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: t\r\nX-Test: {bad_val}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (status, _, _) = raw_exchange(addr, req.as_bytes());
        assert_eq!(
            status, 400,
            "valeur de contrôle {bad_val:?} doit donner 400"
        );
    }

    // 2. Content-Length préfixé par NBSP (\u{00a0}), VT ou FF : ne doit pas être trimé.
    for bad_cl in ["\u{00a0}2", "2\u{00a0}", "\x0b2", "\x0c2"] {
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: {bad_cl}\r\nConnection: close\r\n\r\n{{}}"
        );
        let (status, _, _) = raw_exchange(addr, req.as_bytes());
        assert_eq!(
            status, 400,
            "Content-Length {bad_cl:?} doit être refusé en 400"
        );
    }

    // 3. Caractères invalides dans le nom d'en-tête (espace interne, NBSP, délimiteur).
    for bad_name in [
        "X Bad",
        "X\u{00a0}Header",
        "X@Header",
        "X/Header",
        "Host\u{00a0}",
    ] {
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: t\r\n{bad_name}: v\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        );
        let (status, _, _) = raw_exchange(addr, req.as_bytes());
        assert_eq!(
            status, 400,
            "nom d'en-tête {bad_name:?} doit être refusé en 400"
        );
    }
}

// ============================================================================
// 3. P1 — Ligne de requête stricte (RFC 9112 §3 : simple SP, token, origin-form)
// ============================================================================

#[test]
fn request_line_rejects_multiple_leading_or_trailing_spaces_and_bad_targets() {
    let (addr, _) = spawn_audit_server();

    for bad_request_line in [
        " POST /mcp HTTP/1.1",
        "POST  /mcp HTTP/1.1",
        "POST /mcp  HTTP/1.1",
        "POST /mcp HTTP/1.1 ",
        "POST   /mcp   HTTP/1.1",
        "POST mcp HTTP/1.1",
        "POST http://localhost/mcp HTTP/1.1",
        "POST * HTTP/1.1",
        "POST /mcp#fragment HTTP/1.1",
        "POST /mcp?x=1#fragment HTTP/1.1",
        "G:ET /health HTTP/1.1",
        "POST@ /mcp HTTP/1.1",
    ] {
        let req = format!(
            "{bad_request_line}\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        );
        let (status, _, _) = raw_exchange(addr, req.as_bytes());
        assert_eq!(
            status, 400,
            "ligne de requête {bad_request_line:?} doit être refusée en 400"
        );
    }
}

// ============================================================================
// 4. P1 — SEP-2243 (`Mcp-Method` / `Mcp-Name`) : casse exacte et méthodes sans nom
// ============================================================================

#[test]
fn mcp_routing_headers_enforce_exact_case_and_reject_unexpected_or_empty_names() {
    let (addr, executed) = spawn_audit_server();

    // 1. Contournement par la casse (`Mcp-Name: ADMIN_ACTION` vs `"admin_action"`) -> -32020.
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"admin_action","arguments":{}}}"#;
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nMcp-Method: tools/call\r\nMcp-Name: ADMIN_ACTION\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, _, resp) = raw_exchange(addr, req.as_bytes());
    assert_eq!(status, 400);
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(
        v["error"]["code"], -32020,
        "Mcp-Name avec casse différente doit être refusé en -32020 : {v}"
    );
    assert_eq!(executed.load(Ordering::SeqCst), 0);

    // 2. Contournement par la casse sur `Mcp-Method: TOOLS/CALL` -> -32020.
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nMcp-Method: TOOLS/CALL\r\nMcp-Name: admin_action\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, _, resp) = raw_exchange(addr, req.as_bytes());
    assert_eq!(status, 400);
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(
        v["error"]["code"], -32020,
        "Mcp-Method avec casse différente doit être refusé en -32020 : {v}"
    );
    assert_eq!(executed.load(Ordering::SeqCst), 0);

    // 3. `Mcp-Name` présent sur `resources/read` ou `tools/list` -> -32020.
    for body_no_name in [
        r#"{"jsonrpc":"2.0","id":2,"method":"resources/read","params":{"uri":"sec://secret"}}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":4,"method":"ping"}"#,
    ] {
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nMcp-Name: echo\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body_no_name}",
            body_no_name.len()
        );
        let (status, _, resp) = raw_exchange(addr, req.as_bytes());
        assert_eq!(status, 400);
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(
            v["error"]["code"], -32020,
            "Mcp-Name sur une méthode sans nom doit donner -32020 : {v}"
        );
    }

    // 4. `Mcp-Name: ` (vide) ou `Mcp-Method: ` (vide) -> -32020.
    let body_missing_name = r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{}}"#;
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nMcp-Name: \r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body_missing_name}",
        body_missing_name.len()
    );
    let (_, _, resp) = raw_exchange(addr, req.as_bytes());
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], -32020, "Mcp-Name vide -> -32020 : {v}");

    // 5. En-têtes exacts et cohérents -> succès.
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nMcp-Method: tools/call\r\nMcp-Name: admin_action\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, _, resp) = raw_exchange(addr, req.as_bytes());
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["structuredContent"]["executed"], true);
    assert_eq!(executed.load(Ordering::SeqCst), 1);
}

#[test]
fn notification_with_incoherent_mcp_headers_is_silently_dropped() {
    // JSON-RPC 2.0 §4 : JAMAIS de réponse à une notification. Une notification
    // portant des en-têtes de routage incohérents doit donc être laissée
    // tomber (`202`, corps vide) — pas d'erreur `-32020` sur le fil, qui
    // ferait du serveur un émetteur de réponse sans `id` de requête. Le refus
    // reste visible des hooks d'observation.
    let (addr, executed) = spawn_audit_server();
    let body = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    for headers in [
        "Mcp-Method: tools/call\r\n", // méthode incohérente
        "Mcp-Method: \r\n",           // méthode vide
        "Mcp-Name: admin_action\r\n", // nom sur une méthode sans nom
    ] {
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (status, head, resp) = raw_exchange(addr, req.as_bytes());
        assert_eq!(
            status, 202,
            "notification incohérente ({headers:?}) : 202 sans corps attendu"
        );
        assert!(
            resp.is_empty(),
            "aucune réponse JSON-RPC attendue : {resp:?}"
        );
        assert!(
            !head.contains("jsonrpc"),
            "pas d'erreur JSON-RPC sur le fil"
        );
    }
    assert_eq!(
        executed.load(Ordering::SeqCst),
        0,
        "aucun outil ne doit avoir été exécuté"
    );

    // Frontière notification / requête : `id: null` **explicite** est une
    // requête à réponse obligatoire (pas une notification) — l'incohérence
    // d'en-tête y donne bien `-32020`, rendue avec `id: null`.
    let body_null = r#"{"jsonrpc":"2.0","id":null,"method":"notifications/initialized"}"#;
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nMcp-Method: tools/call\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body_null}",
        body_null.len()
    );
    let (status, _, resp) = raw_exchange(addr, req.as_bytes());
    assert_eq!(
        status, 400,
        "`id: null` explicite = requête : réponse attendue"
    );
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["id"], Value::Null);
    assert_eq!(v["error"]["code"], -32020, "{v}");
    assert_eq!(
        executed.load(Ordering::SeqCst),
        0,
        "l'outil nommé par l'en-tête ne doit pas être exécuté"
    );
}

// ============================================================================
// 5. P1 — `Expect` (RFC 9110 §10.1.1 : 417 Expectation Failed & pas de 100 en HTTP/1.0)
// ============================================================================

#[test]
fn unknown_expect_header_returns_417_and_http10_never_emits_100_continue() {
    let (addr, _) = spawn_audit_server();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;

    // 1. Expect inconnu ou vide -> 417 Expectation Failed.
    for bad_expect in ["custom-expect", "100-continue, unknown", ""] {
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: t\r\nExpect: {bad_expect}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (status, _, _) = raw_exchange(addr, req.as_bytes());
        assert_eq!(status, 417, "Expect: {bad_expect:?} doit répondre 417");
    }

    // 2. HTTP/1.0 avec `Expect: 100-continue` : ne doit JAMAIS émettre `100 Continue`
    //    (RFC 9110 §10.1.1 — évite l'empoisonnement de file de réponses HTTP/1.0).
    let req = format!(
        "POST /mcp HTTP/1.0\r\nHost: t\r\nExpect: 100-continue\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream.write_all(req.as_bytes()).unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    assert!(
        !raw.contains("100 Continue"),
        "HTTP/1.0 ne doit jamais recevoir 100 Continue : {raw:?}"
    );
    // La ligne de statut reflète la version de la requête (m4, audit
    // 2026-10-27) : un client HTTP/1.0 strict refuse une réponse annoncée
    // en `HTTP/1.1`.
    assert!(raw.starts_with("HTTP/1.0 200 OK"), "{raw:?}");
}

// ============================================================================
// 6. P1 — Anti-Slowloris & `connection_ttl` : borne stricte même en dribble ou idle
// ============================================================================

#[test]
fn slow_dribble_under_read_timeout_is_cut_by_scaled_request_deadline() {
    // read_timeout = 250 ms -> request_budget = 500 ms (et non 60 s).
    // Un client qui envoie un octet toutes les 120 ms (sous read_timeout)
    // est coupé dès ~500 ms.
    let bound = ServerBuilder::new("dribble", "1.0.0")
        .tool("ok", "ok", |_| Ok(json!({})))
        .read_timeout(Duration::from_millis(250))
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = bound.local_addr().unwrap();
    std::thread::spawn(move || {
        let _ = bound.serve();
    });

    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let started = Instant::now();
    let _ = stream.write_all(b"POST /mcp HTTP/1.1\r\nHost: t\r\n");
    for i in 0..20 {
        std::thread::sleep(Duration::from_millis(120));
        if stream
            .write_all(format!("X-Dribble-{i}: 1\r\n").as_bytes())
            .is_err()
        {
            break;
        }
        let mut buf = [0u8; 64];
        if let Ok(0) = stream.read(&mut buf) {
            break;
        }
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "le dribble lent sous read_timeout doit être coupé par le budget de requête (~500 ms), pris : {elapsed:?}"
    );
}

#[test]
fn connection_ttl_cuts_idle_keep_alive_even_when_read_timeout_is_long() {
    // AVANT : si `read_timeout` valait 30 s et `connection_ttl` 400 ms, une
    // connexion qui faisait 1 requête puis restait silencieuse bloquait dans
    // `fill_buf()` pendant 30 s. Désormais `tighten_read_timeout` resserre
    // le timeout de socket sur l'échéance du `connection_ttl`.
    let bound = ServerBuilder::new("ttl-idle", "1.0.0")
        .tool("ok", "ok", |_| Ok(json!({})))
        .read_timeout(Duration::from_secs(30))
        .connection_ttl(Duration::from_millis(450))
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = bound.local_addr().unwrap();
    std::thread::spawn(move || {
        let _ = bound.serve();
    });

    let started = Instant::now();
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    // 1re requête keep-alive servie immédiatement.
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: t\r\n\r\n")
        .unwrap();
    let mut buf = [0u8; 256];
    let n = stream.read(&mut buf).unwrap();
    assert!(std::str::from_utf8(&buf[..n]).unwrap().contains("200 OK"));

    // Le client reste silencieux : le serveur doit fermer à ~450 ms (pas 30 s).
    let n2 = stream.read(&mut buf).unwrap();
    assert_eq!(n2, 0, "EOF attendu à l'expiration du connection_ttl");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "la connexion idle doit fermer au connection_ttl (450 ms), pris : {:?}",
        started.elapsed()
    );
}

// ============================================================================
// 7. P2 — Validation stricte de `Host` (HTTP/1.1 & HTTP/1.0, RFC 9112 §3.2)
// ============================================================================

#[test]
fn malformed_host_header_and_http10_duplicate_host_are_rejected_400() {
    let (addr, _) = spawn_audit_server();

    for bad_host in [
        "",
        "user@evil.example",
        "evil.example/path",
        "evil.example?x=1",
        "evil.example#frag",
        "evil .example",
        "evil.example:abc",
        "evil.example:65536",
        "[::1",
        "[::1]:99999",
    ] {
        let req = format!("GET /health HTTP/1.1\r\nHost: {bad_host}\r\nConnection: close\r\n\r\n");
        let (status, _, _) = raw_exchange(addr, req.as_bytes());
        assert_eq!(status, 400, "Host: {bad_host:?} doit être refusé en 400");
    }

    // Doublon de Host en HTTP/1.0 -> 400 aussi (RFC 9112 §3.2).
    let req = "GET /health HTTP/1.0\r\nHost: a\r\nHost: b\r\n\r\n";
    let (status, _, _) = raw_exchange(addr, req.as_bytes());
    assert_eq!(
        status, 400,
        "Host dupliqué en HTTP/1.0 doit être refusé en 400"
    );

    // Host IPv6 valide avec port -> 200.
    let req = "GET /health HTTP/1.1\r\nHost: [::1]:8080\r\nConnection: close\r\n\r\n";
    let (status, _, body) = raw_exchange(addr, req.as_bytes());
    assert_eq!(status, 200);
    assert_eq!(body, "ok");
}

// ============================================================================
// 8. P2 — Garde DNS-Rebinding (`Origin: null`, syntaxe `Origin`, `Sec-Fetch-Site`)
// ============================================================================

#[test]
fn origin_null_and_cross_site_fetch_metadata_are_blocked() {
    // Même si un utilisateur mettait `"null"` par erreur dans `allow_origins`,
    // `Origin: null` (iframe sandboxée) ne doit JAMAIS être autorisé.
    let bound = ServerBuilder::new("origins", "1.0.0")
        .legacy(true)
        .tool("ok", "ok", |_| Ok(json!({ "ok": true })))
        .allow_origins(&["https://trusted.example", "null"])
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = bound.local_addr().unwrap();
    std::thread::spawn(move || {
        let _ = bound.serve();
    });
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;

    // 1. `Origin: null` -> 403 Forbidden.
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nOrigin: null\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, _, _) = raw_exchange(addr, req.as_bytes());
    assert_eq!(status, 403, "Origin: null ne doit jamais être autorisé");

    // 2. `Origin` malformé (vide, avec chemin `/`, avec espace) -> 400 Bad Request.
    for malformed_origin in [
        "",
        "https://trusted.example/path",
        "https://trusted.example https://evil.example",
        "*",
    ] {
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: t\r\nOrigin: {malformed_origin}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (status, _, _) = raw_exchange(addr, req.as_bytes());
        assert_eq!(
            status, 400,
            "Origin malformé {malformed_origin:?} doit donner 400"
        );
    }

    // 3. `Sec-Fetch-Site: cross-site` sans `Origin` autorisé -> 403 Forbidden.
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nSec-Fetch-Site: cross-site\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, _, _) = raw_exchange(addr, req.as_bytes());
    assert_eq!(
        status, 403,
        "Sec-Fetch-Site: cross-site sans Origin autorisé doit être bloqué en 403"
    );

    // 4. `Sec-Fetch-Site: cross-site` dupliqué -> 400 Bad Request.
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nSec-Fetch-Site: same-origin\r\nSec-Fetch-Site: cross-site\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, _, _) = raw_exchange(addr, req.as_bytes());
    assert_eq!(status, 400, "Sec-Fetch-Site dupliqué doit donner 400");

    // 5. `Sec-Fetch-Site: cross-site` AVEC `Origin: https://trusted.example` -> 200 OK.
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nOrigin: https://trusted.example\r\nSec-Fetch-Site: cross-site\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, _, _) = raw_exchange(addr, req.as_bytes());
    assert_eq!(
        status, 200,
        "Origin autorisé + Sec-Fetch-Site: cross-site doit passer"
    );
}

// ============================================================================
// 9. P2 — `Content-Encoding` et `Content-Type` (anti-contournement WAF)
// ============================================================================

#[test]
fn content_encoding_and_non_utf8_or_comma_content_type_are_rejected() {
    let (addr, _) = spawn_audit_server();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;

    // 1. `Content-Encoding: gzip` (ou `identity`, `br`) sur `/mcp` -> 415.
    for ce in ["gzip", "deflate", "br", "identity"] {
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Encoding: {ce}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (status, _, _) = raw_exchange(addr, req.as_bytes());
        assert_eq!(
            status, 415,
            "Content-Encoding: {ce} doit être refusé en 415"
        );
    }

    // 2. `Content-Encoding` dupliqué -> 400.
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Encoding: gzip\r\nContent-Encoding: br\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, _, _) = raw_exchange(addr, req.as_bytes());
    assert_eq!(status, 400);

    // 3. `Content-Type` avec virgule ou charset non-UTF-8 -> 415.
    for bad_ct in [
        "application/json, text/plain",
        "application/json; charset=utf-16",
        "application/json; charset=iso-8859-1",
        "application/json;",
    ] {
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Type: {bad_ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (status, _, _) = raw_exchange(addr, req.as_bytes());
        assert_eq!(status, 415, "Content-Type {bad_ct:?} doit donner 415");
    }
}

// ============================================================================
// 10. P3 — `Accept` avec `q=0` et validation stricte des routes au `bind`
// ============================================================================

#[test]
fn accept_q_zero_is_honored_over_http() {
    let (addr, _) = spawn_audit_server();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;

    // `Accept: text/event-stream, application/json;q=0` -> client refuse JSON,
    // demande SSE -> la réponse doit être `text/event-stream`.
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nAccept: text/event-stream, application/json;q=0\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, head, resp_body) = raw_exchange(addr, req.as_bytes());
    assert_eq!(status, 200);
    assert!(
        head.contains("Content-Type: text/event-stream"),
        "application/json;q=0 ne doit pas bloquer le basculement SSE : {head}"
    );
    assert!(resp_body.starts_with("data: "), "{resp_body:?}");

    // `Accept: text/event-stream;q=0` -> client refuse SSE -> réponse JSON.
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nAccept: text/event-stream;q=0\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, head, _) = raw_exchange(addr, req.as_bytes());
    assert_eq!(status, 200);
    assert!(
        head.contains("Content-Type: application/json"),
        "text/event-stream;q=0 ne doit pas basculer en SSE : {head}"
    );
}

#[test]
fn bind_rejects_routes_with_fragments_whitespace_or_control_chars() {
    for bad_route in [
        "/mcp#frag",
        "/mcp space",
        "/mcp\t",
        "/mcp\r\n",
        "/mcp\x00",
        "/café",
    ] {
        let err = match ServerBuilder::new("r", "1.0.0")
            .route(bad_route)
            .bind("127.0.0.1:0")
        {
            Ok(_) => panic!("route {bad_route:?} aurait dû être refusée au bind"),
            Err(err) => err,
        };
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }
}

// ============================================================================
// 11. P2 — `431` avec octets en vol : pas de RST qui purge la réponse client
// ============================================================================

#[test]
fn error_431_drains_gracefully_so_client_never_loses_response_to_tcp_rst() {
    let (addr, _) = spawn_audit_server();
    // Le client envoie 90 en-têtes (> MAX_HEADERS = 64) + un corps de 8 Kio
    // dans la foulée : sans `shutdown_gracefully` sur le chemin 431, le close()
    // avec octets non lus dans le tampon noyau émettait un RST qui purgeait le 431.
    let mut payload = String::from("POST /mcp HTTP/1.1\r\nHost: t\r\n");
    for i in 0..90 {
        payload.push_str(&format!("X-Extra-{i}: {}\r\n", "v".repeat(64)));
    }
    let body = "A".repeat(8192);
    payload.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));

    for _ in 0..10 {
        let (status, head, resp_body) = raw_exchange(addr, payload.as_bytes());
        assert_eq!(
            status, 431,
            "le 431 ne doit jamais être perdu par RST : {head}"
        );
        assert!(head.contains("Connection: close"), "{head}");
        assert!(resp_body.contains("too many headers"), "{resp_body}");
    }
}

// ============================================================================
// 12. P1 — Isolation pipeline : aucune requête suivant une erreur n'est exécutée
// ============================================================================

#[test]
fn pipelined_request_after_any_protocol_violation_is_never_executed() {
    let (addr, executed) = spawn_audit_server();
    let second_body = r#"{"jsonrpc":"2.0","id":99,"method":"tools/call","params":{"name":"admin_action","arguments":{}}}"#;
    let second_req = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{second_body}",
        second_body.len()
    );

    for first_req in [
        // 1. Espace doublé dans la request line
        "POST  /mcp HTTP/1.1\r\nHost: t\r\nContent-Length: 2\r\n\r\n{}".to_string(),
        // 2. Host invalide avec `@`
        "POST /mcp HTTP/1.1\r\nHost: user@t\r\nContent-Length: 2\r\n\r\n{}".to_string(),
        // 3. Content-Encoding non supporté
        "POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Encoding: gzip\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}".to_string(),
        // 4. Expect inconnu (417)
        "POST /mcp HTTP/1.1\r\nHost: t\r\nExpect: 999-custom\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}".to_string(),
    ] {
        let pipeline = format!("{first_req}{second_req}");
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let _ = stream.write_all(pipeline.as_bytes());
        let mut raw = String::new();
        let _ = stream.read_to_string(&mut raw);
        assert_eq!(
            raw.matches("HTTP/1.1 ").count(),
            1,
            "une seule réponse doit partir avant fermeture : {raw:?}"
        );
        assert!(
            !raw.contains("\"id\":99"),
            "la 2e requête pipelinée ne doit jamais être exécutée : {raw:?}"
        );
    }
    assert_eq!(executed.load(Ordering::SeqCst), 0);
}

// ============================================================================
// 13. L1 — `Host` confronté au listener (`allowed_hosts`, `421 Misdirected Request`)
// ============================================================================

#[test]
fn host_etranger_est_refuse_421_sans_origin() {
    // Avant L1, `Host` n'était vérifié qu'en syntaxe : `POST /mcp` avec
    // `Host: attaquant.example` et **sans** `Origin` rendait 200 — la garde
    // DNS-rebinding ne couvrait donc pas un client non-navigateur (ou une
    // page qui omet `Origin`) qui vise le loopback par un nom qu'il contrôle.
    let (addr, executed) = spawn_audit_server();
    let body = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"admin_action","arguments":{}}}"#;

    for host in [
        "attaquant.example",
        "attaquant.example:443",
        "ataquant.example:23080",
        "10.1.2.3",
        "10.1.2.3:23080",
    ] {
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (status, head, resp) = raw_exchange(addr, req.as_bytes());
        assert_eq!(
            status, 421,
            "Host {host:?} doit être refusé en 421 Misdirected Request : {head}"
        );
        assert!(
            !resp.contains("executed"),
            "aucune réponse de handler derrière un Host étranger : {resp:?}"
        );
    }
    assert_eq!(
        executed.load(Ordering::SeqCst),
        0,
        "aucun handler ne doit s'exécuter derrière un Host étranger"
    );
}

#[test]
fn host_du_listener_est_accepte_sans_origin() {
    let (addr, _) = spawn_audit_server();
    let port = addr.port();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;

    // Autorités **locales** : adresse effective du listener, `localhost`,
    // littéral de bouclage — avec ou sans port, la partie nom reste locale.
    for host in [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        "127.0.0.1".to_string(),
        "localhost".to_string(),
        "mon-service".to_string(),
    ] {
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (status, _, resp) = raw_exchange(addr, req.as_bytes());
        assert_eq!(status, 200, "Host {host:?} doit être accepté : {resp}");
    }
}

#[test]
fn host_ipv6_de_bouclage_est_accepte_sur_un_listener_ipv6() {
    let Ok(bound) = ServerBuilder::new("v6", "1.0.0")
        .legacy(true)
        .tool("ok", "Répond ok", |_| Ok(json!({ "ok": true })))
        .bind("[::1]:0")
    else {
        return; // IPv6 indisponible sur ce runner : rien à vérifier.
    };
    let addr = bound.local_addr().expect("local_addr");
    std::thread::spawn(move || {
        let _ = bound.serve();
    });
    let port = addr.port();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    for host in [format!("[::1]:{port}"), format!("localhost:{port}")] {
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (status, head, _) = raw_exchange(addr, req.as_bytes());
        assert_eq!(status, 200, "Host {host:?} sur listener [::1] : {head}");
    }
}

#[test]
fn host_ipv4_est_accepte_sur_un_listener_double_pile() {
    // A5 (contre-revue) : sur un listener `[::]:port`, un client IPv4 fait
    // rendre `::ffff:a.b.c.d` à `stream.local_addr()`, alors que `Host` porte
    // le littéral IPv4 nu. Sans normalisation v4-mappée, `Host: a.b.c.d:port`
    // (l'IP de la machine) rendait `421` alors que le client parle bien à ce
    // listener. La normalisation elle-même est verrouillée unitairement dans
    // `src/http.rs` (`host_autorise_normalise_les_adresses_ipv4_mappees`) ;
    // ce test couvre le chemin réel bout en bout.
    let Ok(bound) = ServerBuilder::new("double-pile", "1.0.0")
        .legacy(true)
        .tool("ok", "Répond ok", |_| Ok(json!({ "ok": true })))
        .bind("[::]:0")
    else {
        return; // IPv6 indisponible sur ce runner : rien à vérifier.
    };
    let addr = bound.local_addr().expect("local_addr");
    std::thread::spawn(move || {
        let _ = bound.serve();
    });
    let port = addr.port();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;

    // Plateforme double pile (Linux) : le client IPv4 parle au listener
    // `[::]` et `stream.local_addr()` rend `::ffff:127.0.0.1`. Sous Windows,
    // un listener `[::]` est **v6-only** par défaut : la connexion v4 est
    // refusée avant toute requête, on vise alors `[::1]`. La normalisation
    // v4-mappée est verrouillée unitairement dans `src/http.rs`
    // (`host_autorise_normalise_les_adresses_ipv4_mappees`) ; ce test couvre
    // le chemin réel bout en bout là où la plateforme l'expose.
    let v4 = SocketAddr::from(([127, 0, 0, 1], port));
    let v6 = SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port));
    let (cible, hotes) = match TcpStream::connect(v4) {
        Ok(_) => (
            v4,
            vec![format!("127.0.0.1:{port}"), format!("localhost:{port}")],
        ),
        Err(_) => (
            v6,
            vec![format!("[::1]:{port}"), format!("localhost:{port}")],
        ),
    };
    for host in hotes {
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (status, head, _) = raw_exchange(cible, req.as_bytes());
        assert_eq!(status, 200, "Host {host:?} sur listener [::] : {head}");
    }

    // Forme **v4-mappée** : elle ne se résout en une interface locale qu'après
    // normalisation — `421` avant le correctif, sur les deux plateformes.
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: [::ffff:127.0.0.1]:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, head, _) = raw_exchange(cible, req.as_bytes());
    assert_eq!(
        status, 200,
        "Host v4-mappé de bouclage sur listener [::] : {head}"
    );

    for host in [format!("10.1.2.3:{port}"), "10.1.2.3".to_string()] {
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (status, head, _) = raw_exchange(cible, req.as_bytes());
        assert_eq!(
            status, 421,
            "Host {host:?} (IP étrangère non mappée) doit rester refusé : {head}"
        );
    }
}

#[test]
fn allowed_hosts_custom_ajoute_le_nom_public_du_proxy() {
    // Reverse proxy : le client envoie le nom public, le proxy route vers le
    // loopback. `.allowed_hosts` ouvre ce nom — en **ajout** des autorités
    // locales par défaut (une configuration explicite ne doit pas casser
    // l'exception loopback documentée par `allow_origins`).
    let bound = ServerBuilder::new("proxy", "1.0.0")
        .legacy(true)
        .tool("ok", "Répond ok", |_| Ok(json!({ "ok": true })))
        .allowed_hosts(["mcp.exemple.com", "interne.exemple.com:8443"])
        .bind("127.0.0.1:0")
        .expect("bind");
    let addr = bound.local_addr().expect("local_addr");
    std::thread::spawn(move || {
        let _ = bound.serve();
    });
    let port = addr.port();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;

    for host in [
        "mcp.exemple.com".to_string(),
        "mcp.exemple.com:443".to_string(),
        "interne.exemple.com:8443".to_string(),
        // Autorités locales : toujours acceptées.
        format!("localhost:{port}"),
        "127.0.0.1".to_string(),
    ] {
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (status, head, _) = raw_exchange(addr, req.as_bytes());
        assert_eq!(status, 200, "Host {host:?} doit être accepté : {head}");
    }

    for host in ["autre.exemple.com", "evil.example", "10.1.2.3:23080"] {
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (status, head, _) = raw_exchange(addr, req.as_bytes());
        assert_eq!(
            status, 421,
            "Host {host:?} hors liste explicite doit être refusé : {head}"
        );
    }
}

// ============================================================================
// 14. L1 — CORS complet (`OPTIONS` preflight 204, `Access-Control-Allow-Origin`)
// ============================================================================

fn spawn_cors_server() -> SocketAddr {
    let bound = ServerBuilder::new("cors", "1.0.0")
        .legacy(true)
        .tool("ok", "Répond ok", |_| Ok(json!({ "ok": true })))
        .allow_origins(&["https://app.exemple.com"])
        .bind("127.0.0.1:0")
        .expect("bind");
    let addr = bound.local_addr().expect("local_addr");
    std::thread::spawn(move || {
        let _ = bound.serve();
    });
    addr
}

#[test]
fn cors_preflight_autorise_rend_204_avec_les_entetes_attendus() {
    // Avant L1 : `OPTIONS /mcp` rendait 405 et aucune réponse ne portait
    // `Access-Control-Allow-Origin` — un client navigateur ne pouvait rien lire.
    let addr = spawn_cors_server();
    let req = "OPTIONS /mcp HTTP/1.1\r\nHost: t\r\nOrigin: https://app.exemple.com\r\nAccess-Control-Request-Method: POST\r\nAccess-Control-Request-Headers: content-type,mcp-protocol-version\r\nConnection: close\r\n\r\n";
    let (status, head, body) = raw_exchange(addr, req.as_bytes());
    assert_eq!(status, 204, "préflight autorisé : {head}");
    assert_eq!(body, "", "un 204 n'a pas de corps");
    assert!(
        head.contains("Access-Control-Allow-Origin: https://app.exemple.com"),
        "ACAO échoé : {head}"
    );
    assert!(
        head.contains("Access-Control-Allow-Methods: POST, OPTIONS"),
        "méthodes autorisées : {head}"
    );
    assert!(
        head.contains(
            "Access-Control-Allow-Headers: Content-Type, Accept, MCP-Protocol-Version, Mcp-Method, Mcp-Name"
        ),
        "en-têtes autorisés : {head}"
    );
    assert!(head.contains("Vary: Origin"), "Vary: Origin : {head}");
    assert!(
        !head.to_ascii_lowercase().contains("content-length"),
        "un 204 ne doit pas porter Content-Length (RFC 9110 §8.6) : {head}"
    );
}

#[test]
fn cors_preflight_origine_refusee_ou_absente_rend_403() {
    let addr = spawn_cors_server();
    for origin_line in [
        "Origin: https://evil.example\r\n",
        "Origin: null\r\n",
        "", // pas d'`Origin` : rien n'est autorisé
    ] {
        let req = format!(
            "OPTIONS /mcp HTTP/1.1\r\nHost: t\r\n{origin_line}Access-Control-Request-Method: POST\r\nConnection: close\r\n\r\n"
        );
        let (status, head, _) = raw_exchange(addr, req.as_bytes());
        assert_eq!(
            status, 403,
            "préflight {origin_line:?} doit être refusé : {head}"
        );
        assert!(
            !head.contains("Access-Control-Allow-Origin"),
            "aucun ACAO sur un refus : {head}"
        );
    }
}

#[test]
fn cors_post_origine_autorisee_porte_acao_et_vary() {
    let addr = spawn_cors_server();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nOrigin: https://app.exemple.com\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, head, resp) = raw_exchange(addr, req.as_bytes());
    assert_eq!(status, 200, "{head}");
    assert!(
        head.contains("Access-Control-Allow-Origin: https://app.exemple.com"),
        "ACAO sur la réponse POST autorisée : {head}"
    );
    assert!(head.contains("Vary: Origin"), "Vary: Origin : {head}");
    assert!(resp.contains("\"result\""), "{resp:?}");

    // Sans `Origin` (client CLI), aucune en-tête CORS n'est ajouté : le
    // chemin chaud reste identique octet pour octet.
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, head, _) = raw_exchange(addr, req.as_bytes());
    assert_eq!(status, 200, "{head}");
    assert!(
        !head.contains("Access-Control-Allow-Origin"),
        "pas d'ACAO sans Origin : {head}"
    );
}

#[test]
fn cors_notification_origine_autorisee_porte_acao_et_vary() {
    // B1 (contre-revue) : le chemin notification (`Ok(None)` → `202`) passait
    // une chaîne vide en `extra_headers` — aucun en-tête CORS. `notifications/
    // initialized` est pourtant la première requête qu'un client MCP envoie
    // après l'initialisation : sans `Access-Control-Allow-Origin` sur le
    // `202`, le navigateur rejette la réponse et la session ne démarre pas.
    let addr = spawn_cors_server();
    let body = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nOrigin: https://app.exemple.com\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, head, resp) = raw_exchange(addr, req.as_bytes());
    assert_eq!(status, 202, "notification d'origine autorisée : {head}");
    assert!(resp.is_empty(), "un 202 n'a pas de corps : {resp:?}");
    assert!(
        head.contains("Access-Control-Allow-Origin: https://app.exemple.com"),
        "ACAO sur le 202 : {head}"
    );
    assert!(
        head.contains("Vary: Origin"),
        "Vary: Origin sur le 202 : {head}"
    );

    // Une origine non autorisée reste refusée en 403, sans en-tête CORS.
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nOrigin: https://evil.example\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, head, _) = raw_exchange(addr, req.as_bytes());
    assert_eq!(status, 403, "origine non autorisée : {head}");
    assert!(
        !head.contains("Access-Control-Allow-Origin"),
        "aucun ACAO sur un refus : {head}"
    );
}

// ============================================================================
// 15. L1 — Échéance **globale** d'écriture (client qui lit lentement)
// ============================================================================

#[test]
fn client_qui_ne_lit_pas_est_coupe_a_l_echeance_globale() {
    // Remplace `client_lent_en_lecture_perd_la_connexion_a_l_echeance_globale`
    // (audit L6), qui passait pour la mauvaise raison : sa charge de 8 Mio,
    // dupliquée par le résultat (`content[].text` + `structuredContent`),
    // faisait 16 777 454 octets — **au-dessus** du plafond de réponse par
    // défaut (16 777 216). La connexion se fermait sur le refus de taille
    // (`-32603 response size ... exceeds maximum`), pas sur l'échéance.
    //
    // Ici la charge (7 Mio) reste sous le plafond (16 Mio : dupliquée par le
    // résultat, la réponse fait ~14 Mio) et le test vérifie explicitement que
    // le corps n'est **pas** ce refus. Pourquoi 7 Mio et pas 2 : sur Linux,
    // les tampons noyau d'un client qui ne lit pas absorbent ~4 Mio (wmem
    // autotuné ≤ 4 Mio + fenêtre TCP fermée ≈ 212 Kio de rcvbuf) — une
    // réponse de 4 Mio y entrait en entier, aucune écriture ne bloquait, le
    // budget ne se consommait pas et la coupure n'avait jamais lieu (échec CI
    // ubuntu 2026-11-19 : « le client a reçu 2 621 440 octets de corps sur
    // 4 194 533 : aucune coupure »). À ~14 Mio sur le fil, le blocage est
    // garanti sur les trois plateformes.
    //
    // Ce que le test mesure, et pourquoi le côté client ne suffit pas : la
    // durée vue par un lecteur lent est dominée par le drainage des octets
    // déjà remis au noyau (mesuré : 512 Kio à 17 Kio/s = 30 s), pas par
    // l'instant de la coupure. La garantie réelle est **serveur** : le slot
    // de connexion est rendu à l'échéance, pas au `WRITE_TIMEOUT` de 10 s.
    // On l'observe de l'extérieur avec `max_connections(1)` : un second
    // client est refusé (`503`) tant que le premier retient le slot, et servi
    // (`200`) dès qu'il l'a rendu.
    //
    // Sans tranches bornées, le premier `write_vectored` de la réponse est
    // absorbé d'un coup par le noyau (mesuré : 4 194 533 octets en un appel)
    // et le
    // serveur considère la réponse écrite : rien n'est coupé. Sans borne par
    // tentative, l'appel bloqué ne rend la main qu'au `WRITE_TIMEOUT` (10 s).
    // Les deux mécanismes sont donc nécessaires, et le test tombe si l'un
    // manque (rapport interne LB du 2026-11-13).
    let charge = "x".repeat(7 * 1024 * 1024);
    let bound = ServerBuilder::new("lent", "1.0.0")
        .legacy(true)
        .tool("gros", "Grosse réponse", move |_| {
            Ok(json!({ "charge": charge }))
        })
        .read_timeout(Duration::from_millis(300))
        .write_deadline(Duration::from_secs(1))
        .max_connections(1)
        .bind("127.0.0.1:0")
        .expect("bind");
    let addr = bound.local_addr().expect("local_addr");
    std::thread::spawn(move || {
        let _ = bound.serve();
    });

    // Premier client : demande une réponse de ~14 Mio et **ne lit pas** — sa
    // fenêtre TCP reste fermée, l'écriture du serveur se bloque sur le corps.
    let mut lent = TcpStream::connect(addr).expect("connect");
    lent.set_read_timeout(Some(Duration::from_millis(100)))
        .expect("set_read_timeout");
    let body =
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"gros","arguments":{}}}"#;
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let debut = Instant::now();
    lent.write_all(req.as_bytes()).expect("write");

    // Sonde : à quel instant le slot revient-il ?
    let mut rendu = None;
    while debut.elapsed() < Duration::from_secs(12) {
        if slot_rendu(addr) {
            rendu = Some(debut.elapsed());
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let rendu = rendu.expect("le slot n'a jamais été rendu : la connexion n'est pas coupée");

    // Le premier client draine ce qui a été mis en file avant la coupure.
    let mut recu = Vec::new();
    let mut tampon = [0u8; 32 * 1024];
    loop {
        match lent.read(&mut tampon) {
            Ok(0) => break,
            Ok(n) => recu.extend_from_slice(&tampon[..n]),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => panic!("lecture interrompue après {} octets : {e}", recu.len()),
        }
        assert!(
            debut.elapsed() < Duration::from_secs(20),
            "drainage interminable : {} octets reçus",
            recu.len()
        );
    }

    // Distinction explicite des deux causes de fermeture : réponse 200, corps
    // qui n'est pas l'erreur de taille.
    let texte = String::from_utf8_lossy(&recu).into_owned();
    let (tete, corps) = match texte.split_once("\r\n\r\n") {
        Some((tete, corps)) => (tete.to_string(), corps.to_string()),
        None => (texte.clone(), String::new()),
    };
    assert!(
        tete.starts_with("HTTP/1.1 200"),
        "la fermeture ne doit pas venir d'un refus de transport : {tete}"
    );
    assert!(
        !corps.contains("-32603") && !corps.contains("exceeds maximum"),
        "le corps est le refus de taille, pas une réponse tronquée : {:.200}",
        corps
    );
    let longueur: usize = tete
        .lines()
        .find_map(|ligne| ligne.strip_prefix("Content-Length: "))
        .and_then(|valeur| valeur.trim().parse().ok())
        .expect("Content-Length");
    assert!(
        longueur > 13 * 1024 * 1024,
        "la réponse attendue fait ~14 Mio sur le fil (7 Mio dupliqués) : {longueur}"
    );
    assert!(
        rendu < Duration::from_secs(6),
        "slot rendu trop tard ({rendu:?}) : la tentative a bloqué jusqu'au WRITE_TIMEOUT"
    );
    assert!(
        corps.len() < longueur / 2,
        "le client a reçu {} octets de corps sur {longueur} : aucune coupure",
        corps.len()
    );
}

/// Vrai si le serveur accepte une requête neuve (slot de connexion libre) :
/// `GET /health` rend `200` quand le plafond de connexions le permet, `503`
/// quand il est atteint.
fn slot_rendu(addr: SocketAddr) -> bool {
    let Ok(mut flux) = TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else {
        return false;
    };
    if flux
        .set_read_timeout(Some(Duration::from_millis(500)))
        .is_err()
    {
        return false;
    }
    if flux
        .write_all(b"GET /health HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut reponse = String::new();
    let _ = flux.read_to_string(&mut reponse);
    reponse.starts_with("HTTP/1.1 200")
}

/// Serveur à grosse réponse pour le test de plafond d'écriture : `plafond`
/// explicite, ou `None` pour laisser le défaut (`read_timeout` 300 ms → 1 s)
/// couper l'écriture.
fn spawn_gros(plafond: Duration) -> SocketAddr {
    // 4 Mio de charge : le résultat MCP la duplique (`content[].text` +
    // `structuredContent`), soit ~8,4 Mo sur le fil — sous le plafond de
    // réponse de 16 Mio.
    let charge = "x".repeat(4 * 1024 * 1024);
    let builder = ServerBuilder::new("lent-releve", "1.0.0")
        .legacy(true)
        .tool("gros", "Grosse réponse", move |_| {
            Ok(json!({ "charge": charge }))
        })
        .read_timeout(Duration::from_millis(300))
        .write_deadline(plafond);
    let bound = builder.bind("127.0.0.1:0").expect("bind");
    let addr = bound.local_addr().expect("local_addr");
    std::thread::spawn(move || {
        let _ = bound.serve();
    });
    addr
}

fn requete_gros() -> String {
    let body =
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"gros","arguments":{}}}"#;
    format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[test]
fn plafond_d_ecriture_releve_laisse_passer_une_reponse_lente() {
    // A3 (contre-revue) : le défaut (`read_timeout` 300 ms → échéance 1 s)
    // coupe une grosse réponse lue à 32 Kio toutes les 25 ms (~3,2 s) —
    // un client lent mais légitime, servi **en entier** avant L1.
    // `.write_deadline(30 s)` relève le plafond : la réponse passe. Le
    // plafond doit couvrir la cadence du **lecteur le plus lent** : sur les
    // runners macOS, le drain des ~8,4 Mo prend ~14 s (granularité des
    // sleeps + lectures plus petites) contre ~3 s ailleurs — le plafond à
    // 10 s y coupait la réponse à ~6 Mo (échec CI macOS 2026-11-21 :
    // « réponse tronquée : 6 031 543 octets reçus sur ~8,4 Mo »).
    let addr = spawn_gros(Duration::from_secs(30));
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set_read_timeout");
    stream.write_all(requete_gros().as_bytes()).expect("write");

    let start = Instant::now();
    let mut recu = 0usize;
    let mut buf = [0u8; 32 * 1024];
    loop {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "test bloqué après {recu} octets"
        );
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => recu += n,
            Err(e) => panic!(
                "réponse coupée après {recu} octets alors que le plafond est relevé à 10 s : {e}"
            ),
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        recu >= 8 * 1024 * 1024,
        "réponse tronquée : {recu} octets reçus sur ~8,4 Mo"
    );

    // Témoin négatif **non inclus** : sur cette machine, les tampons de
    // socket absorbent les ~8,4 Mo même avec un lecteur lent, donc « le défaut
    // coupe » n'est pas observable de bout en bout. Le contraste « défaut
    // coupe / plafond relevé laisse passer » est verrouillé de manière
    // déterministe au niveau du mécanisme
    // (`ecrire_tous_vecteurs_plafond_releve_laisse_passer_une_ecriture_lente`)
    // et le câblage du builder par `write_deadline_du_builder_remplace_le_defaut`,
    // tous deux dans `src/http.rs`.
}

// ============================================================================
// 16. L1 — `HTTP/1.0` sur les réponses d'erreur globales
// ============================================================================

#[test]
fn http10_error_responses_keep_the_request_version() {
    let (addr, _) = spawn_audit_server();

    // 400 après parsing de la ligne de requête (framing ambigu).
    let req = "POST /mcp HTTP/1.0\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n";
    let (status, head, _) = raw_exchange(addr, req.as_bytes());
    assert_eq!(status, 400, "{head}");
    assert!(
        head.starts_with("HTTP/1.0 400"),
        "version propagée : {head}"
    );

    // 404 route inconnue.
    let req = "GET /inconnue HTTP/1.0\r\nHost: t\r\nConnection: close\r\n\r\n";
    let (status, head, _) = raw_exchange(addr, req.as_bytes());
    assert_eq!(status, 404, "{head}");
    assert!(
        head.starts_with("HTTP/1.0 404"),
        "version propagée : {head}"
    );

    // 405 méthode refusée.
    let req = "GET /mcp HTTP/1.0\r\nHost: t\r\nConnection: close\r\n\r\n";
    let (status, head, _) = raw_exchange(addr, req.as_bytes());
    assert_eq!(status, 405, "{head}");
    assert!(
        head.starts_with("HTTP/1.0 405"),
        "version propagée : {head}"
    );
}

#[test]
fn http10_503_refusal_stays_http11_because_the_version_is_unknown() {
    // Le refus de saturation part **avant** toute lecture : la version de la
    // requête n'est pas connue au moment d'écrire la ligne de statut. La
    // réponse reste donc `HTTP/1.1` — limite documentée (docs/SECURITE.md),
    // et c'est le seul cas où la version ne peut pas être propagée.
    let bound = ServerBuilder::new("cap10", "1.0.0")
        .legacy(true)
        .tool("ok", "Répond ok", |_| Ok(json!({ "ok": true })))
        .read_timeout(Duration::from_millis(300))
        .max_connections(1)
        .bind("127.0.0.1:0")
        .expect("bind");
    let addr = bound.local_addr().expect("local_addr");
    std::thread::spawn(move || {
        let _ = bound.serve();
    });

    let first = TcpStream::connect(addr).expect("connect 1"); // occupe l'unique slot
    std::thread::sleep(Duration::from_millis(150));
    let req = "POST /mcp HTTP/1.0\r\nHost: t\r\nContent-Length: 2\r\n\r\n{}";
    let (status, head, body) = raw_exchange(addr, req.as_bytes());
    assert_eq!(status, 503, "{head}");
    assert!(
        head.starts_with("HTTP/1.1 503"),
        "refus avant lecture : version inconnue, HTTP/1.1 documenté : {head}"
    );
    assert!(body.contains("too many connections"), "{body:?}");
    drop(first);
}
