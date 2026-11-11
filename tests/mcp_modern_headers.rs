//! Tests de conformité MCP pour les en-têtes obligatoires sur le profil moderne (SEP-2243, SEP-2575).
//!
//! Couvre :
//! - `MCP-Protocol-Version`, `Mcp-Method` et `Mcp-Name` requis sur le profil moderne
//! - Refus des doublons de `MCP-Protocol-Version` (HTTP 400)
//! - Correspondance exacte en-tête / corps (HTTP 400 + -32020)
//! - Noms d'en-têtes insensibles à la casse (ex. `mcp-method`, `MCP-METHOD`)
//! - Découpage strict OWS
//! - Décodage Base64 `=?base64?...?=` pour `Mcp-Name` et `Mcp-Param-*` :
//!   - Base64 valide
//!   - Base64 invalide (padding incorrect, caractères hors alphabet) -> 400 + -32020
//!   - Sentinelles littérales (sans préfixe `=?base64?` ou sans suffixe `?=`)
//! - Paramètres `x-mcp-header` (`Mcp-Param-*`) :
//!   - Types autorisés (string, integer, boolean) et refus statique des types interdits (`number`)
//!   - Absence obligatoire du header si argument null ou absent
//!   - Présence obligatoire du header si argument présent
//!   - Comparaison numérique pour les entiers (ex. 42 vs 42.0)

use litemcp::protocol::{
    META_CLIENT_CAPABILITIES, META_PROTOCOL_VERSION, PROTOCOL_VERSION, ResourceContents,
};
use litemcp::{ProtocolProfile, ServerBuilder, zod};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

fn valid_meta() -> Value {
    json!({
        META_PROTOCOL_VERSION: PROTOCOL_VERSION,
        META_CLIENT_CAPABILITIES: {}
    })
}

fn spawn_modern_server() -> SocketAddr {
    let bound = ServerBuilder::new("modern-test-srv", "1.0.0")
        .protocol_profile(ProtocolProfile::Strict2026_07_28)
        .tool("echo", "Echo input", |args| Ok(args.clone()))
        .tool_typed(
            "custom_tool",
            "Tool with x-mcp-header parameters",
            zod::object()
                .field("str_param", zod::string().header("str-h"))
                .field("int_param", zod::integer().header("int-h"))
                .field("bool_param", zod::boolean().header("bool-h"))
                .opt("opt_param", zod::string().header("opt-h")),
            |args| Ok(args.clone()),
        )
        .resource("memo://test", "test_res", "Test resource", || {
            Ok(ResourceContents {
                uri: "memo://test".to_string(),
                mime_type: Some("text/plain".to_string()),
                text: "contenu test".to_string(),
            })
        })
        .resource("memo://café", "cafe_res", "Cafe resource", || {
            Ok(ResourceContents {
                uri: "memo://café".to_string(),
                mime_type: Some("text/plain".to_string()),
                text: "contenu café".to_string(),
            })
        })
        .bind("127.0.0.1:0")
        .expect("bind modern server");
    let addr = bound.local_addr().expect("local_addr");
    std::thread::spawn(move || {
        let _ = bound.serve();
    });
    addr
}

fn raw_exchange(addr: SocketAddr, raw_request: &str) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout");
    stream.write_all(raw_request.as_bytes()).expect("write");
    stream.flush().expect("flush");

    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    let s = String::from_utf8_lossy(&buf).to_string();
    let (head, body) = s.split_once("\r\n\r\n").unwrap_or((&s, ""));
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let val: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    (status, val)
}

fn send_modern_post(addr: SocketAddr, headers: &[(&str, &str)], body: &str) -> (u16, Value) {
    let mut req = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    raw_exchange(addr, &req)
}

// ============================================================================
// 1. Tests des en-têtes standards requis sur le profil moderne
// ============================================================================

#[test]
fn modern_profile_rejects_missing_protocol_version_header() {
    let addr = spawn_modern_server();
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "server/discover",
        "params": { "_meta": valid_meta() }
    })
    .to_string();

    let (status, val) = send_modern_post(addr, &[("Mcp-Method", "server/discover")], &body);
    assert_eq!(status, 400);
    assert_eq!(val["error"]["code"], -32020);
}

#[test]
fn modern_profile_rejects_duplicate_protocol_version_header() {
    let addr = spawn_modern_server();
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "server/discover",
        "params": { "_meta": valid_meta() }
    })
    .to_string();

    let raw = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nMCP-Protocol-Version: 2026-07-28\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: server/discover\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, _) = raw_exchange(addr, &raw);
    assert_eq!(
        status, 400,
        "duplicate MCP-Protocol-Version must be rejected with HTTP 400"
    );
}

#[test]
fn modern_profile_rejects_mismatched_protocol_version_header() {
    let addr = spawn_modern_server();
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "server/discover",
        "params": { "_meta": valid_meta() }
    })
    .to_string();

    let (status, val) = send_modern_post(
        addr,
        &[
            ("MCP-Protocol-Version", "2025-11-25"),
            ("Mcp-Method", "server/discover"),
        ],
        &body,
    );
    assert_eq!(status, 400);
    assert_eq!(val["error"]["code"], -32020);
}

#[test]
fn modern_profile_rejects_missing_mcp_method_header() {
    let addr = spawn_modern_server();
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "server/discover",
        "params": { "_meta": valid_meta() }
    })
    .to_string();

    let (status, val) = send_modern_post(addr, &[("MCP-Protocol-Version", "2026-07-28")], &body);
    assert_eq!(status, 400);
    assert_eq!(val["error"]["code"], -32020);
}

#[test]
fn modern_profile_rejects_mismatched_mcp_method_header() {
    let addr = spawn_modern_server();
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "server/discover",
        "params": { "_meta": valid_meta() }
    })
    .to_string();

    let (status, val) = send_modern_post(
        addr,
        &[
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/list"),
        ],
        &body,
    );
    assert_eq!(status, 400);
    assert_eq!(val["error"]["code"], -32020);
}

#[test]
fn modern_profile_rejects_missing_mcp_name_header_on_named_methods() {
    let addr = spawn_modern_server();
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "_meta": valid_meta(),
            "name": "echo",
            "arguments": {}
        }
    })
    .to_string();

    // tools/call sans Mcp-Name -> rejet 400 + -32020
    let (status, val) = send_modern_post(
        addr,
        &[
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/call"),
        ],
        &body,
    );
    assert_eq!(status, 400);
    assert_eq!(val["error"]["code"], -32020);
}

#[test]
fn modern_profile_rejects_mcp_name_on_unnamed_methods() {
    let addr = spawn_modern_server();
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "server/discover",
        "params": { "_meta": valid_meta() }
    })
    .to_string();

    let (status, val) = send_modern_post(
        addr,
        &[
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "server/discover"),
            ("Mcp-Name", "unexpected"),
        ],
        &body,
    );
    assert_eq!(status, 400);
    assert_eq!(val["error"]["code"], -32020);
}

// ============================================================================
// 2. Tests casse des noms d'en-têtes et OWS
// ============================================================================

#[test]
fn modern_profile_accepts_case_insensitive_header_names_and_ows() {
    let addr = spawn_modern_server();
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "_meta": valid_meta(),
            "name": "echo",
            "arguments": { "msg": "hello" }
        }
    })
    .to_string();

    // En-têtes en minuscules et majuscules avec espaces OWS autour des valeurs
    let (status, val) = send_modern_post(
        addr,
        &[
            ("mcp-protocol-version", "  2026-07-28 \t"),
            ("MCP-METHOD", "\ttools/call  "),
            ("mcp-name", "  echo  "),
        ],
        &body,
    );
    assert_eq!(
        status, 200,
        "OWS and case-insensitive headers must succeed: {val}"
    );
    assert_eq!(val["result"]["structuredContent"]["msg"], "hello");
}

// ============================================================================
// 3. Tests Base64 pour Mcp-Name et ressources
// ============================================================================

#[test]
fn modern_profile_mcp_name_base64_valid_and_resources_read() {
    let addr = spawn_modern_server();

    // 1. Nominal ASCII avec Base64 valide pour Mcp-Name : "echo" en Base64 = "ZWNoby=="
    let body_tool = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "_meta": valid_meta(),
            "name": "echo",
            "arguments": {}
        }
    })
    .to_string();
    let (status, val) = send_modern_post(
        addr,
        &[
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "=?base64?ZWNoby==?="),
        ],
        &body_tool,
    );
    assert_eq!(status, 200, "Base64 encoded Mcp-Name must match: {val}");

    // 2. Resource avec URI accentuée et Mcp-Name en Base64
    // "memo://café" -> b"memo://caf\xc3\xa9" -> Base64 = "bWVtby8vY2Fmw6k="
    let body_res = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "resources/read",
        "params": {
            "_meta": valid_meta(),
            "uri": "memo://café"
        }
    })
    .to_string();
    let (status, val) = send_modern_post(
        addr,
        &[
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "resources/read"),
            ("Mcp-Name", "=?base64?bWVtbzovL2NhZsOp?="),
        ],
        &body_res,
    );
    assert_eq!(status, 200, "Base64 encoded URI resource must match: {val}");
    assert_eq!(val["result"]["contents"][0]["text"], "contenu café");

    // 3. Base64 invalide (padding erroné) -> 400 + -32020
    let (status, val) = send_modern_post(
        addr,
        &[
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "=?base64?ZWNoby?="), // padding manquant (len 6 au lieu de multiple de 4)
        ],
        &body_tool,
    );
    assert_eq!(status, 400);
    assert_eq!(val["error"]["code"], -32020);

    // 4. Base64 invalide (caractères hors alphabet) -> 400 + -32020
    let (status, val) = send_modern_post(
        addr,
        &[
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "=?base64?ZW!Noby==?="),
        ],
        &body_tool,
    );
    assert_eq!(status, 400);
    assert_eq!(val["error"]["code"], -32020);

    // 5. Sentinelle littérale (sans préfixe =?base64?) -> traité comme littéral "ZWNoby=="
    let (status, val) = send_modern_post(
        addr,
        &[
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "ZWNoby=="), // littéral ne correspondant pas à "echo"
        ],
        &body_tool,
    );
    assert_eq!(status, 400);
    assert_eq!(val["error"]["code"], -32020);

    // 6. Sentinelle littérale (sans suffixe ?=) -> traité comme littéral
    let (status, val) = send_modern_post(
        addr,
        &[
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "=?base64?ZWNoby=="),
        ],
        &body_tool,
    );
    assert_eq!(status, 400);
    assert_eq!(val["error"]["code"], -32020);
}

// ============================================================================
// 4. Tests x-mcp-header (Mcp-Param-*)
// ============================================================================

#[test]
fn modern_profile_custom_headers_nominal_and_integer_42_vs_42_0() {
    let addr = spawn_modern_server();

    // 1. Nominal avec tous les paramètres requis : string, integer (42), boolean (true)
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "_meta": valid_meta(),
            "name": "custom_tool",
            "arguments": {
                "str_param": "hello world",
                "int_param": 42,
                "bool_param": true
            }
        }
    })
    .to_string();

    let (status, val) = send_modern_post(
        addr,
        &[
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "custom_tool"),
            ("Mcp-Param-str-h", "hello world"),
            ("Mcp-Param-int-h", "42"),
            ("Mcp-Param-bool-h", "true"),
        ],
        &body,
    );
    assert_eq!(
        status, 200,
        "custom headers nominal call must succeed: {val}"
    );

    // 2. Entier 42.0 dans le corps JSON comparé à "42" dans l'en-tête (équivalence numérique)
    let body_float_int = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "_meta": valid_meta(),
            "name": "custom_tool",
            "arguments": {
                "str_param": "hello world",
                "int_param": 42.0,
                "bool_param": true
            }
        }
    })
    .to_string();

    let (status, val) = send_modern_post(
        addr,
        &[
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "custom_tool"),
            ("Mcp-Param-str-h", "hello world"),
            ("Mcp-Param-int-h", "42"),
            ("Mcp-Param-bool-h", "true"),
        ],
        &body_float_int,
    );
    assert_eq!(
        status, 200,
        "42.0 in JSON body matches 42 in Mcp-Param: {val}"
    );

    // 3. String avec Base64 dans Mcp-Param
    // "hello world" -> Base64 = "aGVsbG8gd29ybGQ="
    let (status, val) = send_modern_post(
        addr,
        &[
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "custom_tool"),
            ("Mcp-Param-str-h", "=?base64?aGVsbG8gd29ybGQ=?="),
            ("Mcp-Param-int-h", "42"),
            ("Mcp-Param-bool-h", "true"),
        ],
        &body,
    );
    assert_eq!(status, 200, "Base64 in Mcp-Param must succeed: {val}");
}

#[test]
fn modern_profile_custom_headers_rejections() {
    let addr = spawn_modern_server();

    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "_meta": valid_meta(),
            "name": "custom_tool",
            "arguments": {
                "str_param": "hello",
                "int_param": 42,
                "bool_param": true
            }
        }
    })
    .to_string();

    // 1. En-tête manquant alors que l'argument est présent dans le corps -> 400 + -32020
    let (status, val) = send_modern_post(
        addr,
        &[
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "custom_tool"),
            // Mcp-Param-str-h omis
            ("Mcp-Param-int-h", "42"),
            ("Mcp-Param-bool-h", "true"),
        ],
        &body,
    );
    assert_eq!(status, 400);
    assert_eq!(val["error"]["code"], -32020);

    // 2. En-tête présent alors que l'argument est absent/null dans le corps -> 400 + -32020
    let (status, val) = send_modern_post(
        addr,
        &[
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "custom_tool"),
            ("Mcp-Param-str-h", "hello"),
            ("Mcp-Param-int-h", "42"),
            ("Mcp-Param-bool-h", "true"),
            ("Mcp-Param-opt-h", "unexpected_present"), // opt_param est absent dans body
        ],
        &body,
    );
    assert_eq!(status, 400);
    assert_eq!(val["error"]["code"], -32020);

    // 3. Valeur d'en-tête divergente de l'argument -> 400 + -32020
    let (status, val) = send_modern_post(
        addr,
        &[
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "custom_tool"),
            ("Mcp-Param-str-h", "wrong_val"),
            ("Mcp-Param-int-h", "42"),
            ("Mcp-Param-bool-h", "true"),
        ],
        &body,
    );
    assert_eq!(status, 400);
    assert_eq!(val["error"]["code"], -32020);
}

/// Argument **null** explicite (pas seulement absent) : le header doit être
/// absent — et un appel nominal sans header pour un argument null réussit.
#[test]
fn modern_profile_custom_headers_explicit_null_argument() {
    let addr = spawn_modern_server();

    // 1. Header présent + argument explicitement null -> 400 + -32020
    //    (la présence du header prétend transporter une valeur qui n'existe pas).
    let body_null = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "_meta": valid_meta(),
            "name": "custom_tool",
            "arguments": {
                "str_param": "hello",
                "int_param": 42,
                "bool_param": true,
                "opt_param": null
            }
        }
    })
    .to_string();
    let (status, val) = send_modern_post(
        addr,
        &[
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "custom_tool"),
            ("Mcp-Param-str-h", "hello"),
            ("Mcp-Param-int-h", "42"),
            ("Mcp-Param-bool-h", "true"),
            ("Mcp-Param-opt-h", "present_for_null"),
        ],
        &body_null,
    );
    assert_eq!(
        status, 400,
        "header sur argument null doit être refusé : {val}"
    );
    assert_eq!(val["error"]["code"], -32020);

    // 2. Header ABSENT + argument null -> nominal (l'absence est obligatoire).
    let (status, val) = send_modern_post(
        addr,
        &[
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "custom_tool"),
            ("Mcp-Param-str-h", "hello"),
            ("Mcp-Param-int-h", "42"),
            ("Mcp-Param-bool-h", "true"),
        ],
        &body_null,
    );
    assert_eq!(
        status, 200,
        "argument null sans header doit être accepté : {val}"
    );
    assert!(
        val["result"]["structuredContent"]
            .get("opt_param")
            .is_none(),
        "le null reste absent des arguments transmis : {val}"
    );
}

#[test]
#[should_panic(expected = "number")]
fn registration_panics_on_number_type_with_x_mcp_header() {
    struct RawSchema(Value);
    impl zod::Schema for RawSchema {
        fn json(&self) -> Value {
            self.0.clone()
        }
        fn validate(&self, _: &mut zod::Path, _: &Value, _: &mut zod::Issues) {}
    }

    let _ = ServerBuilder::new("test", "1.0.0").tool_typed(
        "bad_tool",
        "Bad number tool",
        RawSchema(json!({
            "type": "object",
            "properties": {
                "n": {
                    "type": "number",
                    "x-mcp-header": "num-h"
                }
            }
        })),
        |_| Ok(json!({})),
    );
}
