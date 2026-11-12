//! Test de régression de la fixture de conformance officielle MCP
//! **2026-07-28** — fige la surface `test_*` que la suite
//! `modelcontextprotocol/conformance` (`--requirements 2026-07-28`) note,
//! **sans** dépendre du harness npm.
//!
//! Avant l'existence de la fixture (et si un objet `test_*` disparaît ou se
//! déforme), ce test échoue : toute régression de la lib sur un chemin scoré
//! est donc visible dès `cargo test`, pas seulement au run officiel.
//!
//! Le serveur testé est **exactement** celui du binaire
//! `examples/conformance_server` (module `fixture.rs` inclus via `#[path]` —
//! une seule source de vérité). Les requêtes suivent l'idiome moderne
//! 2026-07-28 : `_meta` complet dans `params`, en-têtes `MCP-Protocol-Version`
//! / `Mcp-Method` / `Mcp-Name` — comme la suite officielle.

#[path = "../examples/conformance_server/fixture.rs"]
mod fixture;

use litemcp::protocol::{META_CLIENT_CAPABILITIES, META_PROTOCOL_VERSION, PROTOCOL_VERSION};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicI64, Ordering};

/// Outils exigés par les scénarios scorés de `requirements/2026-07-28.yaml`
/// (copie gelée du harness `@modelcontextprotocol/conformance@0.2.0-alpha.12`).
const SCORED_TOOLS: &[&str] = &[
    "test_simple_text",
    "test_image_content",
    "test_audio_content",
    "test_embedded_resource",
    "test_multiple_content_types",
    "test_error_handling",
    "test_tool_with_progress",
    "test_streaming_elicitation",
    "test_logging_tool",
    "test_missing_capability",
    // Scénarios `input-required-result-*` (SEP-2322) — cycle MRTR.
    "test_input_required_result_elicitation",
    "test_input_required_result_sampling",
    "test_input_required_result_list_roots",
    "test_input_required_result_request_state",
    "test_input_required_result_multiple_inputs",
    "test_input_required_result_multi_round",
    "test_input_required_result_tampered_state",
    "test_input_required_result_capabilities",
];

const SCORED_PROMPTS: &[&str] = &[
    "test_simple_prompt",
    "test_prompt_with_arguments",
    "test_prompt_with_embedded_resource",
    "test_prompt_with_image",
    // `prompts/get` supporte le MRTR (scénario non-tool-request).
    "test_input_required_result_prompt",
];

const SCORED_RESOURCES: &[&str] = &[
    "test://static-text",
    "test://static-binary",
    "test://template/123/data",
];

fn spawn_fixture() -> SocketAddr {
    let bound = fixture::build_fixture("127.0.0.1:0").expect("la fixture doit se lier");
    let addr = bound.local_addr().expect("adresse locale connue");
    std::thread::spawn(move || bound.serve().expect("boucle de service"));
    addr
}

fn next_id() -> i64 {
    static ID: AtomicI64 = AtomicI64::new(1);
    ID.fetch_add(1, Ordering::Relaxed)
}

/// Requête POST moderne valide (headers déduits du corps, comme la suite).
fn rpc(addr: SocketAddr, method: &str, params: Value) -> (u16, Value) {
    rpc_with_caps(addr, method, params, json!({}))
}

/// Variante avec `clientCapabilities` explicites dans `_meta`.
fn rpc_with_caps(addr: SocketAddr, method: &str, params: Value, caps: Value) -> (u16, Value) {
    let mut params = params;
    let obj = params.as_object_mut().expect("params objet");
    obj.entry("_meta").or_insert_with(|| {
        json!({
            META_PROTOCOL_VERSION: PROTOCOL_VERSION,
            META_CLIENT_CAPABILITIES: caps,
        })
    });
    let body = json!({
        "jsonrpc": "2.0",
        "id": next_id(),
        "method": method,
        "params": params,
    })
    .to_string();

    let mut headers =
        format!("MCP-Protocol-Version: {PROTOCOL_VERSION}\r\nMcp-Method: {method}\r\n");
    let name = match method {
        "tools/call" | "prompts/get" => params.get("name").and_then(Value::as_str),
        "resources/read" => params.get("uri").and_then(Value::as_str),
        _ => None,
    };
    if let Some(n) = name {
        headers.push_str(&format!("Mcp-Name: {n}\r\n"));
    }

    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: application/json\r\n\
         {headers}Content-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, payload) = post_raw(addr, &request);
    let json: Value = serde_json::from_str(payload.trim()).expect("réponse JSON-RPC exploitable");
    (status, json)
}

fn post_raw(addr: SocketAddr, request: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).expect("connexion TCP à la fixture");
    stream
        .write_all(request.as_bytes())
        .expect("écriture requête");
    let mut raw = String::new();
    stream.read_to_string(&mut raw).expect("lecture réponse");
    let (head, body) = raw.split_once("\r\n\r\n").expect("réponse HTTP complète");
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(0);
    (status, body.to_string())
}

fn tools_call(name: &str, arguments: Value) -> Value {
    json!({ "name": name, "arguments": arguments })
}

fn assert_result_ok(method: &str, status: u16, v: &Value) {
    assert_eq!(
        v.get("error"),
        None,
        "{method} doit réussir sans erreur : {v}"
    );
    assert!(
        v["result"].is_object(),
        "{method} doit renvoyer un objet result : {v}"
    );
    assert_eq!(status, 200, "{method} doit répondre HTTP 200");
}

#[test]
fn health_probe_is_served() {
    let addr = spawn_fixture();
    let request = "GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
    let (status, _) = post_raw(addr, request);
    assert_eq!(status, 200, "le job CI attend GET /health → 200");
}

#[test]
fn discover_declares_all_three_surfaces() {
    let addr = spawn_fixture();
    let (status, v) = rpc(addr, "server/discover", json!({}));
    assert_result_ok("server/discover", status, &v);
    let caps = &v["result"]["capabilities"];
    for surface in ["tools", "resources", "prompts", "completions"] {
        assert!(
            caps[surface].is_object(),
            "capacité `{surface}` attendue dans server/discover : {v}"
        );
    }
    let versions = v["result"]["supportedVersions"]
        .as_array()
        .expect("supportedVersions présent");
    assert!(
        versions.iter().any(|x| x == PROTOCOL_VERSION),
        "2026-07-28 annoncé dans supportedVersions : {v}"
    );
}

#[test]
fn tools_list_contains_every_scored_tool() {
    let addr = spawn_fixture();
    let (status, v) = rpc(addr, "tools/list", json!({}));
    assert_result_ok("tools/list", status, &v);
    let names: Vec<&str> = v["result"]["tools"]
        .as_array()
        .expect("tools tableau")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for expected in SCORED_TOOLS {
        assert!(
            names.contains(expected),
            "l'outil scoré `{expected}` doit être annoncé par tools/list : {names:?}"
        );
    }
    // Outil diagnostique du scénario (pending) SEP-2243 : présent mais hors
    // liste scorée — sa disparition doit aussi être visible.
    assert!(
        names.contains(&"test_header_param_tool"),
        "l'outil diagnostique `test_header_param_tool` doit rester enregistré : {names:?}"
    );
}

#[test]
fn tools_call_scored_content_shapes() {
    let addr = spawn_fixture();

    // Texte simple — libellé exact attendu par `tools-call-simple-text`.
    let (status, v) = rpc(
        addr,
        "tools/call",
        tools_call("test_simple_text", json!({})),
    );
    assert_result_ok("tools/call test_simple_text", status, &v);
    assert_eq!(
        v["result"]["content"][0]["type"], "text",
        "premier bloc texte : {v}"
    );
    assert_eq!(
        v["result"]["content"][0]["text"],
        "This is a simple text response for testing."
    );

    // Image PNG — base64 + mimeType.
    let (status, v) = rpc(
        addr,
        "tools/call",
        tools_call("test_image_content", json!({})),
    );
    assert_result_ok("tools/call test_image_content", status, &v);
    assert_eq!(v["result"]["content"][0]["type"], "image");
    assert_eq!(v["result"]["content"][0]["mimeType"], "image/png");
    let data = v["result"]["content"][0]["data"]
        .as_str()
        .expect("data base64 présente");
    assert!(
        !data.is_empty() && data.is_ascii(),
        "data base64 non vide : {data}"
    );

    // Audio WAV — base64 + mimeType.
    let (status, v) = rpc(
        addr,
        "tools/call",
        tools_call("test_audio_content", json!({})),
    );
    assert_result_ok("tools/call test_audio_content", status, &v);
    assert_eq!(v["result"]["content"][0]["type"], "audio");
    assert_eq!(v["result"]["content"][0]["mimeType"], "audio/wav");

    // Ressource embarquée — uri + texte.
    let (status, v) = rpc(
        addr,
        "tools/call",
        tools_call("test_embedded_resource", json!({})),
    );
    assert_result_ok("tools/call test_embedded_resource", status, &v);
    assert_eq!(v["result"]["content"][0]["type"], "resource");
    assert_eq!(
        v["result"]["content"][0]["resource"]["uri"],
        "test://embedded-resource"
    );
    assert_eq!(
        v["result"]["content"][0]["resource"]["text"],
        "This is an embedded resource content."
    );

    // Contenus multiples — texte, image, ressource.
    let (status, v) = rpc(
        addr,
        "tools/call",
        tools_call("test_multiple_content_types", json!({})),
    );
    assert_result_ok("tools/call test_multiple_content_types", status, &v);
    let types: Vec<&str> = v["result"]["content"]
        .as_array()
        .expect("content tableau")
        .iter()
        .filter_map(|c| c["type"].as_str())
        .collect();
    assert_eq!(
        types,
        ["text", "image", "resource"],
        "trois blocs ordonnés : {v}"
    );

    // Erreur volontaire — isError=true, jamais une erreur JSON-RPC.
    let (status, v) = rpc(
        addr,
        "tools/call",
        tools_call("test_error_handling", json!({})),
    );
    assert_result_ok("tools/call test_error_handling", status, &v);
    assert_eq!(v["result"]["isError"], true, "isError attendu : {v}");

    // Progression — sans jeton, aucune notification : le flux JSON reste
    // unique (le canal request-scoped est couvert par
    // `tests/mcp_progress_channel.rs`).
    let (status, v) = rpc(
        addr,
        "tools/call",
        tools_call("test_tool_with_progress", json!({})),
    );
    assert_result_ok("tools/call test_tool_with_progress", status, &v);
    assert_ne!(v["result"]["isError"], true);

    // Diagnostiques server-stateless : succès simple.
    for name in ["test_streaming_elicitation", "test_logging_tool"] {
        let (status, v) = rpc(addr, "tools/call", tools_call(name, json!({})));
        assert_result_ok(&format!("tools/call {name}"), status, &v);
    }
}

#[test]
fn tools_call_missing_client_capability_rejected_with_32021_and_400() {
    let addr = spawn_fixture();
    // Sans la capacité `sampling` (cas scoré server-stateless / SEP-2575) :
    // -32021 + HTTP 400, pas une erreur interne.
    let (status, v) = rpc(
        addr,
        "tools/call",
        tools_call("test_missing_capability", json!({})),
    );
    assert_eq!(
        v["error"]["code"], -32021,
        "MissingRequiredClientCapabilityError attendu : {v}"
    );
    assert_eq!(status, 400, "HTTP 400 attendu pour capacité manquante");

    // Avec la capacité déclarée : la requête est servie (bascule positif).
    let (status, v) = rpc_with_caps(
        addr,
        "tools/call",
        tools_call("test_missing_capability", json!({})),
        json!({ "sampling": {} }),
    );
    assert_result_ok("tools/call test_missing_capability (sampling)", status, &v);
}

#[test]
fn tools_call_header_param_enforced_and_echoed() {
    let addr = spawn_fixture();
    let args = json!({ "str_param": "hello", "int_param": 42, "bool_param": true });

    // Les annotations `x-mcp-header` sont **appliquées** : appeler avec les
    // arguments en corps mais sans les en-têtes `Mcp-Param-*` est refusé.
    let (status, v) = rpc(
        addr,
        "tools/call",
        tools_call("test_header_param_tool", args.clone()),
    );
    assert_eq!(
        v["error"]["code"], -32020,
        "Mcp-Name/Mcp-Param manquant → -32020 : {v}"
    );
    assert_eq!(status, 400);

    // Avec les en-têtes bruts cohérents : succès, arguments écho en
    // structuredContent.
    let body = json!({
        "jsonrpc": "2.0",
        "id": next_id(),
        "method": "tools/call",
        "params": {
            "_meta": {
                META_PROTOCOL_VERSION: PROTOCOL_VERSION,
                META_CLIENT_CAPABILITIES: {},
            },
            "name": "test_header_param_tool",
            "arguments": args,
        },
    })
    .to_string();
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: application/json\r\n\
         MCP-Protocol-Version: {PROTOCOL_VERSION}\r\nMcp-Method: tools/call\r\n\
         Mcp-Name: test_header_param_tool\r\n\
         Mcp-Param-str-h: hello\r\nMcp-Param-int-h: 42\r\nMcp-Param-bool-h: true\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, payload) = post_raw(addr, &request);
    let v: Value = serde_json::from_str(payload.trim()).unwrap();
    assert_result_ok("tools/call test_header_param_tool (headers)", status, &v);
    assert_eq!(v["result"]["structuredContent"]["str_param"], "hello");
    assert_eq!(v["result"]["structuredContent"]["int_param"], 42);
    assert_eq!(v["result"]["structuredContent"]["bool_param"], true);

    // Format `=?base64?…?=` accepté (SEP-2243) : « hello » encodé.
    let body = json!({
        "jsonrpc": "2.0",
        "id": next_id(),
        "method": "tools/call",
        "params": {
            "_meta": {
                META_PROTOCOL_VERSION: PROTOCOL_VERSION,
                META_CLIENT_CAPABILITIES: {},
            },
            "name": "test_header_param_tool",
            "arguments": args,
        },
    })
    .to_string();
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: application/json\r\n\
         MCP-Protocol-Version: {PROTOCOL_VERSION}\r\nMcp-Method: tools/call\r\n\
         Mcp-Name: test_header_param_tool\r\n\
         Mcp-Param-str-h: =?base64?aGVsbG8=?=\r\nMcp-Param-int-h: 42\r\nMcp-Param-bool-h: true\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, payload) = post_raw(addr, &request);
    let v: Value = serde_json::from_str(payload.trim()).unwrap();
    assert_result_ok("tools/call test_header_param_tool (base64)", status, &v);

    // En-tête cohérent mais valeur divergente : refusé.
    let body = json!({
        "jsonrpc": "2.0",
        "id": next_id(),
        "method": "tools/call",
        "params": {
            "_meta": {
                META_PROTOCOL_VERSION: PROTOCOL_VERSION,
                META_CLIENT_CAPABILITIES: {},
            },
            "name": "test_header_param_tool",
            "arguments": { "str_param": "hello", "int_param": 42, "bool_param": true },
        },
    })
    .to_string();
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: application/json\r\n\
         MCP-Protocol-Version: {PROTOCOL_VERSION}\r\nMcp-Method: tools/call\r\n\
         Mcp-Name: test_header_param_tool\r\n\
         Mcp-Param-str-h: divergent\r\nMcp-Param-int-h: 42\r\nMcp-Param-bool-h: true\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, payload) = post_raw(addr, &request);
    let v: Value = serde_json::from_str(payload.trim()).unwrap();
    assert_eq!(
        v["error"]["code"], -32020,
        "valeur divergente → -32020 : {v}"
    );
    assert_eq!(status, 400);
}

#[test]
fn resources_list_read_and_templates_cover_scored_uris() {
    let addr = spawn_fixture();

    // Listes : les trois URI scorés + le gabarit.
    let (status, v) = rpc(addr, "resources/list", json!({}));
    assert_result_ok("resources/list", status, &v);
    let uris: Vec<String> = v["result"]["resources"]
        .as_array()
        .expect("resources tableau")
        .iter()
        .filter_map(|r| r["uri"].as_str().map(str::to_owned))
        .collect();
    for expected in SCORED_RESOURCES {
        assert!(
            uris.iter().any(|u| u == expected),
            "la ressource scorée `{expected}` doit être listée : {uris:?}"
        );
    }

    let (status, v) = rpc(addr, "resources/templates/list", json!({}));
    assert_result_ok("resources/templates/list", status, &v);
    let templates: Vec<String> = v["result"]["resourceTemplates"]
        .as_array()
        .expect("resourceTemplates tableau")
        .iter()
        .filter_map(|r| r["uriTemplate"].as_str().map(str::to_owned))
        .collect();
    assert!(
        templates.iter().any(|u| u == "test://template/{id}/data"),
        "le gabarit scoré doit être listé : {templates:?}"
    );

    // Lecture texte — contenu exact attendu par `resources-read-text`.
    let (status, v) = rpc(
        addr,
        "resources/read",
        json!({ "uri": "test://static-text" }),
    );
    assert_result_ok("resources/read static-text", status, &v);
    assert_eq!(v["result"]["contents"][0]["uri"], "test://static-text");
    assert_eq!(v["result"]["contents"][0]["mimeType"], "text/plain");
    assert_eq!(
        v["result"]["contents"][0]["text"],
        "This is the content of the static text resource."
    );

    // Lecture binaire — `blob` base64 + mimeType (resources-read-binary).
    let (status, v) = rpc(
        addr,
        "resources/read",
        json!({ "uri": "test://static-binary" }),
    );
    assert_result_ok("resources/read static-binary", status, &v);
    assert_eq!(v["result"]["contents"][0]["mimeType"], "image/png");
    let blob = v["result"]["contents"][0]["blob"]
        .as_str()
        .expect("blob base64 présent");
    assert!(!blob.is_empty() && blob.is_ascii());

    // Lecture du gabarit par l'URI concierge (resources-templates-read).
    let (status, v) = rpc(
        addr,
        "resources/read",
        json!({ "uri": "test://template/123/data" }),
    );
    assert_result_ok("resources/read template/123/data", status, &v);
    assert_eq!(v["result"]["contents"][0]["mimeType"], "application/json");
    assert_eq!(
        v["result"]["contents"][0]["text"],
        "{\"id\":\"123\",\"templateTest\":true,\"data\":\"Data for ID: 123\"}"
    );

    // SEP-2164 : URI inconnu → erreur -32602, jamais de `contents` vide.
    // (Le scénario scoré n'engage pas le statut HTTP — seulement le code et
    // l'absence de résultat — donc le test épingle exactement ce contrat.)
    let (status, v) = rpc(
        addr,
        "resources/read",
        json!({ "uri": "test://nonexistent-resource-for-conformance-testing" }),
    );
    assert_eq!(
        v["error"]["code"], -32602,
        "code attendu par le scénario sep-2164 : {v}"
    );
    assert!(
        (200..500).contains(&status),
        "ni succès serveur ni 5xx attendus pour un URI inconnu : {status}"
    );
    assert!(v.get("result").is_none(), "aucun résultat vide : {v}");
}

#[test]
fn resources_read_without_mcp_name_header_rejected() {
    let addr = spawn_fixture();
    // En-tête `Mcp-Name` absent sur resources/read → -32020 + 400 (profil
    // moderne ; c'est l'ancien défaut -32020 sur lecture conforme, figé ici).
    let body = json!({
        "jsonrpc": "2.0",
        "id": next_id(),
        "method": "resources/read",
        "params": {
            "_meta": {
                META_PROTOCOL_VERSION: PROTOCOL_VERSION,
                META_CLIENT_CAPABILITIES: {},
            },
            "uri": "test://static-text",
        },
    })
    .to_string();
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: application/json\r\n\
         MCP-Protocol-Version: {PROTOCOL_VERSION}\r\nMcp-Method: resources/read\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, payload) = post_raw(addr, &request);
    let v: Value = serde_json::from_str(payload.trim()).unwrap();
    assert_eq!(v["error"]["code"], -32020, "Mcp-Name absent → -32020 : {v}");
    assert_eq!(status, 400);
}

#[test]
fn prompts_list_and_get_cover_scored_shapes() {
    let addr = spawn_fixture();

    let (status, v) = rpc(addr, "prompts/list", json!({}));
    assert_result_ok("prompts/list", status, &v);
    let names: Vec<String> = v["result"]["prompts"]
        .as_array()
        .expect("prompts tableau")
        .iter()
        .filter_map(|p| p["name"].as_str().map(str::to_owned))
        .collect();
    for expected in SCORED_PROMPTS {
        assert!(
            names.iter().any(|n| n == expected),
            "le prompt scoré `{expected}` doit être listé : {names:?}"
        );
    }

    // Simple — message utilisateur exact.
    let (status, v) = rpc(addr, "prompts/get", json!({ "name": "test_simple_prompt" }));
    assert_result_ok("prompts/get simple", status, &v);
    assert_eq!(v["result"]["messages"][0]["role"], "user");
    assert_eq!(
        v["result"]["messages"][0]["content"]["text"],
        "This is a simple prompt for testing."
    );

    // Avec arguments — substitution exacte attendue par `prompts-get-with-args`.
    let (status, v) = rpc(
        addr,
        "prompts/get",
        json!({
            "name": "test_prompt_with_arguments",
            "arguments": { "arg1": "testValue1", "arg2": "testValue2" },
        }),
    );
    assert_result_ok("prompts/get with-args", status, &v);
    assert_eq!(
        v["result"]["messages"][0]["content"]["text"],
        "Prompt with arguments: arg1='testValue1', arg2='testValue2'"
    );

    // Ressource embarquée — le `resourceUri` est repris dans le bloc.
    let (status, v) = rpc(
        addr,
        "prompts/get",
        json!({
            "name": "test_prompt_with_embedded_resource",
            "arguments": { "resourceUri": "test://custom-resource" },
        }),
    );
    assert_result_ok("prompts/get embedded", status, &v);
    assert_eq!(
        v["result"]["messages"][0]["content"]["resource"]["uri"],
        "test://custom-resource"
    );

    // Image — bloc image PNG.
    let (status, v) = rpc(
        addr,
        "prompts/get",
        json!({ "name": "test_prompt_with_image" }),
    );
    assert_result_ok("prompts/get image", status, &v);
    assert_eq!(v["result"]["messages"][0]["content"]["type"], "image");
    assert_eq!(
        v["result"]["messages"][0]["content"]["mimeType"],
        "image/png"
    );

    // Cas limite : arguments absents — substitution par chaînes vides,
    // jamais d'erreur interne.
    let (status, v) = rpc(
        addr,
        "prompts/get",
        json!({ "name": "test_prompt_with_arguments" }),
    );
    assert_result_ok("prompts/get sans arguments", status, &v);
    assert_eq!(
        v["result"]["messages"][0]["content"]["text"],
        "Prompt with arguments: arg1='', arg2=''"
    );
}

#[test]
fn completion_serves_scored_values_and_empty_for_unknown_ref() {
    let addr = spawn_fixture();

    // Cas scoré : valeurs exactes attendues par `completion-complete`.
    let (status, v) = rpc(
        addr,
        "completion/complete",
        json!({
            "ref": { "type": "ref/prompt", "name": "test_prompt_with_arguments" },
            "argument": { "name": "arg1", "value": "test" },
        }),
    );
    assert_result_ok("completion/complete prompt", status, &v);
    assert_eq!(
        v["result"]["completion"]["values"],
        json!(["testValue1", "testValue2"]),
        "valeurs scorées attendues : {v}"
    );

    // Cas limite : référence inconnue → complétion vide mais réussie.
    let (status, v) = rpc(
        addr,
        "completion/complete",
        json!({
            "ref": { "type": "ref/prompt", "name": "test_unknown_prompt" },
            "argument": { "name": "arg1", "value": "test" },
        }),
    );
    assert_result_ok("completion/complete inconnu", status, &v);
    assert_eq!(v["result"]["completion"]["values"], json!([]));
}

#[test]
fn modern_rejections_hold_on_the_fixture() {
    let addr = spawn_fixture();

    // `_meta` absent → -32602 + HTTP 400 (scénario server-stateless).
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{}}"#;
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: application/json\r\n\
         MCP-Protocol-Version: {PROTOCOL_VERSION}\r\nMcp-Method: server/discover\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, payload) = post_raw(addr, &request);
    let v: Value = serde_json::from_str(payload.trim()).unwrap();
    assert_eq!(v["error"]["code"], -32602, "_meta absente → -32602 : {v}");
    assert_eq!(status, 400);

    // Méthodes supprimées (initialize, ping) → -32601 + HTTP 404.
    for method in ["initialize", "ping"] {
        let (status, v) = rpc(addr, method, json!({}));
        assert_eq!(
            v["error"]["code"], -32601,
            "{method} supprimé → -32601 : {v}"
        );
        assert_eq!(status, 404, "{method} supprimé → HTTP 404");
    }

    // Méthode inconnue → -32601 + HTTP 404.
    let (status, v) = rpc(addr, "unknown/method", json!({}));
    assert_eq!(v["error"]["code"], -32601);
    assert_eq!(status, 404);

    // Outil inconnu → erreur JSON-RPC (jamais un `result` silencieux).
    let (_, v) = rpc(
        addr,
        "tools/call",
        tools_call("test_unknown_tool", json!({})),
    );
    assert!(
        v.get("error").is_some(),
        "outil inconnu doit produire une erreur : {v}"
    );
}
