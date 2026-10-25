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
        r#"{"jsonrpc":"2.0","id":6,"method":"sampling/createMessage"}"#,
        "application/json",
        None,
    );
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], -32601);
}

#[test]
fn ping_result_is_identified_and_typed() {
    // `ping` est legacy (retirée de la spec 2026-07-28) mais son résultat
    // porte `resultType` (SEP-2322) et l'identité du serveur (SEP-2575),
    // comme tous les autres — la promesse « chaque résultat » sans exception.
    let addr = spawn_server();
    let (status, body) = post(
        addr,
        r#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#,
        "application/json",
        None,
    );
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["result"]["resultType"], "complete", "{body}");
    assert!(
        v["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"].is_string(),
        "serverInfo attendu même sur ping : {body}"
    );
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
            .any(|i| i.as_str().unwrap().contains("required"))
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
    assert!(raw.contains("\"resultType\":\"complete\""));
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
    assert!(
        resp_body.contains("\"resultType\":\"complete\""),
        "{resp_body:?}"
    );
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
    assert!(raw.contains("too many connections"), "{raw:?}");
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
    assert!(raw.contains("\"resultType\":\"complete\""), "{raw:?}");
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
    assert!(body.contains("\"resultType\":\"complete\""));
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
    assert_eq!(v["result"]["resultType"], "complete", "{body}");
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

// --- Durcissement supplémentaire (audit du 2026-10-23) ---

#[test]
fn header_without_colon_is_400() {
    // RFC 9112 §5 : une ligne d'en-tête sans deux-points est malformée —
    // l'ignorer exposerait une lecture divergente d'avec un proxy amont.
    let addr = spawn_hardened();
    let (status, _) = raw_request(
        addr,
        "POST /mcp HTTP/1.1\r\nHost: t\r\nJunkHeaderSansColon\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
    );
    assert_eq!(status, 400);
}

#[test]
fn space_before_colon_is_400() {
    // RFC 9112 §5.1 : aucun espace entre le nom de champ et le deux-points.
    let addr = spawn_hardened();
    let (status, _) = raw_request(
        addr,
        "POST /mcp HTTP/1.1\r\nHost : t\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
    );
    assert_eq!(status, 400);
}

#[test]
fn empty_header_name_is_400() {
    let addr = spawn_hardened();
    let (status, _) = raw_request(
        addr,
        "POST /mcp HTTP/1.1\r\nHost: t\r\n: vide\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
    );
    assert_eq!(status, 400);
}

#[test]
fn unicode_whitespace_in_request_line_is_400() {
    // RFC 9112 §3 : séparateurs = espaces ASCII uniquement. Un NBSP (valide
    // en UTF-8) ne doit pas faire une ligne de requête acceptable.
    let addr = spawn_hardened();
    let request = "POST\u{00a0}/mcp\u{00a0}HTTP/1.1\r\nHost: t\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
    let (status, _) = raw_request(addr, request);
    assert_eq!(status, 400);
}

#[test]
fn lowercase_version_is_505() {
    // RFC 9112 §2.3 : la version est un jeton exact, sensible à la casse.
    let addr = spawn_hardened();
    let (status, _) = raw_request(
        addr,
        "POST /mcp http/1.1\r\nHost: t\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
    );
    assert_eq!(status, 505);
}

#[test]
fn head_health_is_200_without_body_and_keeps_alive() {
    // HEAD : en-têtes du GET équivalent (Content-Length: 2), SANS corps —
    // et la connexion reste réutilisable (le client HEAD sait qu'aucun
    // corps ne suit, la requête pipelinée d'après doit être servie).
    let addr = spawn_hardened();
    let ping_body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    let pipeline = format!(
        "HEAD /health HTTP/1.1\r\nHost: t\r\n\r\nPOST /mcp HTTP/1.1\r\nHost: t\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{ping_body}",
        ping_body.len()
    );
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.write_all(pipeline.as_bytes()).unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    assert_eq!(raw.matches("HTTP/1.1 200 OK").count(), 2, "{raw:?}");
    // Le Content-Length annoncé reste celui du GET.
    assert!(raw.contains("Content-Length: 2"), "{raw:?}");
    // Zéro octet entre la fin des en-têtes HEAD et la réponse suivante.
    let head_end = raw.find("\r\n\r\n").unwrap() + 4;
    let next_response = raw[head_end..].find("HTTP/1.1 200 OK").unwrap() + head_end;
    assert_eq!(
        head_end,
        next_response,
        "la réponse HEAD porte un corps : {:?}",
        &raw[head_end..head_end + 20]
    );
    assert!(raw.contains("\"resultType\":\"complete\""), "{raw:?}");
}

#[test]
fn head_on_mcp_is_405_without_body() {
    // Toute réponse à HEAD est sans corps — même les erreurs : un corps sur
    // une réponse HEAD désynchroniserait un client keep-alive.
    let addr = spawn_hardened();
    let (status, body) = raw_request(
        addr,
        "HEAD /mcp HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status, 405);
    assert!(body.is_empty(), "corps sur une réponse HEAD : {body:?}");
}

#[test]
fn params_array_is_32600_not_silently_empty() {
    // JSON-RPC 2.0 : params présent mais non-objet = requête invalide,
    // pas une requête sans paramètres.
    let addr = spawn_hardened();
    let (status, body) = post(
        addr,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":["add"]}"#,
        "application/json",
        None,
    );
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], -32600);
}

// --- Correctifs de l'audit du 2026-10-23 (constats A0, A2, A4, B1, B3, B4) ---

/// Envoie une requête brute et renvoie la réponse complète (status + corps).
fn raw_exchange(addr: SocketAddr, request: &str, timeout: std::time::Duration) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(timeout)).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = String::new();
    let _ = stream.read_to_string(&mut raw);
    parse(&raw)
}

#[test]
fn health_probe_rejects_a_body_so_it_cannot_become_the_next_request() {
    // P1 : la branche /health ne consommait pas le corps. Le corps résiduel
    // devenait la requête suivante → 2 réponses pour 1 requête et exécution
    // du corps caché (désynchronisation de la file).
    //
    // Correctif retenu : un corps est **refusé** sur la sonde (400) — c'est
    // la seule façon d'être sûr qu'aucun reliquat ne reste, sans attendre
    // des octets qui pourraient ne jamais arriver.
    let addr = spawn_hardened();
    let hidden_body = r#"{"jsonrpc":"2.0","id":42,"method":"tools/call","params":{"name":"add","arguments":{"a":1,"b":2}}}"#;
    let hidden = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{hidden_body}",
        hidden_body.len()
    );
    let request = format!(
        "GET /health HTTP/1.1\r\nHost: t\r\nContent-Length: {}\r\n\r\n{hidden}",
        hidden.len()
    );
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_millis(800)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = String::new();
    let _ = stream.read_to_string(&mut raw);

    assert_eq!(
        raw.matches("HTTP/1.1").count(),
        1,
        "une seule réponse attendue (le corps caché ne doit pas être exécuté) : {raw:?}"
    );
    assert!(
        raw.contains("HTTP/1.1 400"),
        "un corps sur la sonde doit être refusé : {raw:?}"
    );
    assert!(
        !raw.contains("\"id\":42"),
        "le corps caché ne doit jamais être exécuté : {raw:?}"
    );
}

#[test]
fn health_probe_with_an_announced_body_does_not_hang() {
    // Un `Content-Length` annoncé sans corps ne doit pas retenir le slot
    // jusqu'à la deadline de 60 s : la sonde refuse immédiatement.
    let addr = spawn_hardened();
    let request = "GET /health HTTP/1.1\r\nHost: t\r\nContent-Length: 1048576\r\n\r\n";
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_millis(500)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = String::new();
    let result = stream.read_to_string(&mut raw);
    assert!(
        result.is_ok(),
        "la sonde doit répondre sans attendre le corps : {raw:?} ({result:?})"
    );
    assert!(raw.contains("HTTP/1.1 400"), "{raw:?}");
}

#[test]
fn health_probe_with_expect_100_continue_does_not_deadlock() {
    // `Expect: 100-continue` sur la sonde : sans traitement, le client
    // attendrait un `100 Continue` que la sonde n'envoie jamais.
    let addr = spawn_hardened();
    let request =
        "GET /health HTTP/1.1\r\nHost: t\r\nExpect: 100-continue\r\nContent-Length: 10\r\n\r\n";
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_millis(500)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = String::new();
    let result = stream.read_to_string(&mut raw);
    assert!(
        result.is_ok(),
        "pas de blocage croisé attendu : {raw:?} ({result:?})"
    );
    assert!(raw.contains("HTTP/1.1 400"), "{raw:?}");
}

#[test]
fn transfer_encoding_is_rejected_on_every_route() {
    // Le refus de Transfer-Encoding était placé APRÈS le routage : il ne
    // s'appliquait donc pas à /health.
    let addr = spawn_hardened();
    for path in ["/mcp", "/health"] {
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"
        );
        let (status, _) = raw_exchange(addr, &request, std::time::Duration::from_secs(2));
        assert_eq!(status, 400, "Transfer-Encoding sur {path} doit être refusé");
    }
}

#[test]
fn content_type_must_be_json() {
    // Un POST en text/plain était traité comme du JSON (confusion de type).
    let addr = spawn_hardened();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, _) = raw_exchange(addr, &request, std::time::Duration::from_secs(2));
    assert_eq!(status, 415, "Content-Type non-JSON doit être refusé");

    // ... mais application/json (avec ou sans charset) passe.
    for ct in ["application/json", "application/json; charset=utf-8"] {
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (status, _) = raw_exchange(addr, &request, std::time::Duration::from_secs(2));
        assert_eq!(status, 200, "Content-Type {ct} doit être accepté");
    }
}

#[test]
fn invalid_requests_keep_their_id_in_the_error_response() {
    // JSON-RPC 2.0 §5 : la réponse d'erreur porte le même id.
    let addr = spawn_hardened();
    for (request, expected_id) in [
        (r#"{"jsonrpc":"1.0","id":7,"method":"ping"}"#, 7),
        (r#"{"jsonrpc":"2.0","id":8,"method":42}"#, 8),
        (
            r#"{"jsonrpc":"2.0","id":9,"method":"ping","params":[1]}"#,
            9,
        ),
    ] {
        let (status, body) = post(addr, request, "application/json", None);
        assert_eq!(status, 200);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["id"], expected_id, "id non préservé pour {request}");
        assert_eq!(v["error"]["code"], -32600);
    }
}

#[test]
fn arguments_must_be_an_object_even_without_a_schema() {
    // Le contrat de l'API (« la closure reçoit arguments (objet JSON) »)
    // doit tenir même pour un outil sans schéma.
    let addr = spawn_hardened();
    for arguments in ["\"chaine\"", "[1,2]", "null", "42"] {
        let request = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"echo","arguments":{arguments}}}}}"#
        );
        let (status, body) = post(addr, &request, "application/json", None);
        assert_eq!(status, 200);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            v["error"]["code"], -32602,
            "arguments={arguments} doit être refusé : {body}"
        );
    }
}

#[test]
fn server_discover_advertises_capabilities_and_versions() {
    // MCP 2026-07-28 : `server/discover` est obligatoire (SEP-2575).
    let addr = spawn_hardened();
    let (status, body) = post(
        addr,
        r#"{"jsonrpc":"2.0","id":1,"method":"server/discover"}"#,
        "application/json",
        None,
    );
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    let result = &v["result"];
    assert_eq!(result["resultType"], "complete");
    assert_eq!(result["supportedVersions"][0], "2026-07-28");
    assert!(result["capabilities"]["tools"].is_object());
    assert!(
        result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"].is_string(),
        "serverInfo attendu dans _meta : {body}"
    );
}

#[test]
fn every_result_carries_a_result_type() {
    // SEP-2322 : `resultType` est obligatoire dans chaque result.
    let addr = spawn_hardened();
    for method in ["tools/list", "server/discover"] {
        let (status, body) = post(
            addr,
            &format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{method}"}}"#),
            "application/json",
            None,
        );
        assert_eq!(status, 200);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["result"]["resultType"], "complete", "méthode {method}");
    }
}

#[test]
fn tools_list_carries_cache_hints() {
    // SEP-2549 : `ttlMs` et `cacheScope` sur les résultats de liste.
    let addr = spawn_hardened();
    let (status, body) = post(
        addr,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        "application/json",
        None,
    );
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(v["result"]["ttlMs"].is_u64(), "ttlMs absent : {body}");
    assert_eq!(v["result"]["cacheScope"], "private");
}

#[test]
fn unsupported_protocol_version_is_rejected_with_32022() {
    // SEP-2575 : une version non supportée → UnsupportedProtocolVersionError.
    let addr = spawn_hardened();
    let (status, body) = post(
        addr,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"1999-01-01"}}}"#,
        "application/json",
        None,
    );
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], -32022);
    assert_eq!(v["error"]["data"]["supported"][0], "2026-07-28");
}

#[test]
fn tab_in_the_request_line_is_rejected() {
    // RFC 9112 §3 : seul SP sépare les jetons de la ligne de requête.
    let addr = spawn_hardened();
    let request =
        "POST\t/mcp\tHTTP/1.1\r\nHost: t\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
    let (status, _) = raw_exchange(addr, request, std::time::Duration::from_secs(2));
    assert_eq!(status, 400);
}

#[test]
fn bare_lf_line_endings_are_rejected() {
    // RFC 9112 §2.2 : la fin de ligne est CRLF (le LF seul est le vecteur
    // « httpoxy »).
    let addr = spawn_hardened();
    let request = "GET /health HTTP/1.1\nHost: t\n\n";
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_millis(800)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = String::new();
    let _ = stream.read_to_string(&mut raw);
    assert!(
        !raw.contains("HTTP/1.1 200"),
        "un LF seul ne doit pas composer une requête valide : {raw:?}"
    );
}

#[test]
fn accept_and_expect_are_matched_by_exact_token() {
    // `text/event-stream-x` ne contient pas le jeton `text/event-stream` ;
    // `not-100-continued` ne contient pas le jeton `100-continue`.
    let addr = spawn_hardened();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;

    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nAccept: text/event-stream-x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = String::new();
    let _ = stream.read_to_string(&mut raw);
    assert!(
        !raw.contains("text/event-stream"),
        "Accept par sous-chaîne ne doit pas basculer en SSE : {raw:?}"
    );

    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nExpect: not-100-continued\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = String::new();
    let _ = stream.read_to_string(&mut raw);
    assert!(
        !raw.contains("100 Continue"),
        "Expect par sous-chaîne ne doit pas déclencher 100-continue : {raw:?}"
    );
}

#[test]
fn saturated_server_delivers_the_503_to_a_client_that_wrote_first() {
    // Le chemin 503 faisait un `close()` avec la requête du client non lue
    // dans le tampon de réception → RST → le client ne lisait jamais le 503
    // (16/20 réponses perdues). Le drain + shutdown(Write) le corrige.
    let bound = ServerBuilder::new("cap", "0.2.0")
        .tool("ok", "Répond ok", |_args| Ok(serde_json::json!({})))
        .read_timeout(std::time::Duration::from_millis(300))
        .max_connections(1)
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = bound.local_addr().unwrap();
    std::thread::spawn(move || bound.serve().unwrap());

    // La première connexion occupe l'unique slot.
    let _holder = TcpStream::connect(addr).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(150));

    // Client réaliste : il ENVOIE sa requête avant de lire la réponse.
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let write_result = stream.write_all(request.as_bytes());
    assert!(
        write_result.is_ok(),
        "l'écriture peut échouer si le serveur ferme — mais le 503 doit rester lisible"
    );

    let mut raw = String::new();
    let read_result = stream.read_to_string(&mut raw);
    assert!(
        read_result.is_ok(),
        "le client doit lire le 503 sans erreur RST : {raw:?} ({read_result:?})"
    );
    assert!(
        raw.contains("HTTP/1.1 503"),
        "le client doit lire le 503 : {raw:?}"
    );
    // Le corps JSON complet doit être là (pas seulement la ligne de statut).
    assert!(
        raw.contains("too many connections"),
        "le corps du 503 doit être complet : {raw:?}"
    );
}

#[test]
fn connection_header_semantics_are_exact() {
    // Matrice complète du comportement `Connection` (HTTP/1.0 vs 1.1).
    // `Connection: close` est ABSORBANT : une fois présent, il reste même
    // si un `keep-alive` suit dans un autre en-tête (RFC 9112 §9.6).
    let addr = spawn_hardened();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;

    fn connection_of(response: &str) -> String {
        response
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("connection:"))
            .map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string())
            .unwrap_or_default()
    }

    for (version, headers, expected) in [
        ("1.0", vec![], "close"),
        ("1.0", vec!["Connection: close"], "close"),
        ("1.0", vec!["Connection: keep-alive"], "keep-alive"),
        (
            "1.0",
            vec!["Connection: close", "Connection: keep-alive"],
            "close",
        ),
        (
            "1.0",
            vec!["Connection: keep-alive", "Connection: close"],
            "close",
        ),
        ("1.1", vec![], "keep-alive"),
        ("1.1", vec!["Connection: close"], "close"),
        ("1.1", vec!["Connection: keep-alive"], "keep-alive"),
        (
            "1.1",
            vec!["Connection: close", "Connection: keep-alive"],
            "close",
        ),
    ] {
        let mut lines = vec![format!("POST /mcp HTTP/{version}"), "Host: t".to_string()];
        lines.extend(headers.iter().map(|h| h.to_string()));
        lines.push(format!("Content-Length: {}", body.len()));
        let request = format!("{}\r\n\r\n{body}", lines.join("\r\n"));

        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            // Lit UNE réponse complète (en-tête + corps selon Content-Length).
            let mut read = 0usize;
            while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        raw.extend_from_slice(&buf[..n]);
                        read += n;
                    }
                    Err(_) => break,
                }
            }
            if read == 0 {
                break;
            }
            let text = String::from_utf8_lossy(&raw).into_owned();
            let Some(split) = text.find("\r\n\r\n") else {
                break;
            };
            let head = &text[..split];
            let body_len = head
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                .and_then(|l| l.split(':').nth(1))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if text.len() - (split + 4) >= body_len {
                break;
            }
        }
        let text = String::from_utf8_lossy(&raw).into_owned();
        assert_eq!(
            connection_of(&text),
            expected,
            "HTTP/{version} avec {:?} -> Connection attendu {expected}",
            headers
        );
    }
}

#[test]
fn accept_ignores_media_type_parameters() {
    // RFC 9110 §12.5.1 : `application/json; charset=utf-8` contient bien le
    // type `application/json`.
    let addr = spawn_hardened();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    for (accept, expect_sse) in [
        ("application/json", false),
        ("application/json; charset=utf-8", false),
        ("text/event-stream", true),
        ("text/event-stream; charset=utf-8", true),
    ] {
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: t\r\nAccept: {accept}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut raw = String::new();
        let _ = stream.read_to_string(&mut raw);

        let split = raw.find("\r\n\r\n").expect("réponse complète");
        let head = &raw[..split];
        let resp_body = &raw[split + 4..];
        assert!(
            head.starts_with("HTTP/1.1 200"),
            "Accept {accept} : {raw:?}"
        );
        assert_eq!(
            head.contains("Content-Type: text/event-stream"),
            expect_sse,
            "Accept {accept} : {head}"
        );
        // SSE : le corps est `data: {json}` ; JSON : le corps est `{json}`.
        let payload = resp_body.trim();
        let payload = payload.strip_prefix("data: ").unwrap_or(payload).trim();
        let v: Value = serde_json::from_str(payload)
            .unwrap_or_else(|e| panic!("Accept {accept} : {e} — {resp_body:?}"));
        assert!(v.get("result").is_some(), "Accept {accept} : {resp_body}");
    }
}

#[test]
fn initialize_result_carries_a_result_type() {
    // SEP-2322 : `resultType` est obligatoire dans CHAQUE résultat.
    let addr = spawn_hardened();
    let (status, body) = post(
        addr,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        "application/json",
        None,
    );
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["result"]["resultType"], "complete", "{body}");
}

#[test]
fn health_probe_rejects_a_body_larger_than_one_chunk() {
    // Un corps de plus de 64 Kio (la taille d'un chunk de lecture) doit lui
    // aussi être refusé : aucun reliquat ne doit devenir la requête suivante.
    let addr = spawn_hardened();
    let hidden_body = r#"{"jsonrpc":"2.0","id":42,"method":"tools/call","params":{"name":"add","arguments":{"a":1,"b":1}}}"#;
    let hidden = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{hidden_body}",
        hidden_body.len()
    );
    let payload = "A".repeat(64 * 1024) + &hidden;
    let request = format!(
        "GET /health HTTP/1.1\r\nHost: t\r\nContent-Length: {}\r\n\r\n{payload}",
        payload.len()
    );
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_millis(800)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = String::new();
    let _ = stream.read_to_string(&mut raw);

    assert_eq!(
        raw.matches("HTTP/1.1").count(),
        1,
        "une seule réponse attendue : {raw:?}"
    );
    assert!(raw.contains("HTTP/1.1 400"), "{raw:?}");
    assert!(
        !raw.contains("\"id\":42"),
        "le corps caché ne doit jamais être exécuté : {raw:?}"
    );
}

#[test]
fn connection_ttl_zero_disables_the_ttl() {
    // `connection_ttl(0)` doit DÉSACTIVER le TTL (documenté ainsi), pas
    // couper la connexion immédiatement.
    let bound = ServerBuilder::new("ttl0", "0.1.0")
        .tool("ok", "ok", |_args| Ok(serde_json::json!({})))
        .read_timeout(std::time::Duration::from_secs(30))
        .connection_ttl(std::time::Duration::ZERO)
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = bound.local_addr().unwrap();
    std::thread::spawn(move || bound.serve().unwrap());

    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );

    // Plusieurs requêtes espacées : avec un TTL nul, la connexion survit.
    for i in 0..3 {
        stream.write_all(request.as_bytes()).unwrap();
        let mut buf = [0u8; 4096];
        let n = stream.read(&mut buf).expect("réponse attendue");
        assert!(
            n > 0,
            "connexion coupée à l'itération {i} — TTL nul mal traité"
        );
        std::thread::sleep(std::time::Duration::from_millis(120));
    }
}

#[test]
fn connection_ttl_cuts_a_drip_fed_connection() {
    let bound = ServerBuilder::new("ttl", "0.1.0")
        .tool("ok", "ok", |_args| Ok(serde_json::json!({})))
        .read_timeout(std::time::Duration::from_secs(30))
        .connection_ttl(std::time::Duration::from_millis(800))
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = bound.local_addr().unwrap();
    std::thread::spawn(move || bound.serve().unwrap());

    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_millis(400)))
        .unwrap();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );

    let start = std::time::Instant::now();
    let mut closed = false;
    while start.elapsed() < std::time::Duration::from_secs(6) {
        if stream.write_all(request.as_bytes()).is_err()
            || stream.write_all(body.as_bytes()).is_err()
        {
            closed = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        let mut buf = [0u8; 4096];
        match stream.read(&mut buf) {
            Ok(0) => {
                closed = true;
                break;
            }
            Ok(_) => {}
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => {
                closed = true;
                break;
            }
        }
    }
    assert!(closed, "la connexion doit être coupée par le TTL");
    assert!(
        start.elapsed() < std::time::Duration::from_secs(4),
        "la coupure doit venir du TTL (800 ms), pas du read_timeout (30 s)"
    );
}

// --- Correctifs de l'audit du 2026-10-24 (N1, N3, N4, N5) ---

#[test]
fn tool_results_identify_the_server_in_meta() {
    // N1 : SEP-2575 — les serveurs SHOULD s'identifier dans le `_meta` de
    // chaque résultat, pas seulement dans `server/discover`/`initialize`.
    let addr = spawn_hardened();
    let (status, body) = post(
        addr,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        "application/json",
        None,
    );
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"], "durci",
        "serverInfo attendu sur tools/list : {body}"
    );

    let (status, body) = post(
        addr,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"ok","arguments":{}}}"#,
        "application/json",
        None,
    );
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"], "durci",
        "serverInfo attendu sur tools/call : {body}"
    );

    // Un échec d'exécution (`isError: true`) reste un `result` : identifié
    // lui aussi.
    let bound = ServerBuilder::new("durci", "0.2.0")
        .tool("echoue", "échoue exprès", |_args| Err("échec".into()))
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = bound.local_addr().unwrap();
    std::thread::spawn(move || bound.serve().unwrap());
    let (status, body) = post(
        addr,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"echoue","arguments":{}}}"#,
        "application/json",
        None,
    );
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["result"]["isError"], true);
    assert!(
        v["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"].is_string(),
        "serverInfo attendu même sur un résultat en erreur : {body}"
    );
}

#[test]
fn duplicate_content_type_is_rejected_regardless_of_order() {
    // N3 : le verdict ne doit pas dépendre de l'ordre — avant le correctif,
    // `application/json` puis `text/plain` était refusé (415) et l'ordre
    // inverse accepté (200), divergent d'un proxy qui combine les valeurs.
    let addr = spawn_hardened();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    for headers in [
        "Content-Type: application/json\r\nContent-Type: text/plain",
        "Content-Type: text/plain\r\nContent-Type: application/json",
        "Content-Type: application/json\r\nContent-Type: application/json",
    ] {
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: t\r\n{headers}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (status, _) = raw_exchange(addr, &request, std::time::Duration::from_secs(2));
        assert_eq!(status, 400, "Content-Type dupliqué → 400 : {headers}");
    }
}

#[test]
fn duplicate_mcp_routing_headers_are_rejected() {
    // N4 : un `Mcp-Method`/`Mcp-Name` dupliqué rend le routage ambigu (un
    // gateway qui route sur la première valeur diverge du contrôle fait sur
    // la dernière). Avant le correctif : dernier-gagne, silencieux.
    let addr = spawn_hardened();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    for headers in [
        "Mcp-Method: ping\r\nMcp-Method: tools/list",
        "Mcp-Method: tools/list\r\nMcp-Method: ping",
        "Mcp-Name: ok\r\nMcp-Name: autre",
    ] {
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\n{headers}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (status, _) = raw_exchange(addr, &request, std::time::Duration::from_secs(2));
        assert_eq!(status, 400, "{headers} → 400");
    }

    // Un en-tête unique cohérent avec le corps reste accepté.
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nMcp-Method: ping\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, _) = raw_exchange(addr, &request, std::time::Duration::from_secs(2));
    assert_eq!(status, 200, "Mcp-Method unique et cohérent → 200");
}

#[test]
fn bind_rejects_routes_that_cannot_match_or_mask_health() {
    // N5 : une route sans '/' initial ou avec une query ne peut jamais
    // matcher ; `/health` masque la sonde de vie. Avant le correctif, le
    // serveur démarrait et était silencieusement injoignable (405/404).
    fn base() -> ServerBuilder {
        ServerBuilder::new("r", "0.1.0").tool("ok", "ok", |_args| Ok(serde_json::json!({})))
    }
    for route in ["mcp", "", "/mcp?x=1", "/health"] {
        let err = match base().route(route).bind("127.0.0.1:0") {
            Err(err) => err,
            Ok(_) => panic!("route {route:?} doit être refusée au bind"),
        };
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::InvalidInput,
            "route {route:?} doit être refusée au bind : {err}"
        );
    }
    // Route valide : bind OK.
    assert!(base().route("/api/mcp").bind("127.0.0.1:0").is_ok());
}

// --- Arrêt propre (ServerHandle) et nouvelles surfaces sur le transport ---

#[test]
fn shutdown_stops_serve_and_closes_an_idle_connection() {
    let bound = ServerBuilder::new("stop", "0.1.0")
        .tool("ok", "ok", |_args| Ok(serde_json::json!({})))
        .read_timeout(std::time::Duration::from_millis(300))
        .bind("127.0.0.1:0")
        .unwrap();
    let handle = bound.handle();
    let addr = bound.local_addr().unwrap();
    let server = std::thread::spawn(move || bound.serve());

    // Connexion idle (aucune requête) : elle occupe un thread bloqué en
    // lecture jusqu'au read_timeout.
    let mut client = TcpStream::connect(addr).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(150));

    handle.shutdown();
    let started = std::time::Instant::now();
    server
        .join()
        .expect("le thread serve ne doit pas paniquer")
        .expect("serve() doit rendre Ok(()) sur shutdown");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "serve() doit rendre promptement après shutdown()"
    );

    // La connexion idle est fermée au plus tard à son read_timeout (300 ms).
    client
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    let mut buf = [0u8; 16];
    let n = client.read(&mut buf).expect("EOF attendu, pas d'erreur");
    assert_eq!(n, 0, "la connexion doit être fermée proprement (EOF)");
}

#[test]
fn shutdown_is_idempotent_and_refuses_new_connections() {
    let bound = ServerBuilder::new("stop", "0.1.0")
        .tool("ok", "ok", |_args| Ok(serde_json::json!({})))
        .bind("127.0.0.1:0")
        .unwrap();
    let handle = bound.handle();
    let addr = bound.local_addr().unwrap();
    std::thread::spawn(move || {
        let _ = bound.serve();
    });
    std::thread::sleep(std::time::Duration::from_millis(150));

    // Double appel : le second est un no-op (pas de panique, pas d'erreur).
    handle.shutdown();
    handle.shutdown();
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(
        TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(500)).is_err(),
        "après shutdown, le listener est fermé : plus de connexion possible"
    );
}

#[test]
fn resources_and_prompts_are_served_over_http() {
    let bound = ServerBuilder::new("full", "0.1.0")
        .tool("ok", "ok", |_args| Ok(serde_json::json!({})))
        .resource("mem://doc", "Doc", "La doc", || {
            Ok(litemcp::protocol::ResourceContents {
                uri: "mem://doc".to_string(),
                mime_type: Some("text/markdown".to_string()),
                text: "# Bonjour".to_string(),
            })
        })
        .prompt("salut", "Salue", |args| {
            let nom = args.get("nom").and_then(Value::as_str).unwrap_or("monde");
            Ok(vec![litemcp::protocol::PromptMessage::user(format!(
                "Bonjour {nom}"
            ))])
        })
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = bound.local_addr().unwrap();
    std::thread::spawn(move || bound.serve().unwrap());

    // Les quatre surfaces répondent en JSON-RPC sur le transport.
    for (id, body) in [
        (1, r#"{"jsonrpc":"2.0","id":1,"method":"resources/list"}"#),
        (
            2,
            r#"{"jsonrpc":"2.0","id":2,"method":"resources/read","params":{"uri":"mem://doc"}}"#,
        ),
        (3, r#"{"jsonrpc":"2.0","id":3,"method":"prompts/list"}"#),
        (
            4,
            r#"{"jsonrpc":"2.0","id":4,"method":"prompts/get","params":{"name":"salut","arguments":{"nom":"Léa"}}}"#,
        ),
    ] {
        let (status, resp_body) = post(addr, body, "application/json", None);
        assert_eq!(status, 200, "requête {id}");
        let v: Value = serde_json::from_str(&resp_body).unwrap();
        assert!(v["result"].is_object(), "un résultat est attendu : {v}");
    }

    // Et l'annonce de capacités suit les registres.
    let (status, resp_body) = post(
        addr,
        r#"{"jsonrpc":"2.0","id":5,"method":"server/discover"}"#,
        "application/json",
        None,
    );
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&resp_body).unwrap();
    let capabilities = &v["result"]["capabilities"];
    assert!(capabilities["tools"].is_object(), "{v}");
    assert!(capabilities["resources"].is_object(), "{v}");
    assert!(capabilities["prompts"].is_object(), "{v}");
}

/// Une réponse JSON-RPC sérialisée qui dépasse `MAX_RESPONSE_BYTES` (16 Mio) est
/// remplacée par une erreur `-32603` : le serveur ne pousse jamais un corps
/// illimité, même si un handler rend une valeur énorme.
#[test]
fn response_larger_than_16_mib_is_replaced_by_internal_error() {
    let bound = ServerBuilder::new("gros", "0.1.0")
        .tool("blob", "Rend une chaîne énorme", |_args| {
            Ok(Value::String("x".repeat(17 * 1024 * 1024)))
        })
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = bound.local_addr().unwrap();
    std::thread::spawn(move || bound.serve().unwrap());

    let (status, body) = post(
        addr,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"blob","arguments":{}}}"#,
        "application/json",
        None,
    );
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], -32603, "{v}");
    let message = v["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("exceeds maximum"),
        "message inattendu : {message}"
    );
}
