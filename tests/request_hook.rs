//! Tests du verrou de requête (`ServerBuilder::on_request`) : auth par
//! token, rate-limit, exemptions (`/health`, routes inconnues), contexte
//! vu par le hook, assainissement des en-têtes de rejet, panique contenue,
//! hygiène de la rétention sur keep-alive.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const CALL_BODY: &str = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"add","arguments":{"a":2,"b":3}}}"#;

/// Démarre un serveur sur un port libre et rend l'adresse effective.
fn start(build: impl FnOnce(litemcp::ServerBuilder) -> litemcp::ServerBuilder) -> SocketAddr {
    let bound =
        build(
            litemcp::ServerBuilder::new("t", "0.1.0").tool("add", "Additionne", |args| {
                let a = args["a"].as_i64().unwrap_or(0);
                let b = args["b"].as_i64().unwrap_or(0);
                Ok(serde_json::json!({ "somme": a + b }))
            }),
        )
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = bound.local_addr().unwrap();
    std::thread::spawn(move || bound.serve().unwrap());
    addr
}

/// POST complet (Connection: close) — rend la réponse brute, jusqu'à EOF.
fn post(addr: SocketAddr, target: &str, headers: &[(&str, &str)], body: &str) -> String {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut request = format!("POST {target} HTTP/1.1\r\nHost: t\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    raw
}

/// Lit **une** réponse HTTP complète (en-têtes + corps selon
/// `Content-Length`) — pour les connexions keep-alive qui ne ferment pas.
fn read_one_response(stream: &mut TcpStream) -> String {
    let mut raw = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).unwrap();
        raw.push(byte[0]);
        if raw.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&raw).to_string();
    let len: usize = head
        .split("\r\n")
        .find_map(|line| {
            line.strip_prefix("Content-Length: ")
                .map(str::parse::<usize>)
        })
        .transpose()
        .ok()
        .flatten()
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).unwrap();
    format!("{head}{}", String::from_utf8_lossy(&body))
}

#[test]
fn bearer_gate_lets_valid_requests_through_and_rejects_others() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let addr = start(|b| {
        b.on_request(move |ctx| {
            match ctx
                .header("Authorization")
                .and_then(|v| v.strip_prefix("Bearer "))
            {
                Some("valid") => Ok(()),
                _ => Err(litemcp::RequestRejection::unauthorized(
                    "missing or invalid bearer token",
                )),
            }
        })
        .on_call(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        })
    });

    // Nom d'en-tête insensible à la casse : `authorization` minuscule passe.
    let ok = post(
        addr,
        "/mcp",
        &[("authorization", "Bearer valid")],
        CALL_BODY,
    );
    assert!(
        ok.contains("HTTP/1.1 200 OK"),
        "token valide accepté : {ok}"
    );
    assert!(ok.contains("\"somme\":5"), "le handler a tourné : {ok}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // Token invalide → 401 avec le message du hook, handler jamais appelé.
    let ko = post(addr, "/mcp", &[("Authorization", "Bearer nope")], CALL_BODY);
    assert!(
        ko.contains("HTTP/1.1 401 Unauthorized"),
        "token invalide rejeté : {ko}"
    );
    assert!(
        ko.contains("missing or invalid bearer token"),
        "message du hook dans le corps : {ko}"
    );

    // Absence d'en-tête → 401 aussi.
    let none = post(addr, "/mcp", &[], CALL_BODY);
    assert!(
        none.contains("HTTP/1.1 401 Unauthorized"),
        "sans Authorization : 401 : {none}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "aucun rejet ne doit exécuter le handler"
    );
}

#[test]
fn health_probe_and_unknown_route_bypass_the_gate() {
    let addr =
        start(|b| b.on_request(|_ctx| Err(litemcp::RequestRejection::forbidden("gate closed"))));

    // Sonde de vie : jamais soumise au verrou (probes k8s sans auth).
    let mut probe = TcpStream::connect(addr).unwrap();
    probe
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    probe
        .write_all(b"GET /health HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut raw = String::new();
    probe.read_to_string(&mut raw).unwrap();
    assert!(
        raw.contains("HTTP/1.1 200 OK") && raw.contains("ok"),
        "GET /health exempt du verrou : {raw}"
    );

    // Route inconnue : 404 de la lib, pas le verdict du hook.
    let unknown = post(addr, "/elsewhere", &[], "");
    assert!(
        unknown.contains("HTTP/1.1 404 Not Found"),
        "route inconnue : 404 avant le hook : {unknown}"
    );
    assert!(
        !unknown.contains("gate closed"),
        "le hook ne doit pas être consulté sur une route inconnue : {unknown}"
    );
}

#[test]
fn hook_sees_method_path_query_peer_and_headers() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    let addr = start(|b| {
        b.on_request(move |ctx| {
            let mut record = format!(
                "{}|{}|{:?}|{}|{:?}",
                ctx.method,
                ctx.path,
                ctx.query,
                ctx.peer,
                ctx.header("x-marker")
            );
            if let Some(value) = ctx.header("X-Marker") {
                record.push_str(&format!("|case-insensitive={value}"));
            }
            recorder.lock().unwrap().push(record);
            Ok(())
        })
    });

    post(addr, "/mcp?x=1", &[("X-Marker", "abc")], CALL_BODY);

    let records = seen.lock().unwrap();
    assert_eq!(records.len(), 1, "le hook a vu exactement une requête");
    let record = &records[0];
    assert!(
        record.starts_with("POST|/mcp|Some(\"x=1\")|"),
        "méthode, chemin sans query et query séparés : {record}"
    );
    let peer_part = record.split('|').nth(3).unwrap();
    assert!(
        peer_part.starts_with("127.0.0.1:"),
        "le pair est une adresse TCP locale : {record}"
    );
    assert!(
        record.contains("Some(\"abc\")"),
        "valeur d'en-tête visible par recherche insensible à la casse : {record}"
    );
    assert!(
        record.contains("case-insensitive=abc"),
        "deux orthographes du même nom trouvent la valeur : {record}"
    );
}

#[test]
fn rate_limited_rejection_sets_status_and_retry_after() {
    let counter = Arc::new(AtomicUsize::new(0));
    let quota = Arc::clone(&counter);
    let addr = start(|b| {
        b.on_request(move |_ctx| {
            if quota.fetch_add(1, Ordering::SeqCst) >= 2 {
                Err(litemcp::RequestRejection::rate_limited(Some(
                    Duration::from_millis(1500),
                )))
            } else {
                Ok(())
            }
        })
    });

    let first = post(addr, "/mcp", &[], CALL_BODY);
    let second = post(addr, "/mcp", &[], CALL_BODY);
    let third = post(addr, "/mcp", &[], CALL_BODY);
    assert!(
        first.contains("HTTP/1.1 200 OK"),
        "1re requête passe : {first}"
    );
    assert!(
        second.contains("HTTP/1.1 200 OK"),
        "2e requête passe : {second}"
    );
    assert!(
        third.contains("HTTP/1.1 429 Too Many Requests"),
        "3e requête rejetée : {third}"
    );
    assert!(
        third.contains("Retry-After: 2"),
        "1 500 ms arrondis au dessus → Retry-After: 2 : {third}"
    );
    assert!(
        third.contains("rate limit exceeded"),
        "message par défaut du 429 : {third}"
    );
}

#[test]
fn hook_panic_becomes_500_and_server_continues() {
    let first = Arc::new(AtomicBool::new(true));
    let flag = Arc::clone(&first);
    let addr = start(|b| {
        b.on_request(move |_ctx| {
            if flag.swap(false, Ordering::SeqCst) {
                panic!("boom volontaire du hook");
            }
            Ok(())
        })
    });

    let panicked = post(addr, "/mcp", &[], CALL_BODY);
    assert!(
        panicked.contains("HTTP/1.1 500 Internal Server Error"),
        "panique du hook → 500 : {panicked}"
    );
    assert!(
        panicked.contains("internal error (request hook panicked)"),
        "message générique (aucun contenu de panique réfléchi) : {panicked}"
    );

    let recovered = post(addr, "/mcp", &[], CALL_BODY);
    assert!(
        recovered.contains("HTTP/1.1 200 OK"),
        "le serveur continue après la panique du hook : {recovered}"
    );
    assert!(
        recovered.contains("\"somme\":5"),
        "le handler tourne toujours : {recovered}"
    );
}

#[test]
fn rejection_headers_are_forwarded_and_injection_attempts_dropped() {
    let addr = start(|b| {
        b.on_request(|_ctx| {
            Err(litemcp::RequestRejection::unauthorized("no")
                .with_header("WWW-Authenticate", "Bearer realm=\"mcp\"")
                .with_header("X-Evil", "pwned\r\nX-Root: yes"))
        })
    });

    let response = post(addr, "/mcp", &[], CALL_BODY);
    assert!(
        response.contains("WWW-Authenticate: Bearer realm=\"mcp\""),
        "en-tête légitime transmis : {response}"
    );
    assert!(
        !response.contains("X-Evil") && !response.contains("X-Root"),
        "valeur à CRLF écartée (aucune injection) : {response}"
    );
    assert!(
        response.contains("HTTP/1.1 401 Unauthorized"),
        "le statut reste intact malgré l'en-tête invalidé : {response}"
    );
}

#[test]
fn gate_runs_before_method_check() {
    let addr =
        start(|b| b.on_request(|_ctx| Err(litemcp::RequestRejection::unauthorized("locked"))));

    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(b"GET /mcp HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    assert!(
        raw.contains("HTTP/1.1 401 Unauthorized"),
        "auth avant le verdict de méthode (pas de 405) : {raw}"
    );
}

#[test]
fn headers_are_not_retained_across_keep_alive_requests() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    let addr = start(|b| {
        b.on_request(move |ctx| {
            recorder
                .lock()
                .unwrap()
                .push(ctx.header("X-Marker").map(str::to_string));
            Ok(())
        })
    });

    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    for marker in ["m1", "m2"] {
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: t\r\nX-Marker: {marker}\r\nContent-Length: {}\r\n\r\n{CALL_BODY}",
            CALL_BODY.len()
        );
        stream.write_all(request.as_bytes()).unwrap();
        let response = read_one_response(&mut stream);
        assert!(
            response.contains("HTTP/1.1 200 OK"),
            "requête {marker} servie en keep-alive : {response}"
        );
    }
    let records = seen.lock().unwrap();
    assert_eq!(
        *records,
        vec![Some("m1".to_string()), Some("m2".to_string())],
        "chaque requête ne voit que SES en-têtes (pas de rémanence m1 sur m2)"
    );
}

#[test]
fn multiple_hooks_run_in_order_and_first_rejection_wins() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&order);
    let addr = start(|b| {
        b.on_request({
            let recorder = Arc::clone(&recorder);
            move |_ctx| {
                recorder.lock().unwrap().push("a");
                Ok(())
            }
        })
        .on_request({
            let recorder = Arc::clone(&recorder);
            move |_ctx| {
                recorder.lock().unwrap().push("b");
                Err(litemcp::RequestRejection::forbidden("b said no"))
            }
        })
        .on_request({
            let recorder = Arc::clone(&recorder);
            move |_ctx| {
                recorder.lock().unwrap().push("c");
                Ok(())
            }
        })
    });

    let response = post(addr, "/mcp", &[], CALL_BODY);
    assert!(
        response.contains("HTTP/1.1 403 Forbidden"),
        "le premier rejet gagne : {response}"
    );
    assert_eq!(
        *order.lock().unwrap(),
        vec!["a", "b"],
        "le hook c n'est pas exécuté après le rejet de b"
    );
}

#[test]
fn rejection_status_outside_4xx_5xx_is_clamped_to_400() {
    let addr =
        start(|b| b.on_request(|_ctx| Err(litemcp::RequestRejection::new(200, "not a rejection"))));

    let response = post(addr, "/mcp", &[], CALL_BODY);
    assert!(
        response.contains("HTTP/1.1 400 Bad Request"),
        "statut 200 ramené à 400 : {response}"
    );
    assert!(
        response.contains("not a rejection"),
        "le message reste celui du hook : {response}"
    );
}

#[test]
fn notifications_pass_through_the_gate() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let addr = start(|b| {
        b.on_request(move |_ctx| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    });

    let response = post(
        addr,
        "/mcp",
        &[],
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    );
    assert!(
        response.contains("HTTP/1.1 202 Accepted"),
        "notification servie : {response}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "les notifications passent par le verrou comme les requêtes"
    );
}
