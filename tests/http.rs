//! Tests d'intégration du transport HTTP stateless — de vraies sockets.

use litemcp::ServerBuilder;
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};

fn spawn_server() -> SocketAddr {
    let bound = ServerBuilder::new("calc", "0.1.0")
        .tool("add", "Additionne deux nombres", |args| {
            let a = args.get("a").and_then(Value::as_i64).ok_or("a manquant")?;
            let b = args.get("b").and_then(Value::as_i64).ok_or("b manquant")?;
            Ok(serde_json::json!({ "somme": a + b }))
        })
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = bound.local_addr().unwrap();
    std::thread::spawn(move || bound.serve().unwrap());
    addr
}

fn parse(raw: &str) -> (u16, String) {
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw, ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (status, body.to_string())
}

fn raw_request(addr: SocketAddr, request: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    parse(&raw)
}

fn post(addr: SocketAddr, body: &str, accept: &str, origin: Option<&str>) -> (u16, String) {
    let origin_header = origin
        .map(|o| format!("Origin: {o}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: test\r\nAccept: {accept}\r\n{origin_header}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    raw_request(addr, &request)
}

#[test]
fn initialize_and_tools_list() {
    let addr = spawn_server();

    let (status, body) = post(
        addr,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        "application/json",
        None,
    );
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["result"]["protocolVersion"], "2026-07-28");
    assert_eq!(v["result"]["serverInfo"]["name"], "calc");

    let (status, body) = post(
        addr,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        "application/json",
        None,
    );
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["id"], 2);
    assert_eq!(v["result"]["tools"][0]["name"], "add");
    assert_eq!(v["result"]["tools"][0]["inputSchema"]["type"], "object");
}

#[test]
fn tools_call_computes_structured_content() {
    let addr = spawn_server();
    let (status, body) = post(
        addr,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"add","arguments":{"a":5,"b":7}}}"#,
        "application/json",
        None,
    );
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["id"], 3);
    assert_eq!(v["result"]["structuredContent"]["somme"], 12);
    assert!(v["result"].get("isError").is_none());
    assert_eq!(v["result"]["content"][0]["type"], "text");
}

#[test]
fn tool_error_becomes_is_error_result() {
    let addr = spawn_server();
    let (status, body) = post(
        addr,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"add","arguments":{}}}"#,
        "application/json",
        None,
    );
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["result"]["isError"], true);
    assert_eq!(v["result"]["content"][0]["text"], "a manquant");
}

#[test]
fn unknown_tool_is_invalid_params() {
    let addr = spawn_server();
    let (status, body) = post(
        addr,
        r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"nope"}}"#,
        "application/json",
        None,
    );
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], -32602);
}

#[test]
fn unknown_method_is_method_not_found() {
    let addr = spawn_server();
    let (status, body) = post(
        addr,
        r#"{"jsonrpc":"2.0","id":6,"method":"resources/list"}"#,
        "application/json",
        None,
    );
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], -32601);
}

#[test]
fn ping_returns_empty_object() {
    let addr = spawn_server();
    let (status, body) = post(
        addr,
        r#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#,
        "application/json",
        None,
    );
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["result"], serde_json::json!({}));
}

#[test]
fn malformed_json_gets_parse_error_with_null_id() {
    let addr = spawn_server();
    let (status, body) = post(addr, "not json", "application/json", None);
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], -32700);
    assert_eq!(v["id"], Value::Null);
}

#[test]
fn notification_gets_202_with_empty_body() {
    let addr = spawn_server();
    let (status, body) = post(
        addr,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        "application/json",
        None,
    );
    assert_eq!(status, 202);
    assert!(body.is_empty());
}

#[test]
fn get_is_refused_with_405() {
    let addr = spawn_server();
    let (status, _) = raw_request(
        addr,
        "GET /mcp HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status, 405);
}

#[test]
fn unknown_path_is_404() {
    let addr = spawn_server();
    let (status, _) = post(
        addr,
        r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
        "application/json",
        None,
    );
    assert_eq!(status, 200);
    let request =
        "POST /autre HTTP/1.1\r\nHost: test\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
    let (status, _) = raw_request(addr, request);
    assert_eq!(status, 404);
}

#[test]
fn missing_content_length_is_411() {
    let addr = spawn_server();
    let (status, _) = raw_request(
        addr,
        "POST /mcp HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status, 411);
}

#[test]
fn origin_refused_by_default() {
    let addr = spawn_server();
    let (status, _) = post(
        addr,
        r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
        "application/json",
        Some("https://evil.example"),
    );
    assert_eq!(status, 403);
}

#[test]
fn sse_when_client_accepts_only_event_stream() {
    let addr = spawn_server();
    let (status, body) = post(
        addr,
        r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"add","arguments":{"a":1,"b":2}}}"#,
        "text/event-stream",
        None,
    );
    assert_eq!(status, 200);
    let payload = body.strip_prefix("data: ").unwrap_or(&body);
    let v: Value = serde_json::from_str(payload.trim()).unwrap();
    assert_eq!(v["result"]["structuredContent"]["somme"], 3);
}

#[test]
fn sse_response_carries_anti_buffering_headers() {
    // Contrat anti-tampon : tout événement SSE porte `X-Accel-Buffering: no`
    // (honoré par nginx par réponse) et `Cache-Control: no-cache, no-transform`.
    let addr = spawn_hardened();
    let body = r#"{"jsonrpc":"2.0","id":8,"method":"ping"}"#;
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nAccept: text/event-stream\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    assert!(raw.contains("HTTP/1.1 200 OK"), "{raw:?}");
    assert!(raw.contains("Content-Type: text/event-stream"), "{raw:?}");
    assert!(
        raw.contains("X-Accel-Buffering: no"),
        "en-tête anti-tampon nginx manquant : {raw:?}"
    );
    assert!(
        raw.contains("Cache-Control: no-cache, no-transform"),
        "{raw:?}"
    );
}

#[test]
fn json_response_has_no_anti_buffering_headers() {
    // Le JSON n'a rien à tamponner : pas d'en-têtes SSE superflus.
    let addr = spawn_hardened();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    assert!(raw.contains("HTTP/1.1 200 OK"), "{raw:?}");
    assert!(
        !raw.contains("X-Accel-Buffering"),
        "en-têtes SSE sur une réponse JSON : {raw:?}"
    );
    assert!(!raw.contains("text/event-stream"), "{raw:?}");
}

// --- Outils typés : schéma dérivé + validation avant exécution ---

fn spawn_typed_server() -> SocketAddr {
    use litemcp::zod;

    #[derive(serde::Deserialize)]
    struct SearchArgs {
        query: String,
        limit: Option<u32>,
    }

    let bound = ServerBuilder::new("recherche", "0.2.0")
        .tool_parse(
            "search",
            "Recherche indexée",
            zod::object()
                .field("query", zod::string().min(2).max(64))
                .opt("limit", zod::integer().min(1).max(100))
                .strict(),
            |args: SearchArgs| {
                let limit = args.limit.unwrap_or(10);
                Ok(serde_json::json!({ "query": args.query, "limit": limit }))
            },
        )
        .tool_typed("clock", "Heure courante", zod::object(), |_args| {
            Ok(serde_json::json!({ "now": "2026-10-22T00:00:00Z" }))
        })
        .annotate(
            "clock",
            litemcp::protocol::ToolAnnotations::new()
                .title("Horloge")
                .read_only(true)
                .idempotent(true),
        )
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = bound.local_addr().unwrap();
    std::thread::spawn(move || bound.serve().unwrap());
    addr
}

#[test]
fn typed_tool_derives_schema_and_annotations_in_tools_list() {
    let addr = spawn_typed_server();
    let (status, body) = post(
        addr,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        "application/json",
        None,
    );
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    let tools = v["result"]["tools"].as_array().unwrap();

    let search = tools.iter().find(|t| t["name"] == "search").unwrap();
    let schema = &search["inputSchema"];
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["properties"]["query"]["type"], "string");
    assert_eq!(schema["properties"]["query"]["minLength"], 2);
    assert_eq!(schema["properties"]["query"]["maxLength"], 64);
    assert_eq!(schema["properties"]["limit"]["type"], "integer");
    assert_eq!(schema["properties"]["limit"]["maximum"], 100);
    assert_eq!(schema["required"], serde_json::json!(["query"]));
    assert_eq!(schema["additionalProperties"], false);

    let clock = tools.iter().find(|t| t["name"] == "clock").unwrap();
    assert_eq!(clock["annotations"]["title"], "Horloge");
    assert_eq!(clock["annotations"]["readOnlyHint"], true);
    assert_eq!(clock["annotations"]["idempotentHint"], true);
    assert!(clock["annotations"].get("destructiveHint").is_none());
}

#[test]
fn typed_tool_executes_valid_arguments() {
    let addr = spawn_typed_server();
    let (status, body) = post(
        addr,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search","arguments":{"query":"rust mcp","limit":5}}}"#,
        "application/json",
        None,
    );
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["result"]["structuredContent"]["query"], "rust mcp");
    assert_eq!(v["result"]["structuredContent"]["limit"], 5);
    assert!(v["result"].get("isError").is_none());
}

#[test]
fn typed_tool_rejects_invalid_arguments_without_running_handler() {
    let addr = spawn_typed_server();
    // query trop court + champ inconnu (objet strict) : -32602 avec issues
    let (status, body) = post(
        addr,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search","arguments":{"query":"r","extra":1}}}"#,
        "application/json",
        None,
    );
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], -32602);
    let issues = v["error"]["data"]["issues"].as_array().unwrap();
    assert!(issues.iter().any(|i| i.as_str().unwrap().contains("query")));
    assert!(issues.iter().any(|i| i.as_str().unwrap().contains("extra")));

    // limite hors bornes : rejetée aussi
    let (_, body) = post(
        addr,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"search","arguments":{"query":"ok","limit":1000}}}"#,
        "application/json",
        None,
    );
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], -32602);

    // champ requis manquant : rejeté
    let (_, body) = post(
        addr,
        r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"search","arguments":{}}}"#,
        "application/json",
        None,
    );
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], -32602);
    assert!(
        v["error"]["data"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i.as_str().unwrap().contains("requis"))
    );
}

// --- Phase 3 : durcissement HTTP (timeouts, limites, 100-continue) ---

fn spawn_hardened() -> SocketAddr {
    let bound = ServerBuilder::new("durci", "0.2.0")
        .tool("ok", "Répond ok", |_args| {
            Ok(serde_json::json!({ "ok": true }))
        })
        .read_timeout(std::time::Duration::from_millis(400))
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = bound.local_addr().unwrap();
    std::thread::spawn(move || bound.serve().unwrap());
    addr
}

#[test]
fn slowloris_is_disconnected_without_response() {
    let addr = spawn_hardened();
    let mut stream = TcpStream::connect(addr).unwrap();
    // Le client n'envoie qu'un fragment de requête, puis se tait.
    stream
        .write_all(b"POST /mcp HTTP/1.1\r\nHost: t\r\n")
        .unwrap();
    let _ = stream.flush();
    std::thread::sleep(std::time::Duration::from_millis(900)); // > timeout (400 ms)
    let mut raw = String::new();
    let result = stream.read_to_string(&mut raw);
    // Le serveur doit avoir coupé : EOF (0 octet) ou erreur — jamais de réponse.
    let closed = match result {
        Ok(n) => n == 0,
        Err(_) => true,
    };
    assert!(closed, "le serveur aurait dû déconnecter le slowloris");
}

#[test]
fn excessive_headers_get_431() {
    let addr = spawn_hardened();
    let mut request = String::from("POST /mcp HTTP/1.1\r\nHost: t\r\n");
    for i in 0..80 {
        request.push_str(&format!("X-Pad-{i}: y\r\n"));
    }
    request.push_str("Content-Length: 2\r\nConnection: close\r\n\r\n{}");
    let (status, _) = raw_request(addr, &request);
    assert_eq!(status, 431);
}

#[test]
fn oversized_line_gets_431() {
    let addr = spawn_hardened();
    let long_path = "a".repeat(20_000); // > MAX_LINE (16 KiB)
    let request = format!("POST /{long_path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n");
    let (status, _) = raw_request(addr, &request);
    assert_eq!(status, 431);
}

#[test]
fn expect_100_continue_receives_interim_then_final_response() {
    let addr = spawn_hardened();
    let mut stream = TcpStream::connect(addr).unwrap();
    let head = "POST /mcp HTTP/1.1\r\nHost: t\r\nAccept: application/json\r\nExpect: 100-continue\r\nContent-Type: application/json\r\nContent-Length: 40\r\n\r\n";
    stream.write_all(head.as_bytes()).unwrap();
    // Lire l'intermédiaire 100 Continue avant d'envoyer le corps.
    let mut buf = [0u8; 512];
    let mut interim = Vec::new();
    loop {
        let n = stream.read(&mut buf).unwrap();
        if n == 0 {
            break; // EOF : le serveur a fermé sans intermédiaire
        }
        interim.extend_from_slice(&buf[..n]);
        if interim.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let interim_str = String::from_utf8_lossy(&interim);
    assert!(
        interim_str.starts_with("HTTP/1.1 100 Continue"),
        "intermédiaire attendu, reçu : {interim_str:?}"
    );
    let body = r#"{"jsonrpc":"2.0","id":9,"method":"ping"}"#;
    assert_eq!(body.len(), 40); // cohérent avec Content-Length ci-dessus
    stream.write_all(body.as_bytes()).unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    assert!(raw.contains("HTTP/1.1 200 OK"), "réponse finale : {raw:?}");
    assert!(raw.contains("\"result\":{}"));
}

#[test]
fn keep_alive_serves_multiple_requests_on_one_connection() {
    let addr = spawn_hardened();
    let mut stream = TcpStream::connect(addr).unwrap();
    let ping_body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    let ok_body =
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"ok","arguments":{}}}"#;
    let mut pipeline = String::new();
    for body in [ping_body, ok_body] {
        pipeline.push_str(&format!(
            "POST /mcp HTTP/1.1\r\nHost: t\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ));
    }
    stream.write_all(pipeline.as_bytes()).unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    assert_eq!(
        raw.matches("HTTP/1.1 200 OK").count(),
        2,
        "2 réponses keep-alive : {raw:?}"
    );
    assert!(raw.contains("\"id\":1"));
    assert!(raw.contains("\"id\":2"));
}

// --- Corrections issues de l'audit (revue indépendante) ---

#[test]
fn error_path_closes_connection_no_desync_possible() {
    // 404 pré-corps : réponse + fermeture systématique — les octets du corps
    // non lu ne peuvent JAMAIS être réinterprétés (connexion fermée).
    let addr = spawn_hardened();
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .write_all(b"POST /autre HTTP/1.1\r\nHost: t\r\nContent-Length: 6\r\n\r\nbuffer")
        .unwrap();

    // Lire une réponse bornée par son Content-Length (garder la socket ouverte).
    fn read_one_response(stream: &mut TcpStream) -> std::io::Result<String> {
        let mut buf = [0u8; 4096];
        let mut received = Vec::new();
        let mut expected = 0usize;
        loop {
            let n = stream.read(&mut buf)?;
            if n == 0 {
                break;
            }
            received.extend_from_slice(&buf[..n]);
            if expected == 0 {
                if let Ok(text) = std::str::from_utf8(&received) {
                    if let Some(pos) = text.find("Content-Length: ") {
                        let rest = &text[pos + "Content-Length: ".len()..];
                        let digits: String =
                            rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                        let head_len = text.find("\r\n\r\n").map(|p| p + 4).unwrap_or(0);
                        expected = head_len + digits.parse::<usize>().unwrap_or(0);
                    }
                }
            }
            if expected > 0 && received.len() >= expected {
                break;
            }
        }
        Ok(String::from_utf8_lossy(&received).into_owned())
    }

    let raw = read_one_response(&mut stream).unwrap();
    assert!(raw.contains("HTTP/1.1 404"), "{raw:?}");
    assert!(
        raw.contains("Connection: close"),
        "erreur pré-corps = fermeture : {raw:?}"
    );

    // Le serveur ferme : la relecture retombe sur EOF (pas de réponse fantôme).
    let mut leftover = Vec::new();
    let n = stream.read_to_end(&mut leftover).unwrap_or(0);
    assert!(
        !String::from_utf8_lossy(&leftover).contains("jsonrpc"),
        "aucune réponse fantôme après fermeture : {n} octets résiduels"
    );
}

#[test]
fn malformed_content_length_is_400() {
    // RFC 9112 §6.3 : un CL présent mais invalide (non numérique ou hors
    // usize) rend la requête invalide — le 411 est réservé à l'absence.
    let addr = spawn_hardened();
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .write_all(b"POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Length: abc\r\n\r\n{}")
        .unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    assert!(raw.contains("HTTP/1.1 400"), "{raw:?}");
    assert!(raw.contains("Connection: close"), "{raw:?}");

    // Dépassement usize : malformé lui aussi.
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .write_all(
            b"POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Length: 99999999999999999999\r\n\r\n{}",
        )
        .unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    assert!(raw.contains("HTTP/1.1 400"), "{raw:?}");
}

#[test]
fn missing_host_header_is_400() {
    // RFC 9112 §3.2 : Host obligatoire en HTTP/1.1.
    let addr = spawn_hardened();
    let (status, _) = raw_request(
        addr,
        "POST /mcp HTTP/1.1\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
    );
    assert_eq!(status, 400);
}

#[test]
fn duplicate_host_header_is_400() {
    let addr = spawn_hardened();
    let (status, _) = raw_request(
        addr,
        "POST /mcp HTTP/1.1\r\nHost: a\r\nHost: b\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
    );
    assert_eq!(status, 400);
}

#[test]
fn http10_without_host_is_served() {
    // HTTP/1.0 : le Host est facultatif — la requête est servie normalement.
    let addr = spawn_hardened();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    let request = format!(
        "POST /mcp HTTP/1.0\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let (status, resp_body) = raw_request(addr, &request);
    assert_eq!(status, 200);
    assert!(resp_body.contains("\"result\":{}"), "{resp_body:?}");
}

#[test]
fn unsupported_version_is_505() {
    let addr = spawn_hardened();
    let (status, _) = raw_request(
        addr,
        "POST /mcp HTTP/2.0\r\nHost: t\r\nContent-Length: 2\r\n\r\n{}",
    );
    assert_eq!(status, 505);
}

#[test]
fn malformed_request_line_is_400() {
    let addr = spawn_hardened();
    // Version absente (deux jetons seulement).
    let (status, _) = raw_request(addr, "POST /mcp\r\nHost: t\r\nContent-Length: 2\r\n\r\n{}");
    assert_eq!(status, 400);
    // Jeton superflu (quatre jetons).
    let (status, _) = raw_request(
        addr,
        "POST /mcp HTTP/1.1 extra\r\nHost: t\r\nContent-Length: 2\r\n\r\n{}",
    );
    assert_eq!(status, 400);
}

#[test]
fn obs_fold_header_line_is_400() {
    // Ligne de continuation obsolète (commence par une espace) : refus net.
    let addr = spawn_hardened();
    let (status, _) = raw_request(
        addr,
        "POST /mcp HTTP/1.1\r\nHost: t\r\n X-Fold: y\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
    );
    assert_eq!(status, 400);
}

#[test]
fn over_capacity_connections_get_503() {
    let bound = ServerBuilder::new("cap", "0.2.0")
        .tool("ok", "Répond ok", |_args| Ok(serde_json::json!({})))
        .read_timeout(std::time::Duration::from_millis(300))
        .max_connections(1)
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = bound.local_addr().unwrap();
    std::thread::spawn(move || bound.serve().unwrap());

    let first = TcpStream::connect(addr).unwrap(); // occupe l'unique slot
    std::thread::sleep(std::time::Duration::from_millis(150));
    let mut second = TcpStream::connect(addr).unwrap();
    let mut raw = String::new();
    second.read_to_string(&mut raw).unwrap();
    assert!(raw.contains("HTTP/1.1 503"), "{raw:?}");
    assert!(raw.contains("trop de connexions"), "{raw:?}");
    drop(first);
}

#[test]
fn transfer_encoding_is_rejected_400() {
    let addr = spawn_hardened();
    let (status, _) = raw_request(
        addr,
        "POST /mcp HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
    );
    assert_eq!(status, 400);
}

#[test]
fn duplicate_content_length_is_rejected_400() {
    let addr = spawn_hardened();
    let (status, _) = raw_request(
        addr,
        "POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Length: 2\r\nContent-Length: 3\r\n\r\n{}",
    );
    assert_eq!(status, 400);
}

#[test]
fn health_probe_answers_200_ok() {
    let addr = spawn_hardened();
    let (status, body) = raw_request(
        addr,
        "GET /health HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status, 200);
    assert_eq!(body, "ok", "corps de sonde attendu : {body:?}");
}

#[test]
fn health_probe_post_is_405() {
    let addr = spawn_hardened();
    let (status, _) = raw_request(
        addr,
        "POST /health HTTP/1.1\r\nHost: t\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status, 405);
}

#[test]
fn health_probe_keeps_connection_reusable() {
    // La sonde ne casse pas le keep-alive : GET /health puis ping sur LA
    // MÊME connexion (requêtes pipelinées).
    let addr = spawn_hardened();
    let ping_body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    let pipeline = format!(
        "GET /health HTTP/1.1\r\nHost: t\r\n\r\nPOST /mcp HTTP/1.1\r\nHost: t\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{ping_body}",
        ping_body.len()
    );
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.write_all(pipeline.as_bytes()).unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    assert_eq!(raw.matches("HTTP/1.1 200 OK").count(), 2, "{raw:?}");
    assert!(raw.contains("\"result\":{}"), "{raw:?}");
}

#[test]
fn query_string_is_ignored_in_route_match() {
    let addr = spawn_hardened();
    let (status, body) = post(
        addr,
        r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
        "application/json",
        None,
    );
    assert_eq!(status, 200);
    assert!(body.contains("\"result\":{}"));
}

#[test]
fn oversized_body_gets_413_and_connection_closes() {
    let addr = spawn_hardened();
    let mut stream = TcpStream::connect(addr).unwrap();
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Length: {}\r\n\r\n",
        (litemcp::http::MAX_BODY) + 1
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    assert!(raw.contains("HTTP/1.1 413"), "{raw:?}");
    assert!(raw.contains("Connection: close"), "{raw:?}");
}

#[test]
fn explicit_null_id_gets_a_response_with_null_id() {
    let addr = spawn_hardened();
    let (status, body) = post(
        addr,
        r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#,
        "application/json",
        None,
    );
    assert_eq!(status, 200, "id:null explicite = requête, pas notification");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["id"], Value::Null);
    assert_eq!(v["result"], serde_json::json!({}));
}

#[test]
fn wrong_jsonrpc_version_gets_32600() {
    let addr = spawn_hardened();
    let (status, body) = post(
        addr,
        r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#,
        "application/json",
        None,
    );
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], -32600);
}

#[test]
fn batch_gets_32600_not_32700() {
    let addr = spawn_hardened();
    let (status, body) = post(
        addr,
        r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#,
        "application/json",
        None,
    );
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v["error"]["code"], -32600,
        "batch = Invalid Request, pas Parse Error"
    );
}

#[test]
fn duplicate_origin_header_is_rejected_400() {
    let addr = spawn_hardened();
    let request = "POST /mcp HTTP/1.1\r\nHost: t\r\nOrigin: https://a.example\r\nOrigin: https://b.example\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
    let (status, _) = raw_request(addr, request);
    assert_eq!(status, 400);
}

#[test]
fn truncated_body_gets_400() {
    let addr = spawn_hardened();
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    stream
        .write_all(
            b"POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{}",
        )
        .unwrap();
    let mut raw = String::new();
    let _ = stream.read_to_string(&mut raw);
    assert!(raw.contains("HTTP/1.1 400"), "{raw:?}");
}
