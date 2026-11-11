//! Tests de conformité stricte MCP 2026-07-28 : métadonnées, statuts HTTP, profils.

use litemcp::protocol::{META_CLIENT_CAPABILITIES, META_PROTOCOL_VERSION, PROTOCOL_VERSION};
use litemcp::{ErrorObject, ProtocolProfile, ServerBuilder};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};

fn spawn_test_server(profile: ProtocolProfile) -> SocketAddr {
    let bound = ServerBuilder::new("strict-srv", "1.0.0")
        .protocol_profile(profile)
        .tool("echo", "Echo input", |args| Ok(json!({ "echo": args })))
        .tool("fail", "Fails", |_args| {
            Err(litemcp::error::ToolError::invalid_input("tool failed"))
        })
        .require_client_capability("special_tool", "experimental.feature")
        .tool("special_tool", "Requires capability", |_args| {
            Ok(json!({ "status": "ok" }))
        })
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = bound.local_addr().unwrap();
    std::thread::spawn(move || bound.serve().unwrap());
    addr
}

fn parse_response(raw: &str) -> (u16, String, String) {
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw, ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let content_type = head
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("content-type:"))
        .map(|line| line.split_once(':').unwrap().1.trim().to_string())
        .unwrap_or_default();
    (status, content_type, body.to_string())
}

fn raw_request(addr: SocketAddr, request: &str) -> (u16, String, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    parse_response(&raw)
}

fn send_post(
    addr: SocketAddr,
    body: &str,
    accept: &str,
    mcp_method: Option<&str>,
    mcp_name: Option<&str>,
) -> (u16, String, String) {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let body_method = parsed
        .as_ref()
        .and_then(|v| v.get("method"))
        .and_then(Value::as_str);
    let method_to_send = mcp_method.or(body_method);

    let mut headers = String::new();
    let meta_version = parsed
        .as_ref()
        .and_then(|v| v.get("params"))
        .and_then(|p| p.get("_meta"))
        .and_then(|m| m.get(META_PROTOCOL_VERSION))
        .and_then(Value::as_str)
        .unwrap_or(PROTOCOL_VERSION);
    headers.push_str(&format!("MCP-Protocol-Version: {meta_version}\r\n"));

    if let Some(m) = method_to_send {
        headers.push_str(&format!("Mcp-Method: {m}\r\n"));
    }

    let name_to_send = mcp_name.or_else(|| {
        if let Some(m) = method_to_send {
            if m == "tools/call" || m == "prompts/get" {
                parsed
                    .as_ref()
                    .and_then(|v| v.get("params"))
                    .and_then(|p| p.get("name"))
                    .and_then(Value::as_str)
            } else if m == "resources/read" {
                parsed
                    .as_ref()
                    .and_then(|v| v.get("params"))
                    .and_then(|p| p.get("uri"))
                    .and_then(Value::as_str)
            } else {
                None
            }
        } else {
            None
        }
    });
    if let Some(n) = name_to_send {
        headers.push_str(&format!("Mcp-Name: {n}\r\n"));
    }
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: test\r\nAccept: {accept}\r\n{headers}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    raw_request(addr, &request)
}

fn extract_json(accept: &str, body: &str) -> Value {
    if accept.contains("text/event-stream") {
        let payload = body
            .lines()
            .find(|line| line.starts_with("data: "))
            .map(|line| line.strip_prefix("data: ").unwrap())
            .unwrap_or(body);
        serde_json::from_str(payload.trim()).expect("valid SSE json payload")
    } else {
        serde_json::from_str(body.trim()).expect("valid JSON payload")
    }
}

fn valid_meta() -> Value {
    json!({
        META_PROTOCOL_VERSION: PROTOCOL_VERSION,
        META_CLIENT_CAPABILITIES: {}
    })
}

#[test]
fn test_strict_modern_missing_or_invalid_meta_json_and_sse() {
    let addr = spawn_test_server(ProtocolProfile::Strict2026_07_28);

    for accept in ["application/json", "text/event-stream"] {
        // 1. Manque total de _meta dans params
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{}}"#;
        let (status, _, resp_body) = send_post(addr, body, accept, None, None);
        assert_eq!(status, 400, "missing _meta must be HTTP 400 on {accept}");
        let json = extract_json(accept, &resp_body);
        assert_eq!(json["error"]["code"], ErrorObject::INVALID_PARAMS);

        // 2. params absent
        let body = r#"{"jsonrpc":"2.0","id":2,"method":"server/discover"}"#;
        let (status, _, resp_body) = send_post(addr, body, accept, None, None);
        assert_eq!(status, 400, "absent params must be HTTP 400 on {accept}");
        let json = extract_json(accept, &resp_body);
        assert_eq!(json["error"]["code"], ErrorObject::INVALID_PARAMS);

        // 3. _meta n'est pas un objet (string)
        let body =
            r#"{"jsonrpc":"2.0","id":3,"method":"server/discover","params":{"_meta":"invalid"}}"#;
        let (status, _, resp_body) = send_post(addr, body, accept, None, None);
        assert_eq!(status, 400, "_meta as string must be HTTP 400 on {accept}");
        let json = extract_json(accept, &resp_body);
        assert_eq!(json["error"]["code"], ErrorObject::INVALID_PARAMS);

        // 4. _meta n'est pas un objet (null)
        let body = r#"{"jsonrpc":"2.0","id":4,"method":"server/discover","params":{"_meta":null}}"#;
        let (status, _, resp_body) = send_post(addr, body, accept, None, None);
        assert_eq!(status, 400, "_meta as null must be HTTP 400 on {accept}");
        let json = extract_json(accept, &resp_body);
        assert_eq!(json["error"]["code"], ErrorObject::INVALID_PARAMS);

        // 5. _meta n'est pas un objet (tableau)
        let body = r#"{"jsonrpc":"2.0","id":5,"method":"server/discover","params":{"_meta":[]}}"#;
        let (status, _, resp_body) = send_post(addr, body, accept, None, None);
        assert_eq!(status, 400, "_meta as array must be HTTP 400 on {accept}");
        let json = extract_json(accept, &resp_body);
        assert_eq!(json["error"]["code"], ErrorObject::INVALID_PARAMS);
    }
}

#[test]
fn test_strict_modern_missing_or_invalid_protocol_version() {
    let addr = spawn_test_server(ProtocolProfile::Strict2026_07_28);

    for accept in ["application/json", "text/event-stream"] {
        // 1. Clé protocolVersion absente
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "server/discover",
            "params": {
                "_meta": {
                    META_CLIENT_CAPABILITIES: {}
                }
            }
        })
        .to_string();
        let (status, _, resp_body) = send_post(addr, &body, accept, None, None);
        assert_eq!(
            status, 400,
            "missing protocolVersion must be HTTP 400 on {accept}"
        );
        let json = extract_json(accept, &resp_body);
        assert_eq!(json["error"]["code"], ErrorObject::INVALID_PARAMS);

        // 2. protocolVersion mal typé (nombre)
        let body = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "server/discover",
            "params": {
                "_meta": {
                    META_PROTOCOL_VERSION: 2026,
                    META_CLIENT_CAPABILITIES: {}
                }
            }
        })
        .to_string();
        let (status, _, resp_body) = send_post(addr, &body, accept, None, None);
        assert_eq!(
            status, 400,
            "protocolVersion as number must be HTTP 400 on {accept}"
        );
        let json = extract_json(accept, &resp_body);
        assert_eq!(json["error"]["code"], ErrorObject::INVALID_PARAMS);

        // 3. protocolVersion non supporté
        let body = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "server/discover",
            "params": {
                "_meta": {
                    META_PROTOCOL_VERSION: "9999-99-99",
                    META_CLIENT_CAPABILITIES: {}
                }
            }
        })
        .to_string();
        let (status, _, resp_body) = send_post(addr, &body, accept, None, None);
        assert_eq!(
            status, 400,
            "unsupported protocolVersion must be HTTP 400 on {accept}"
        );
        let json = extract_json(accept, &resp_body);
        assert_eq!(
            json["error"]["code"],
            ErrorObject::UNSUPPORTED_PROTOCOL_VERSION
        );
        assert_eq!(json["error"]["data"]["requested"], "9999-99-99");
        assert!(json["error"]["data"]["supported"].is_array());
    }
}

#[test]
fn test_strict_modern_missing_or_invalid_client_capabilities() {
    let addr = spawn_test_server(ProtocolProfile::Strict2026_07_28);

    for accept in ["application/json", "text/event-stream"] {
        // 1. Clé clientCapabilities absente
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "server/discover",
            "params": {
                "_meta": {
                    META_PROTOCOL_VERSION: PROTOCOL_VERSION
                }
            }
        })
        .to_string();
        let (status, _, resp_body) = send_post(addr, &body, accept, None, None);
        assert_eq!(
            status, 400,
            "missing clientCapabilities must be HTTP 400 on {accept}"
        );
        let json = extract_json(accept, &resp_body);
        assert_eq!(json["error"]["code"], ErrorObject::INVALID_PARAMS);

        // 2. clientCapabilities mal typé (string)
        let body = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "server/discover",
            "params": {
                "_meta": {
                    META_PROTOCOL_VERSION: PROTOCOL_VERSION,
                    META_CLIENT_CAPABILITIES: "invalid"
                }
            }
        })
        .to_string();
        let (status, _, resp_body) = send_post(addr, &body, accept, None, None);
        assert_eq!(
            status, 400,
            "clientCapabilities as string must be HTTP 400 on {accept}"
        );
        let json = extract_json(accept, &resp_body);
        assert_eq!(json["error"]["code"], ErrorObject::INVALID_PARAMS);

        // 3. clientCapabilities mal typé (tableau)
        let body = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "server/discover",
            "params": {
                "_meta": {
                    META_PROTOCOL_VERSION: PROTOCOL_VERSION,
                    META_CLIENT_CAPABILITIES: []
                }
            }
        })
        .to_string();
        let (status, _, resp_body) = send_post(addr, &body, accept, None, None);
        assert_eq!(
            status, 400,
            "clientCapabilities as array must be HTTP 400 on {accept}"
        );
        let json = extract_json(accept, &resp_body);
        assert_eq!(json["error"]["code"], ErrorObject::INVALID_PARAMS);
    }
}

#[test]
fn test_strict_modern_rejects_null_id() {
    let addr = spawn_test_server(ProtocolProfile::Strict2026_07_28);

    for accept in ["application/json", "text/event-stream"] {
        let body = json!({
            "jsonrpc": "2.0",
            "id": null,
            "method": "server/discover",
            "params": {
                "_meta": valid_meta()
            }
        })
        .to_string();
        let (status, _, resp_body) = send_post(addr, &body, accept, None, None);
        assert_eq!(
            status, 400,
            "null id in modern profile must be HTTP 400 on {accept}"
        );
        let json = extract_json(accept, &resp_body);
        assert_eq!(json["error"]["code"], ErrorObject::INVALID_REQUEST);
        assert_eq!(json["id"], Value::Null);
    }
}

#[test]
fn test_strict_modern_required_client_capability_missing() {
    let addr = spawn_test_server(ProtocolProfile::Strict2026_07_28);

    for accept in ["application/json", "text/event-stream"] {
        // Appel d'un outil nécessitant la capacité "experimental.feature"
        // alors que le client n'a pas déclaré cette capacité
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "_meta": {
                    META_PROTOCOL_VERSION: PROTOCOL_VERSION,
                    META_CLIENT_CAPABILITIES: { "other_capability": true }
                },
                "name": "special_tool",
                "arguments": {}
            }
        })
        .to_string();
        let (status, _, resp_body) = send_post(addr, &body, accept, None, None);
        assert_eq!(
            status, 400,
            "missing required client capability must be HTTP 400 on {accept}"
        );
        let json = extract_json(accept, &resp_body);
        assert_eq!(
            json["error"]["code"],
            ErrorObject::CLIENT_CAPABILITY_REQUIRED
        );
        assert_eq!(ErrorObject::CLIENT_CAPABILITY_REQUIRED, -32021);
        // SEP-2575 : `requiredCapabilities` est un objet indexé par la
        // capacité manquante (chaque valeur étant elle-même un objet), pas
        // une liste de noms — forme validée par le scénario officiel
        // `server-stateless` (check `sep-2575-server-rejects-undeclared-capability`).
        assert_eq!(
            json["error"]["data"]["requiredCapabilities"],
            json!({ "experimental.feature": {} })
        );

        // Appel avec la capacité déclarée -> succès 200
        let body_ok = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "_meta": {
                    META_PROTOCOL_VERSION: PROTOCOL_VERSION,
                    META_CLIENT_CAPABILITIES: {
                        "experimental": { "feature": true }
                    }
                },
                "name": "special_tool",
                "arguments": {}
            }
        })
        .to_string();
        let (status_ok, _, resp_body_ok) = send_post(addr, &body_ok, accept, None, None);
        assert_eq!(
            status_ok, 200,
            "with capability must be HTTP 200 on {accept}"
        );
        let json_ok = extract_json(accept, &resp_body_ok);
        assert!(json_ok["result"].is_object());
    }
}

#[test]
fn test_strict_modern_removed_and_unknown_methods_return_404_and_32601() {
    let addr = spawn_test_server(ProtocolProfile::Strict2026_07_28);

    let methods_to_test = [
        "initialize",
        "ping",
        "logging/setLevel",
        "resources/subscribe",
        "custom/unknownArbitraryMethod",
    ];

    for accept in ["application/json", "text/event-stream"] {
        for method in methods_to_test {
            let body = json!({
                "jsonrpc": "2.0",
                "id": 42,
                "method": method,
                "params": {
                    "_meta": valid_meta()
                }
            })
            .to_string();
            let (status, _, resp_body) = send_post(addr, &body, accept, None, None);
            assert_eq!(
                status, 404,
                "method `{method}` must be HTTP 404 on {accept}, got {status}: {resp_body}"
            );
            let json = extract_json(accept, &resp_body);
            assert_eq!(
                json["error"]["code"],
                ErrorObject::METHOD_NOT_FOUND,
                "method `{method}` must return -32601 on {accept}"
            );
            assert_eq!(json["id"], 42);
        }
    }
}

#[test]
fn test_legacy_profile_serves_initialize_ping_and_tolerates_missing_meta_and_null_id() {
    let addr = spawn_test_server(ProtocolProfile::Legacy);

    // 1. initialize sans _meta est servi en 200
    let body_init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let (status, _, resp) = send_post(addr, body_init, "application/json", None, None);
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["serverInfo"]["name"], "strict-srv");

    // 2. ping sans _meta est servi en 200
    let body_ping = r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#;
    let (status, _, resp) = send_post(addr, body_ping, "application/json", None, None);
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["resultType"], "complete");

    // 3. id: null est accepté en legacy
    let body_null = r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#;
    let (status, _, resp) = send_post(addr, body_null, "application/json", None, None);
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["id"], Value::Null);

    // 4. Une méthode inconnue reste 404 / -32601
    let body_unk = r#"{"jsonrpc":"2.0","id":3,"method":"unknown/foo"}"#;
    let (status, _, resp) = send_post(addr, body_unk, "application/json", None, None);
    assert_eq!(status, 404);
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"]["code"], ErrorObject::METHOD_NOT_FOUND);
}

#[test]
fn test_strict_modern_nominal_flow_200_and_tool_error_200() {
    let addr = spawn_test_server(ProtocolProfile::Strict2026_07_28);

    for accept in ["application/json", "text/event-stream"] {
        // server/discover avec métadonnées valides -> 200
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "server/discover",
            "params": {
                "_meta": valid_meta()
            }
        })
        .to_string();
        let (status, _, resp_body) = send_post(addr, &body, accept, None, None);
        assert_eq!(status, 200);
        let json = extract_json(accept, &resp_body);
        assert!(json["result"]["capabilities"].is_object());

        // tools/list avec métadonnées valides -> 200
        let body = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {
                "_meta": valid_meta()
            }
        })
        .to_string();
        let (status, _, resp_body) = send_post(addr, &body, accept, None, None);
        assert_eq!(status, 200);
        let json = extract_json(accept, &resp_body);
        assert!(json["result"]["tools"].is_array());

        // tools/call nominal -> 200
        let body = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "_meta": valid_meta(),
                "name": "echo",
                "arguments": { "val": 42 }
            }
        })
        .to_string();
        let (status, _, resp_body) = send_post(addr, &body, accept, None, None);
        assert_eq!(status, 200);
        let json = extract_json(accept, &resp_body);
        assert_eq!(json["result"]["structuredContent"]["echo"]["val"], 42);

        // tool error (isError: true) reste 200 en HTTP MCP
        let body = json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "_meta": valid_meta(),
                "name": "fail",
                "arguments": {}
            }
        })
        .to_string();
        let (status, _, resp_body) = send_post(addr, &body, accept, None, None);
        assert_eq!(status, 200);
        let json = extract_json(accept, &resp_body);
        assert_eq!(json["result"]["isError"], true);
    }
}
