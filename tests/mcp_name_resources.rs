//! Tests de validation de l'en-tête `Mcp-Name` pour `resources/read`,
//! `tools/call`, `prompts/get` et rejet sur les méthodes sans nom,
//! au niveau `ServerCore` et sur de vraies sockets TCP.

use litemcp::prelude::*;
use litemcp::protocol::{PromptMessage, ResourceContents};
use litemcp::server::McpHeaders;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

const ASCII_URI: &str = "memo://ascii/test";
const UNICODE_URI: &str = "memo://unicode/café";

fn build_test_server_builder() -> ServerBuilder {
    ServerBuilder::new("test-mcp-name", "1.0.0")
        .legacy(true)
        .resource(ASCII_URI, "ascii_memo", "ASCII note", || {
            Ok(ResourceContents {
                uri: ASCII_URI.to_string(),
                mime_type: Some("text/plain".to_string()),
                text: "contenu note ascii".to_string(),
            })
        })
        .resource(UNICODE_URI, "unicode_memo", "Unicode note", || {
            Ok(ResourceContents {
                uri: UNICODE_URI.to_string(),
                mime_type: Some("text/plain".to_string()),
                text: "contenu note café unicode".to_string(),
            })
        })
        .tool("echo", "Echo args", |args| Ok(args.clone()))
        .prompt("greeting", "Greeting prompt", |_args| {
            Ok(vec![PromptMessage::user("Bonjour!")])
        })
}

// ============================================================================
// 1. Tests ServerCore
// ============================================================================

#[test]
fn server_core_resources_read_ascii_uri() {
    let core = build_test_server_builder().core();

    // 1. Nominal : texte avec URI ASCII et Mcp-Name correspondant.
    let headers = McpHeaders {
        method: Some("resources/read".to_string()),
        name: Some(ASCII_URI.to_string()),
        ..Default::default()
    };
    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "resources/read",
        "params": { "uri": ASCII_URI }
    });
    let reply = core
        .dispatch_with_headers(req.to_string().as_bytes(), Some(&headers))
        .expect("réponse attendue");
    let v: Value = serde_json::from_slice(&reply).unwrap();
    assert_eq!(
        v["result"]["contents"][0]["text"], "contenu note ascii",
        "lecture nominale avec Mcp-Name: {v}"
    );

    // 2. URI inconnue : Mcp-Name correspond à params.uri, mais ressource inconnue (-32602, pas -32020).
    let headers_unknown = McpHeaders {
        method: Some("resources/read".to_string()),
        name: Some("memo://ascii/inconnu".to_string()),
        ..Default::default()
    };
    let req_unknown = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "resources/read",
        "params": { "uri": "memo://ascii/inconnu" }
    });
    let reply = core
        .dispatch_with_headers(req_unknown.to_string().as_bytes(), Some(&headers_unknown))
        .expect("réponse attendue");
    let v: Value = serde_json::from_slice(&reply).unwrap();
    assert_eq!(
        v["error"]["code"], -32602,
        "URI inconnue doit donner -32602 et non -32020 : {v}"
    );

    // 3. En-tête différent : Mcp-Name ne correspond pas à params.uri -> rejet -32020.
    let headers_diff = McpHeaders {
        method: Some("resources/read".to_string()),
        name: Some("memo://ascii/autre".to_string()),
        ..Default::default()
    };
    let reply = core
        .dispatch_with_headers(req.to_string().as_bytes(), Some(&headers_diff))
        .expect("réponse attendue");
    let v: Value = serde_json::from_slice(&reply).unwrap();
    assert_eq!(
        v["error"]["code"], -32020,
        "Mcp-Name divergent doit être rejeté en -32020 : {v}"
    );

    // 4. Méthode sans nom : Mcp-Name présent sur resources/list -> rejet -32020.
    let headers_no_name = McpHeaders {
        method: Some("resources/list".to_string()),
        name: Some(ASCII_URI.to_string()),
        ..Default::default()
    };
    let req_list = json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "resources/list"
    });
    let reply = core
        .dispatch_with_headers(req_list.to_string().as_bytes(), Some(&headers_no_name))
        .expect("réponse attendue");
    let v: Value = serde_json::from_slice(&reply).unwrap();
    assert_eq!(
        v["error"]["code"], -32020,
        "Mcp-Name sur méthode sans nom doit donner -32020 : {v}"
    );
}

#[test]
fn server_core_resources_read_future_base64_uri() {
    let core = build_test_server_builder().core();

    // 1. Nominal : texte avec URI UTF-8 / futurs besoins Base64 (accents, non-ASCII).
    let headers = McpHeaders {
        method: Some("resources/read".to_string()),
        name: Some(UNICODE_URI.to_string()),
        ..Default::default()
    };
    let req = json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "resources/read",
        "params": { "uri": UNICODE_URI }
    });
    let reply = core
        .dispatch_with_headers(req.to_string().as_bytes(), Some(&headers))
        .expect("réponse attendue");
    let v: Value = serde_json::from_slice(&reply).unwrap();
    assert_eq!(
        v["result"]["contents"][0]["text"], "contenu note café unicode",
        "lecture nominale unicode avec Mcp-Name: {v}"
    );

    // 2. URI inconnue avec caractères non-ASCII : correspond à Mcp-Name -> -32602 (pas -32020).
    let headers_unknown = McpHeaders {
        method: Some("resources/read".to_string()),
        name: Some("memo://unicode/inconnu_é".to_string()),
        ..Default::default()
    };
    let req_unknown = json!({
        "jsonrpc": "2.0",
        "id": 11,
        "method": "resources/read",
        "params": { "uri": "memo://unicode/inconnu_é" }
    });
    let reply = core
        .dispatch_with_headers(req_unknown.to_string().as_bytes(), Some(&headers_unknown))
        .expect("réponse attendue");
    let v: Value = serde_json::from_slice(&reply).unwrap();
    assert_eq!(
        v["error"]["code"], -32602,
        "URI unicode inconnue doit donner -32602 et non -32020 : {v}"
    );

    // 3. En-tête différent avec non-ASCII -> rejet -32020.
    let headers_diff = McpHeaders {
        method: Some("resources/read".to_string()),
        name: Some("memo://unicode/différent".to_string()),
        ..Default::default()
    };
    let reply = core
        .dispatch_with_headers(req.to_string().as_bytes(), Some(&headers_diff))
        .expect("réponse attendue");
    let v: Value = serde_json::from_slice(&reply).unwrap();
    assert_eq!(
        v["error"]["code"], -32020,
        "Mcp-Name unicode divergent doit être rejeté en -32020 : {v}"
    );

    // 4. Méthode sans nom avec Mcp-Name unicode -> rejet -32020.
    let headers_no_name = McpHeaders {
        method: Some("resources/list".to_string()),
        name: Some(UNICODE_URI.to_string()),
        ..Default::default()
    };
    let req_list = json!({
        "jsonrpc": "2.0",
        "id": 13,
        "method": "resources/list"
    });
    let reply = core
        .dispatch_with_headers(req_list.to_string().as_bytes(), Some(&headers_no_name))
        .expect("réponse attendue");
    let v: Value = serde_json::from_slice(&reply).unwrap();
    assert_eq!(
        v["error"]["code"], -32020,
        "Mcp-Name unicode sur méthode sans nom doit donner -32020 : {v}"
    );
}

#[test]
fn server_core_non_regression_tools_and_prompts() {
    let core = build_test_server_builder().core();

    // tools/call nominal
    let headers_tool = McpHeaders {
        method: Some("tools/call".to_string()),
        name: Some("echo".to_string()),
        ..Default::default()
    };
    let req_tool = json!({
        "jsonrpc": "2.0",
        "id": 20,
        "method": "tools/call",
        "params": { "name": "echo", "arguments": { "msg": "ok" } }
    });
    let reply = core
        .dispatch_with_headers(req_tool.to_string().as_bytes(), Some(&headers_tool))
        .expect("réponse attendue");
    let v: Value = serde_json::from_slice(&reply).unwrap();
    assert_eq!(v["result"]["structuredContent"]["msg"], "ok");

    // tools/call divergent
    let headers_tool_bad = McpHeaders {
        method: Some("tools/call".to_string()),
        name: Some("autre".to_string()),
        ..Default::default()
    };
    let reply = core
        .dispatch_with_headers(req_tool.to_string().as_bytes(), Some(&headers_tool_bad))
        .expect("réponse attendue");
    let v: Value = serde_json::from_slice(&reply).unwrap();
    assert_eq!(v["error"]["code"], -32020);

    // prompts/get nominal
    let headers_prompt = McpHeaders {
        method: Some("prompts/get".to_string()),
        name: Some("greeting".to_string()),
        ..Default::default()
    };
    let req_prompt = json!({
        "jsonrpc": "2.0",
        "id": 21,
        "method": "prompts/get",
        "params": { "name": "greeting", "arguments": {} }
    });
    let reply = core
        .dispatch_with_headers(req_prompt.to_string().as_bytes(), Some(&headers_prompt))
        .expect("réponse attendue");
    let v: Value = serde_json::from_slice(&reply).unwrap();
    assert_eq!(v["result"]["messages"][0]["content"]["text"], "Bonjour!");

    // prompts/get divergent
    let headers_prompt_bad = McpHeaders {
        method: Some("prompts/get".to_string()),
        name: Some("autre".to_string()),
        ..Default::default()
    };
    let reply = core
        .dispatch_with_headers(req_prompt.to_string().as_bytes(), Some(&headers_prompt_bad))
        .expect("réponse attendue");
    let v: Value = serde_json::from_slice(&reply).unwrap();
    assert_eq!(v["error"]["code"], -32020);

    // méthode sans nom (tools/list) avec Mcp-Name
    let headers_list = McpHeaders {
        method: Some("tools/list".to_string()),
        name: Some("echo".to_string()),
        ..Default::default()
    };
    let req_list = json!({
        "jsonrpc": "2.0",
        "id": 22,
        "method": "tools/list"
    });
    let reply = core
        .dispatch_with_headers(req_list.to_string().as_bytes(), Some(&headers_list))
        .expect("réponse attendue");
    let v: Value = serde_json::from_slice(&reply).unwrap();
    assert_eq!(v["error"]["code"], -32020);
}

// ============================================================================
// 2. Tests vraies sockets TCP
// ============================================================================

fn spawn_socket_server() -> SocketAddr {
    let bound = build_test_server_builder()
        .bind("127.0.0.1:0")
        .expect("bind socket server");
    let addr = bound.local_addr().expect("local_addr");
    std::thread::spawn(move || {
        let _ = bound.serve();
    });
    addr
}

fn raw_post(addr: SocketAddr, headers: &[(&str, &str)], body: &str) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout");
    let mut req = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);

    let _ = stream.write_all(req.as_bytes());
    let _ = stream.flush();

    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    let s = String::from_utf8_lossy(&buf).to_string();
    let (head, body_str) = s.split_once("\r\n\r\n").unwrap_or((&s, ""));
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let val: Value = serde_json::from_str(body_str).unwrap_or(Value::Null);
    (status, val)
}

#[test]
fn socket_resources_read_ascii_uri() {
    let addr = spawn_socket_server();

    // 1. Nominal texte : URI ASCII
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "resources/read",
        "params": { "uri": ASCII_URI }
    })
    .to_string();
    let (status, val) = raw_post(
        addr,
        &[("Mcp-Method", "resources/read"), ("Mcp-Name", ASCII_URI)],
        &body,
    );
    assert_eq!(status, 200);
    assert_eq!(
        val["result"]["contents"][0]["text"], "contenu note ascii",
        "Socket: nominal resources/read ASCII échoué: {val}"
    );

    // 2. URI inconnue : Mcp-Name correspond à params.uri -> -32602
    let body_unk = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "resources/read",
        "params": { "uri": "memo://ascii/unknown" }
    })
    .to_string();
    let (status, val) = raw_post(
        addr,
        &[
            ("Mcp-Method", "resources/read"),
            ("Mcp-Name", "memo://ascii/unknown"),
        ],
        &body_unk,
    );
    assert_eq!(status, 200);
    assert_eq!(
        val["error"]["code"], -32602,
        "Socket: URI inconnue doit être -32602: {val}"
    );

    // 3. En-tête différent : Mcp-Name != params.uri -> -32020
    let (status, val) = raw_post(
        addr,
        &[
            ("Mcp-Method", "resources/read"),
            ("Mcp-Name", "memo://ascii/different"),
        ],
        &body,
    );
    assert_eq!(status, 400);
    assert_eq!(
        val["error"]["code"], -32020,
        "Socket: Mcp-Name divergent doit être -32020: {val}"
    );

    // 4. Méthode sans nom avec Mcp-Name -> -32020
    let body_list = json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "resources/list"
    })
    .to_string();
    let (status, val) = raw_post(
        addr,
        &[("Mcp-Method", "resources/list"), ("Mcp-Name", ASCII_URI)],
        &body_list,
    );
    assert_eq!(status, 400);
    assert_eq!(
        val["error"]["code"], -32020,
        "Socket: Mcp-Name sur resources/list doit être -32020: {val}"
    );
}

#[test]
fn socket_resources_read_future_base64_uri() {
    let addr = spawn_socket_server();

    // 1. Nominal texte : URI UTF-8
    let body = json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "resources/read",
        "params": { "uri": UNICODE_URI }
    })
    .to_string();
    let (status, val) = raw_post(
        addr,
        &[("Mcp-Method", "resources/read"), ("Mcp-Name", UNICODE_URI)],
        &body,
    );
    assert_eq!(status, 200);
    assert_eq!(
        val["result"]["contents"][0]["text"], "contenu note café unicode",
        "Socket: nominal resources/read unicode: {val}"
    );

    // 2. URI inconnue : Mcp-Name correspond à params.uri -> -32602
    let body_unk = json!({
        "jsonrpc": "2.0",
        "id": 11,
        "method": "resources/read",
        "params": { "uri": "memo://unicode/inconnu_é" }
    })
    .to_string();
    let (status, val) = raw_post(
        addr,
        &[
            ("Mcp-Method", "resources/read"),
            ("Mcp-Name", "memo://unicode/inconnu_é"),
        ],
        &body_unk,
    );
    assert_eq!(status, 200);
    assert_eq!(
        val["error"]["code"], -32602,
        "Socket: URI unicode inconnue doit être -32602: {val}"
    );

    // 3. En-tête différent -> -32020
    let (status, val) = raw_post(
        addr,
        &[
            ("Mcp-Method", "resources/read"),
            ("Mcp-Name", "memo://unicode/différent"),
        ],
        &body,
    );
    assert_eq!(status, 400);
    assert_eq!(
        val["error"]["code"], -32020,
        "Socket: Mcp-Name unicode divergent doit être -32020: {val}"
    );

    // 4. Méthode sans nom avec Mcp-Name unicode -> -32020
    let body_list = json!({
        "jsonrpc": "2.0",
        "id": 13,
        "method": "resources/list"
    })
    .to_string();
    let (status, val) = raw_post(
        addr,
        &[("Mcp-Method", "resources/list"), ("Mcp-Name", UNICODE_URI)],
        &body_list,
    );
    assert_eq!(status, 400);
    assert_eq!(
        val["error"]["code"], -32020,
        "Socket: Mcp-Name unicode sur resources/list doit être -32020: {val}"
    );
}

#[test]
fn socket_non_regression_tools_and_prompts() {
    let addr = spawn_socket_server();

    // tools/call nominal
    let body_tool = json!({
        "jsonrpc": "2.0",
        "id": 20,
        "method": "tools/call",
        "params": { "name": "echo", "arguments": { "msg": "hello" } }
    })
    .to_string();
    let (status, val) = raw_post(
        addr,
        &[("Mcp-Method", "tools/call"), ("Mcp-Name", "echo")],
        &body_tool,
    );
    assert_eq!(status, 200);
    assert_eq!(val["result"]["structuredContent"]["msg"], "hello");

    // tools/call divergent -> -32020
    let (status, val) = raw_post(
        addr,
        &[("Mcp-Method", "tools/call"), ("Mcp-Name", "autre")],
        &body_tool,
    );
    assert_eq!(status, 400);
    assert_eq!(val["error"]["code"], -32020);

    // prompts/get nominal
    let body_prompt = json!({
        "jsonrpc": "2.0",
        "id": 21,
        "method": "prompts/get",
        "params": { "name": "greeting", "arguments": {} }
    })
    .to_string();
    let (status, val) = raw_post(
        addr,
        &[("Mcp-Method", "prompts/get"), ("Mcp-Name", "greeting")],
        &body_prompt,
    );
    assert_eq!(status, 200);
    assert_eq!(val["result"]["messages"][0]["content"]["text"], "Bonjour!");

    // prompts/get divergent -> -32020
    let (status, val) = raw_post(
        addr,
        &[("Mcp-Method", "prompts/get"), ("Mcp-Name", "autre")],
        &body_prompt,
    );
    assert_eq!(status, 400);
    assert_eq!(val["error"]["code"], -32020);

    // tools/list avec Mcp-Name -> -32020
    let body_list = json!({
        "jsonrpc": "2.0",
        "id": 22,
        "method": "tools/list"
    })
    .to_string();
    let (status, val) = raw_post(
        addr,
        &[("Mcp-Method", "tools/list"), ("Mcp-Name", "echo")],
        &body_list,
    );
    assert_eq!(status, 400);
    assert_eq!(val["error"]["code"], -32020);
}
