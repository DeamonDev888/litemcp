//! Fuzz du transport HTTP (proptest) — trois propriétés, sur un vrai
//! serveur (sockets réelles, timeouts courts) :
//!
//! 1. **octets arbitraires** : le serveur répond uniquement des réponses
//!    HTTP **complètes et correctement cadrées** (status line, en-têtes,
//!    `Content-Length` exact — les intermédiaires `100 Continue` exceptés),
//!    ou ne répond rien — jamais d'octet orphelin ;
//! 2. **requêtes « réalistes »** (verbes/chemins/en-têtes/pièges connus,
//!    `Content-Length` menteurs, fins de ligne mixtes) : même invariant,
//!    plus la survie du serveur après chaque cas ;
//! 3. **enveloppes JSON-RPC** : à toute requête valide répond exactement
//!    une réponse JSON-RPC bien formée (`jsonrpc: "2.0"` + `result`|`error`).
//!
//! `proptest` est une **dev-dépendance** : le cœur du crate ne gagne rien.

use litemcp::ServerBuilder;
use proptest::prelude::*;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

fn server() -> SocketAddr {
    static ADDR: std::sync::OnceLock<SocketAddr> = std::sync::OnceLock::new();
    *ADDR.get_or_init(|| {
        let bound = ServerBuilder::new("fuzz", "0.1.0")
            .tool("ok", "répond ok", |_args| {
                Ok(serde_json::json!({ "ok": true }))
            })
            .read_timeout(Duration::from_millis(120))
            .bind("127.0.0.1:0")
            .expect("bind fuzz");
        let addr = bound.local_addr().expect("local_addr fuzz");
        std::thread::spawn(move || {
            let _ = bound.serve();
        });
        addr
    })
}

/// Envoie des octets, lit jusqu'à EOF/timeout/RST — rend ce qui est arrivé.
fn exchange(addr: SocketAddr, bytes: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout client");
    // Le serveur peut fermer avant la fin de l'écriture : on ignore.
    let _ = stream.write_all(bytes);
    let mut out = Vec::new();
    let _ = stream.read_to_end(&mut out);
    out
}

/// Invariant de cadrage : tout ce qui est émis est une **suite de réponses
/// HTTP complètes** — status line, en-têtes terminés, `Content-Length`
/// exact — sans octet orphelin. Les intermédiaires `100 Continue` n'ont
/// ni `Content-Length` ni corps (RFC 9110 §15.2.1).
fn assert_only_complete_responses(bytes: &[u8]) {
    let mut rest = bytes;
    while !rest.is_empty() {
        let text =
            std::str::from_utf8(rest).unwrap_or_else(|_| panic!("réponse non UTF-8 : {rest:?}"));
        // La ligne de statut reflète la version de la requête quand elle
        // est `HTTP/1.0` ou `HTTP/1.1` (audit 2026-10-27, m4 — un client
        // HTTP/1.0 strict refuse une réponse annoncée `HTTP/1.1`, test de
        // régression `m4_ligne_de_statut_reflete_la_version_http`) : les
        // deux préfixes sont valides. L'invariant testé ici est le
        // **cadrage** (en-têtes terminés, `Content-Length` exact, aucun
        // octet orphelin), pas l'épinglage de version.
        assert!(
            text.starts_with("HTTP/1.1 ") || text.starts_with("HTTP/1.0 "),
            "status line : {text:?}"
        );
        let head_end = text.find("\r\n\r\n").expect("en-têtes terminés") + 4;
        if text.starts_with("HTTP/1.1 100 ") {
            rest = &rest[head_end..];
            continue;
        }
        let head = &text[..head_end];
        let content_length = head
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
            .and_then(|line| line.split(':').nth(1))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .expect("Content-Length sur toute réponse finale");
        assert!(
            rest.len() >= head_end + content_length,
            "corps tronqué : {content_length} octets attendus, {} disponibles",
            rest.len() - head_end
        );
        rest = &rest[head_end + content_length..];
    }
}

/// Le serveur est vivant : une sonde propre répond 200.
fn assert_alive(addr: SocketAddr) {
    let response = exchange(
        addr,
        b"GET /health HTTP/1.1\r\nHost: f\r\nConnection: close\r\n\r\n",
    );
    let text = String::from_utf8_lossy(&response);
    assert!(
        text.contains("200 OK"),
        "le serveur doit survivre : {text:?}"
    );
}

proptest! {
    // 512 octets de bruit pur par cas : l'UTF-8 invalide doit couper propre,
    // tout le reste doit produire au pire une erreur cadrée.
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn arbitrary_bytes_only_yield_complete_responses(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let addr = server();
        let response = exchange(addr, &bytes);
        assert_only_complete_responses(&response);
        assert_alive(addr);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn request_like_garbage_only_yields_complete_responses(
        (verb, path, version, sep, headers, body) in request_like(),
    ) {
        let addr = server();
        let request = format!("{verb}{sep}{path}{sep}{version}\r\n{headers}\r\n\r\n{body}");
        let response = exchange(addr, request.as_bytes());
        assert_only_complete_responses(&response);
        assert_alive(addr);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn any_jsonrpc_request_gets_one_well_formed_envelope(
        (method, id, params) in jsonrpc_body(),
    ) {
        let addr = server();
        let body = format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}"{params}}}"#);
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: f\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let response = exchange(addr, request.as_bytes());
        assert_only_complete_responses(&response);

        // `Connection: close` : exactement UNE réponse, corps = enveloppe
        // JSON-RPC valide (jsonrpc 2.0 + result|error, code numérique).
        let text = std::str::from_utf8(&response).expect("réponse ASCII/UTF-8");
        let head_end = text.find("\r\n\r\n").expect("en-têtes") + 4;
        let status = &text[..text.find("\r\n").unwrap()];
        prop_assert!(status.contains(" 200 "), "statut : {status}");
        let payload: serde_json::Value =
            serde_json::from_str(text[head_end..].trim()).expect("corps JSON valide");
        prop_assert_eq!(&payload["jsonrpc"], &serde_json::json!("2.0"), "{:?}", payload);
        prop_assert!(
            payload.get("result").is_some() || payload.get("error").is_some(),
            "result|error attendu : {:?}",
            payload
        );
        if let Some(error) = payload.get("error") {
            prop_assert!(error["code"].is_i64(), "code numérique : {:?}", payload);
            prop_assert!(error["message"].is_string(), "message présent : {:?}", payload);
        }
    }
}

/// Requête « réaliste » : verbes/chemins plausibles, en-têtes piégeux
/// (duplications, obs-fold, TE, CL menteurs), séparateurs et versions
/// valides ou cassées, corps variable.
fn request_like() -> impl Strategy<
    Value = (
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        String,
        String,
    ),
> {
    let verb = prop_oneof![
        Just("POST"),
        Just("GET"),
        Just("OPTIONS"),
        Just("PUT"),
        Just("DELETE"),
        Just("post"),
    ];
    let path = prop_oneof![
        Just("/mcp"),
        Just("/mcp?x=1"),
        Just("/health"),
        Just("/"),
        Just("//mcp"),
        Just("/autre"),
        Just("/mcp%2f"),
    ];
    let version = prop_oneof![
        Just("HTTP/1.1"),
        Just("HTTP/1.0"),
        Just("HTTP/2.0"),
        Just("http/1.1"),
    ];
    let sep = prop_oneof![Just(" "), Just(" "), Just(" "), Just("\t"), Just("  ")];
    let headers = proptest::collection::vec(
        prop_oneof![
            Just("Host: f"),
            Just("Host: f\r\nHost: g"),
            Just("Content-Type: application/json"),
            Just("Content-Type: application/json\r\nContent-Type: text/plain"),
            Just("Content-Length: 2"),
            Just("Content-Length: abc"),
            Just("Content-Length: -1"),
            Just("Content-Length: 1048576"),
            Just("Transfer-Encoding: chunked"),
            Just("Origin: https://x.example"),
            Just("Accept: text/event-stream"),
            Just("Accept: application/json, text/event-stream"),
            Just("Connection: close"),
            Just("Connection: keep-alive"),
            Just("X-Weird-Without-Colon"),
            Just("X-Space-Before-Colon : v"),
            Just(" X-Obs-Fold: v"),
        ],
        0..5,
    )
    .prop_map(|lines| lines.join("\r\n"));
    let body = prop_oneof![
        Just(String::new()),
        Just("{}".to_string()),
        Just(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#.to_string()),
        Just(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"ok","arguments":{}}}"#.to_string()),
        Just("garbage pas json".to_string()),
        Just("[1, 2, 3]".to_string()),
    ];
    (verb, path, version, sep, headers, body)
}

/// Corps JSON-RPC : méthodes connues/inconnues, ids ambigus, params
/// valides/invalides, versions de protocole explicites.
fn jsonrpc_body() -> impl Strategy<Value = (&'static str, &'static str, &'static str)> {
    let method = prop_oneof![
        Just("ping"),
        Just("tools/list"),
        Just("tools/call"),
        Just("server/discover"),
        Just("initialize"),
        Just("resources/list"),
        Just("prompts/get"),
        Just("methode/inconnue"),
    ];
    let id = prop_oneof![
        Just("1"),
        Just("\"abc\""),
        Just("null"),
        Just("true"),
        Just("-7"),
        Just("18446744073709551615"),
    ];
    let params = prop_oneof![
        Just(""),
        Just(r#","params":{}"#),
        Just(r#","params":{"name":"ok","arguments":{}}"#),
        Just(r#","params":[1,2]"#),
        Just(r#","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"1999-01-01"}}"#),
    ];
    (method, id, params)
}
