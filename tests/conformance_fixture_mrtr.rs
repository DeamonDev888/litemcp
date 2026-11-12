//! Tests de régression de la surface **MRTR** de la fixture de conformance
//! (`examples/conformance_server/fixture.rs`, incluse via `#[path]`).
//!
//! Chaque test rejoue le cycle fil d'un scénario scoré `input-required-result-*`
//! de `requirements/2026-07-28.yaml` : premier tour `input_required`, rejeu
//! sous un **id différent** avec `inputResponses` + écho exact du
//! `requestState`, puis résultat complet. Le noyau MRTR est déjà couvert par
//! `tests/mcp_mrtr_cycle_2026_11_09.rs` ; ici, c'est la **fixture** qui est
//! figée : un outil renommé, une clé d'`inputRequests` changée ou un handler
//! qui ne redemande plus l'entrée manquante casse ces tests avant le run
//! officiel.
//!
//! Les requêtes suivent l'idiome moderne 2026-07-28 (`_meta` complet,
//! en-têtes `MCP-Protocol-Version` / `Mcp-Method` / `Mcp-Name`).

#[path = "../examples/conformance_server/fixture.rs"]
mod fixture;

use litemcp::protocol::{META_CLIENT_CAPABILITIES, META_PROTOCOL_VERSION, PROTOCOL_VERSION};
use serde_json::{Map, Value, json};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicI64, Ordering};

/// Capacités déclarées par le client de conformance (les trois requêtes
/// embarquées de la spec).
fn all_caps() -> Value {
    json!({ "sampling": {}, "elicitation": {}, "roots": { "listChanged": true } })
}

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

/// Rejeu : `inputResponses` (clé → réponse) et l'écho exact du jeton.
struct Round<'a> {
    responses: Value,
    state: Option<&'a str>,
}

fn no_round() -> Round<'static> {
    Round {
        responses: json!({}),
        state: None,
    }
}

fn rpc(
    addr: SocketAddr,
    method: &str,
    params: Value,
    caps: Value,
    round: Round<'_>,
) -> (u16, Value) {
    let mut params = params;
    let obj = params.as_object_mut().expect("params objet");
    obj.insert(
        "_meta".into(),
        json!({
            META_PROTOCOL_VERSION: PROTOCOL_VERSION,
            META_CLIENT_CAPABILITIES: caps,
        }),
    );
    // Comme la suite officielle : `inputResponses` et `requestState` sont au
    // niveau des `params` de la requête rejouée.
    if let Some(responses) = round.responses.as_object() {
        if !responses.is_empty() {
            obj.insert("inputResponses".into(), round.responses.clone());
        }
    }
    if let Some(state) = round.state {
        obj.insert("requestState".into(), json!(state));
    }
    rpc_raw(addr, method, params)
}

/// Requête POST moderne valide, sans ajout de `_meta` (pour les cas négatifs).
fn rpc_raw(addr: SocketAddr, method: &str, params: Value) -> (u16, Value) {
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
    if let Some(name) = name {
        headers.push_str(&format!("Mcp-Name: {name}\r\n"));
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

/// Appelle un outil avec les arguments donnés.
fn tool(name: &str, arguments: Value) -> Value {
    json!({ "name": name, "arguments": arguments })
}

/// Résultat `input_required` attendu : `resultType`, `requestState` non vide
/// et l'objet `inputRequests` rendu pour examen.
fn expect_input_required<'a>(v: &'a Value, context: &str) -> (&'a Map<String, Value>, &'a str) {
    assert_eq!(
        v["result"]["resultType"], "input_required",
        "{context} : résultat `input_required` attendu : {v}"
    );
    let requests = v["result"]["inputRequests"]
        .as_object()
        .unwrap_or_else(|| panic!("{context} : `inputRequests` objet attendu : {v}"));
    let state = v["result"]["requestState"]
        .as_str()
        .unwrap_or_else(|| panic!("{context} : `requestState` chaîne attendue : {v}"));
    assert!(!state.is_empty(), "{context} : jeton non vide");
    (requests, state)
}

fn method_of(request: &Value) -> &str {
    request["method"].as_str().expect("méthode embarquée")
}

/// Texte du premier bloc de contenu d'un résultat complet.
fn first_text(v: &Value) -> &str {
    v["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("bloc texte attendu : {v}"))
}

/// Map `clé → réponse` d'`inputResponses` (clés dynamiques, `json!` ne les
/// accepte qu'en littéral).
fn responses(entries: impl IntoIterator<Item = (String, Value)>) -> Value {
    let mut map = Map::new();
    for (key, value) in entries {
        map.insert(key, value);
    }
    Value::Object(map)
}

// ---------------------------------------------------------------------------
// Outils
// ---------------------------------------------------------------------------

#[test]
fn elicitation_round_trip_completes_with_the_accepted_value() {
    let addr = spawn_fixture();
    let (status, first) = rpc(
        addr,
        "tools/call",
        tool("test_input_required_result_elicitation", json!({})),
        all_caps(),
        no_round(),
    );
    assert_eq!(status, 200);
    let (requests, state) = expect_input_required(&first, "tour 1");
    assert_eq!(method_of(&requests["user_name"]), "elicitation/create");
    assert_eq!(
        requests["user_name"]["params"]["requestedSchema"]["required"],
        json!(["name"])
    );

    let (status, second) = rpc(
        addr,
        "tools/call",
        tool("test_input_required_result_elicitation", json!({})),
        all_caps(),
        Round {
            responses: json!({ "user_name": { "action": "accept", "content": { "name": "Alice" } } }),
            state: Some(state),
        },
    );
    assert_eq!(status, 200, "{second}");
    assert_eq!(second["result"]["resultType"], "complete", "{second}");
    assert_eq!(first_text(&second), "Hello, Alice!");
}

#[test]
fn missing_input_response_re_requests_instead_of_erroring() {
    let addr = spawn_fixture();
    // Scénario `input-required-result-missing-input-response` : une clé
    // inconnue ne doit pas produire d'erreur JSON-RPC — le serveur redemande.
    //
    // La suite envoie ce rejeu SANS `requestState` : le noyau le refuse alors
    // en `-32602` (« rien n'est vérifiable »), ce que le harness enregistre
    // en WARNING (jamais FAILURE) ; le rejeu conforme ci-dessous porte le
    // jeton et atteint bien le handler, qui redemande l'entrée.
    let (_, first) = rpc(
        addr,
        "tools/call",
        tool("test_input_required_result_elicitation", json!({})),
        all_caps(),
        no_round(),
    );
    let (_, state) = expect_input_required(&first, "tour 1");

    let (status, v) = rpc(
        addr,
        "tools/call",
        tool("test_input_required_result_elicitation", json!({})),
        all_caps(),
        Round {
            responses: json!({ "wrong_key": { "action": "accept", "content": { "data": "wrong" } } }),
            state: Some(state),
        },
    );
    assert_eq!(status, 200, "{v}");
    assert!(v.get("error").is_none(), "aucune erreur JSON-RPC : {v}");
    let (requests, _) = expect_input_required(&v, "re-demande");
    assert!(
        requests.contains_key("user_name"),
        "la clé manquante est redemandée : {v}"
    );
}

#[test]
fn input_responses_without_request_state_are_refused() {
    let addr = spawn_fixture();
    // Contrat du noyau (documenté sur `verify_mrtr`) : des réponses sans le
    // jeton signé ne sont pas vérifiables → `-32602`, le handler n'est pas
    // atteint. C'est la variante exacte de `missing-input-response` et
    // d'`ignore-extra-params` (WARNING côté harness, jamais FAILURE).
    let (status, v) = rpc(
        addr,
        "tools/call",
        tool("test_input_required_result_elicitation", json!({})),
        all_caps(),
        Round {
            responses: json!({ "user_name": { "action": "accept", "content": { "name": "Alice" } } }),
            state: None,
        },
    );
    assert_eq!(v["error"]["code"], -32602, "{v}");
    assert_eq!(status, 400, "{v}");
}

#[test]
fn extra_input_responses_are_ignored() {
    let addr = spawn_fixture();
    // Scénario `input-required-result-ignore-extra-params` : les clés
    // inconnues sont ignorées dès que la clé attendue est acceptée.
    let (_, first) = rpc(
        addr,
        "tools/call",
        tool("test_input_required_result_elicitation", json!({})),
        all_caps(),
        no_round(),
    );
    let (_, state) = expect_input_required(&first, "tour 1");
    let (status, second) = rpc(
        addr,
        "tools/call",
        tool("test_input_required_result_elicitation", json!({})),
        all_caps(),
        Round {
            responses: json!({
                "user_name": { "action": "accept", "content": { "name": "Alice" } },
                "unknown_extra_key": { "action": "accept", "content": { "foo": "bar" } },
                "another_unexpected": { "action": "accept", "content": { "baz": 123 } },
            }),
            state: Some(state),
        },
    );
    assert_eq!(status, 200, "{second}");
    assert_eq!(second["result"]["resultType"], "complete", "{second}");
    assert_eq!(first_text(&second), "Hello, Alice!");
}

#[test]
fn sampling_round_trip_returns_the_sampling_text() {
    let addr = spawn_fixture();
    let (_, first) = rpc(
        addr,
        "tools/call",
        tool("test_input_required_result_sampling", json!({})),
        all_caps(),
        no_round(),
    );
    let (requests, state) = expect_input_required(&first, "tour 1");
    let key = requests
        .keys()
        .next()
        .expect("une requête embarquée")
        .clone();
    assert_eq!(method_of(&requests[&key]), "sampling/createMessage");

    let (status, second) = rpc(
        addr,
        "tools/call",
        tool("test_input_required_result_sampling", json!({})),
        all_caps(),
        Round {
            responses: responses([(
                key,
                json!({
                    "role": "assistant",
                    "content": { "type": "text", "text": "The capital of France is Paris." },
                    "model": "test-model",
                    "stopReason": "endTurn",
                }),
            )]),
            state: Some(state),
        },
    );
    assert_eq!(status, 200, "{second}");
    assert_eq!(second["result"]["resultType"], "complete", "{second}");
    assert!(
        first_text(&second).contains("The capital of France is Paris."),
        "le texte d'échantillonnage est restitué : {second}"
    );
}

#[test]
fn list_roots_round_trip_cites_the_client_roots() {
    let addr = spawn_fixture();
    let (_, first) = rpc(
        addr,
        "tools/call",
        tool("test_input_required_result_list_roots", json!({})),
        all_caps(),
        no_round(),
    );
    let (requests, state) = expect_input_required(&first, "tour 1");
    let key = requests
        .keys()
        .next()
        .expect("une requête embarquée")
        .clone();
    assert_eq!(method_of(&requests[&key]), "roots/list");

    let (status, second) = rpc(
        addr,
        "tools/call",
        tool("test_input_required_result_list_roots", json!({})),
        all_caps(),
        Round {
            responses: responses([(
                key,
                json!({ "roots": [{ "uri": "file:///test/root", "name": "Test Root" }] }),
            )]),
            state: Some(state),
        },
    );
    assert_eq!(status, 200, "{second}");
    assert_eq!(second["result"]["resultType"], "complete", "{second}");
    assert!(
        first_text(&second).contains("file:///test/root"),
        "les racines reçues sont citées : {second}"
    );
}

#[test]
fn request_state_round_trip_reports_state_ok() {
    let addr = spawn_fixture();
    let (_, first) = rpc(
        addr,
        "tools/call",
        tool("test_input_required_result_request_state", json!({})),
        all_caps(),
        no_round(),
    );
    let (requests, state) = expect_input_required(&first, "tour 1");
    let key = requests
        .keys()
        .next()
        .expect("une requête embarquée")
        .clone();

    let (status, second) = rpc(
        addr,
        "tools/call",
        tool("test_input_required_result_request_state", json!({})),
        all_caps(),
        Round {
            responses: responses([(
                key,
                json!({ "action": "accept", "content": { "ok": true } }),
            )]),
            state: Some(state),
        },
    );
    assert_eq!(status, 200, "{second}");
    assert_eq!(second["result"]["resultType"], "complete", "{second}");
    assert!(
        first_text(&second).contains("state-ok"),
        "le scénario exige le mot `state-ok` : {second}"
    );
}

#[test]
fn multiple_input_requests_round_trip_carries_three_methods() {
    let addr = spawn_fixture();
    let (_, first) = rpc(
        addr,
        "tools/call",
        tool("test_input_required_result_multiple_inputs", json!({})),
        all_caps(),
        no_round(),
    );
    let (requests, state) = expect_input_required(&first, "tour 1");
    assert!(requests.len() >= 3, "au moins trois requêtes : {first}");
    let methods: Vec<&str> = requests.values().map(method_of).collect();
    for expected in ["elicitation/create", "sampling/createMessage", "roots/list"] {
        assert!(
            methods.contains(&expected),
            "méthode `{expected}` absente : {methods:?}"
        );
    }

    let mut responses = Map::new();
    for (key, request) in requests {
        responses.insert(
            key.clone(),
            match method_of(request) {
                "elicitation/create" => {
                    json!({ "action": "accept", "content": { "name": "Alice" } })
                }
                "sampling/createMessage" => json!({
                    "role": "assistant",
                    "content": { "type": "text", "text": "Hello there!" },
                    "model": "test-model",
                    "stopReason": "endTurn",
                }),
                _ => json!({ "roots": [{ "uri": "file:///test/root", "name": "Test Root" }] }),
            },
        );
    }

    let (status, second) = rpc(
        addr,
        "tools/call",
        tool("test_input_required_result_multiple_inputs", json!({})),
        all_caps(),
        Round {
            responses: Value::Object(responses),
            state: Some(state),
        },
    );
    assert_eq!(status, 200, "{second}");
    assert_eq!(second["result"]["resultType"], "complete", "{second}");
    assert!(first_text(&second).contains("Alice"), "{second}");
}

#[test]
fn multi_round_emits_a_new_state_then_completes() {
    let addr = spawn_fixture();
    let (_, first) = rpc(
        addr,
        "tools/call",
        tool("test_input_required_result_multi_round", json!({})),
        all_caps(),
        no_round(),
    );
    let (requests, state1) = expect_input_required(&first, "tour 1");
    let key1 = requests.keys().next().expect("step1").clone();

    let (_, second) = rpc(
        addr,
        "tools/call",
        tool("test_input_required_result_multi_round", json!({})),
        all_caps(),
        Round {
            responses: responses([(
                key1,
                json!({ "action": "accept", "content": { "name": "Alice" } }),
            )]),
            state: Some(state1),
        },
    );
    let (requests2, state2) = expect_input_required(&second, "tour 2");
    let key2 = requests2.keys().next().expect("step2").clone();
    assert_ne!(state1, state2, "le requestState évolue entre les tours");

    let (status, third) = rpc(
        addr,
        "tools/call",
        tool("test_input_required_result_multi_round", json!({})),
        all_caps(),
        Round {
            responses: responses([(
                key2,
                json!({ "action": "accept", "content": { "color": "blue" } }),
            )]),
            state: Some(state2),
        },
    );
    assert_eq!(status, 200, "{third}");
    assert_eq!(third["result"]["resultType"], "complete", "{third}");
    let text = first_text(&third);
    assert!(
        text.contains("Alice") && text.contains("blue"),
        "nom du tour 1 et couleur du tour 2 restitués : {third}"
    );
}

#[test]
fn tampered_state_is_rejected_with_invalid_params() {
    let addr = spawn_fixture();
    let (_, first) = rpc(
        addr,
        "tools/call",
        tool("test_input_required_result_tampered_state", json!({})),
        all_caps(),
        no_round(),
    );
    let (requests, state) = expect_input_required(&first, "tour 1");
    let key = requests
        .keys()
        .next()
        .expect("une requête embarquée")
        .clone();
    let tampered = format!("{state}-TAMPERED");

    let (status, second) = rpc(
        addr,
        "tools/call",
        tool("test_input_required_result_tampered_state", json!({})),
        all_caps(),
        Round {
            responses: responses([(
                key,
                json!({ "action": "accept", "content": { "ok": true } }),
            )]),
            state: Some(&tampered),
        },
    );
    assert_eq!(
        second["error"]["code"], -32602,
        "jeton altéré → -32602 : {second}"
    );
    assert_eq!(status, 400, "HTTP 400 pour une erreur de params : {second}");
}

#[test]
fn capability_check_only_requests_declared_capabilities() {
    let addr = spawn_fixture();
    // Le scénario ne déclare que `sampling` : la fixture ne doit émettre
    // aucun `elicitation/create` (le noyau refuserait en -32021).
    let (status, v) = rpc(
        addr,
        "tools/call",
        tool("test_input_required_result_capabilities", json!({})),
        json!({ "sampling": {} }),
        no_round(),
    );
    assert_eq!(status, 200, "{v}");
    let (requests, _) = expect_input_required(&v, "capability-check");
    assert!(
        requests
            .values()
            .all(|r| method_of(r) != "elicitation/create"),
        "aucune élicitation non déclarée : {v}"
    );
    assert!(
        requests
            .values()
            .any(|r| method_of(r) == "sampling/createMessage"),
        "la capacité déclarée est bien utilisée : {v}"
    );

    // Contrôle négatif : sans aucune capacité, le noyau refuse l'émission
    // plutôt que d'envoyer une requête que le client ne sait pas servir.
    let (status, v) = rpc(
        addr,
        "tools/call",
        tool("test_input_required_result_capabilities", json!({})),
        json!({}),
        no_round(),
    );
    assert_eq!(v["error"]["code"], -32021, "{v}");
    assert_eq!(status, 400, "{v}");
}

// ---------------------------------------------------------------------------
// Prompt et ressource (les deux autres méthodes MRTR de la spec)
// ---------------------------------------------------------------------------

#[test]
fn prompt_round_trip_returns_get_prompt_result() {
    let addr = spawn_fixture();
    let (status, first) = rpc(
        addr,
        "prompts/get",
        json!({ "name": "test_input_required_result_prompt" }),
        all_caps(),
        no_round(),
    );
    assert_eq!(status, 200, "{first}");
    let (requests, state) = expect_input_required(&first, "tour 1");
    let key = requests
        .keys()
        .next()
        .expect("une requête embarquée")
        .clone();
    assert_eq!(method_of(&requests[&key]), "elicitation/create");

    let (status, second) = rpc(
        addr,
        "prompts/get",
        json!({ "name": "test_input_required_result_prompt" }),
        all_caps(),
        Round {
            responses: responses([(
                key,
                json!({ "action": "accept", "content": { "context": "test context" } }),
            )]),
            state: Some(state),
        },
    );
    assert_eq!(status, 200, "{second}");
    assert_eq!(second["result"]["resultType"], "complete", "{second}");
    assert_eq!(
        second["result"]["messages"][0]["content"]["text"],
        "Prompt context: test context"
    );
}

#[test]
fn resource_round_trip_reads_after_input() {
    let addr = spawn_fixture();
    let uri = "test://template/zz-mrtr/data";
    let (status, first) = rpc(
        addr,
        "resources/read",
        json!({ "uri": uri }),
        all_caps(),
        no_round(),
    );
    assert_eq!(status, 200, "{first}");
    let (requests, state) = expect_input_required(&first, "tour 1");
    let key = requests
        .keys()
        .next()
        .expect("une requête embarquée")
        .clone();

    let (status, second) = rpc(
        addr,
        "resources/read",
        json!({ "uri": uri }),
        all_caps(),
        Round {
            responses: responses([(
                key,
                json!({ "action": "accept", "content": { "scope": "wide" } }),
            )]),
            state: Some(state),
        },
    );
    assert_eq!(status, 200, "{second}");
    assert_eq!(second["result"]["resultType"], "complete", "{second}");
    assert_eq!(second["result"]["contents"][0]["uri"], uri);
    assert_eq!(
        second["result"]["contents"][0]["text"],
        "MRTR resource resolved for scope 'wide'."
    );
}
