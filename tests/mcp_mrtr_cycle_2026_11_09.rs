//! Tests de régression MRTR — cycle complet multi round-trip (spec MCP 2026-07-28).
//!
//! Couvre : émission d'un résultat `input_required` par un handler (outil,
//! prompt, ressource), rejeu avec `inputResponses` + écho exact du
//! `requestState` sous un **id JSON-RPC différent**, restauration de l'état
//! opaque signé (HMAC-SHA256, TTL, liaison méthode/paramètres), refus des
//! états altérés, expirés, mal formés ou liés à une autre requête, exigence
//! des capacités client déclarées (`-32021`), bogues de handler (`-32603`),
//! champs superflus ignorés, cycle multi-tours, méthodes non concernées et
//! absence de capacité serveur supplémentaire annoncée.

use litemcp::protocol::{
    InputRequiredSpec, META_CLIENT_CAPABILITIES, META_PROTOCOL_VERSION, PROTOCOL_VERSION,
    PromptMessage, ResourceData,
};
use litemcp::server::{InputRound, PromptOutcome, ResourceOutcome, ToolOutcome};
use litemcp::{McpHeaders, ServerBuilder, ServerCore, ToolError};
use serde_json::{Value, json};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Handlers de test
// ---------------------------------------------------------------------------

/// Outil à deux tours : élicite une confirmation au tour 1, termine au
/// tour 2 en restituant l'état vérifié.
fn booking_handler(args: &Value, round: &InputRound) -> Result<ToolOutcome, ToolError> {
    match round.turn {
        1 => {
            let seat = args.get("seat").cloned().unwrap_or(Value::Null);
            Ok(ToolOutcome::InputRequired(
                InputRequiredSpec::new()
                    .elicit(
                        "confirm",
                        "Confirm the booking?",
                        json!({
                            "type": "object",
                            "properties": { "confirm": { "type": "boolean" } },
                            "required": ["confirm"]
                        }),
                    )
                    .with_state(json!({ "step": 1, "seat": seat })),
            ))
        }
        _ => {
            let state = round.state.clone().unwrap_or(Value::Null);
            let seat = state.get("seat").cloned().unwrap_or(Value::Null);
            match round.accepted("confirm") {
                Some(content) => Ok(ToolOutcome::Value(json!({
                    "booked": true,
                    "seat": seat,
                    "confirmed": content.get("confirm").cloned().unwrap_or(Value::Null),
                }))),
                None => Ok(ToolOutcome::Value(json!({ "booked": false, "seat": seat }))),
            }
        }
    }
}

/// Outil à un seul tour : émet un état seul, sans `inputRequests`
/// (l'optionnalité de `inputRequests` — reprise pure).
fn checkpoint_handler(_args: &Value, round: &InputRound) -> Result<ToolOutcome, ToolError> {
    match round.turn {
        1 => Ok(ToolOutcome::InputRequired(
            InputRequiredSpec::new().with_state(json!({ "checkpoint": true })),
        )),
        _ => Ok(ToolOutcome::Value(json!({ "resumed": round.state }))),
    }
}

/// Outil à trois tours : le compteur vit dans l'état signé.
fn walker_handler(_args: &Value, round: &InputRound) -> Result<ToolOutcome, ToolError> {
    let step = round
        .state
        .as_ref()
        .and_then(|s| s.get("step"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
        + 1;
    if step < 3 {
        Ok(ToolOutcome::InputRequired(
            InputRequiredSpec::new()
                .elicit("go", format!("Step {step}?"), json!({ "type": "object" }))
                .with_state(json!({ "step": step })),
        ))
    } else {
        Ok(ToolOutcome::Value(json!({ "done": true, "step": step })))
    }
}

fn sampler_handler(_args: &Value, _round: &InputRound) -> Result<ToolOutcome, ToolError> {
    Ok(ToolOutcome::InputRequired(
        InputRequiredSpec::new().create_message(
            "gen",
            json!({
                "messages": [{ "role": "user", "content": { "type": "text", "text": "hi" } }],
                "maxTokens": 100
            }),
        ),
    ))
}

fn roots_handler(_args: &Value, _round: &InputRound) -> Result<ToolOutcome, ToolError> {
    Ok(ToolOutcome::InputRequired(
        InputRequiredSpec::new().list_roots("roots"),
    ))
}

fn url_elicit_handler(_args: &Value, _round: &InputRound) -> Result<ToolOutcome, ToolError> {
    Ok(ToolOutcome::InputRequired(
        InputRequiredSpec::new().elicit_url(
            "approve",
            "Open the approval page",
            "https://example.test/approve",
        ),
    ))
}

fn bogus_handler(_args: &Value, _round: &InputRound) -> Result<ToolOutcome, ToolError> {
    // `tools/list` n'est pas une méthode embarquable — bogue de handler.
    Ok(ToolOutcome::InputRequired(
        InputRequiredSpec::new().request("x", "tools/list", json!({})),
    ))
}

fn formless_handler(_args: &Value, _round: &InputRound) -> Result<ToolOutcome, ToolError> {
    // Élicitation `form` sans `requestedSchema` — bogue de handler.
    Ok(ToolOutcome::InputRequired(
        InputRequiredSpec::new().request(
            "c",
            "elicitation/create",
            json!({ "mode": "form", "message": "m" }),
        ),
    ))
}

fn urlless_handler(_args: &Value, _round: &InputRound) -> Result<ToolOutcome, ToolError> {
    // Élicitation `url` sans `url` — bogue de handler.
    Ok(ToolOutcome::InputRequired(
        InputRequiredSpec::new().request(
            "c",
            "elicitation/create",
            json!({ "mode": "url", "message": "m" }),
        ),
    ))
}

fn big_state_handler(_args: &Value, _round: &InputRound) -> Result<ToolOutcome, ToolError> {
    // 9 Kio sérialisés > borne de 8 Kio — bogue de handler.
    Ok(ToolOutcome::InputRequired(
        InputRequiredSpec::new().with_state(Value::String("x".repeat(9000))),
    ))
}

fn empty_spec_handler(_args: &Value, _round: &InputRound) -> Result<ToolOutcome, ToolError> {
    // Ni `inputRequests` ni état — bogue de handler.
    Ok(ToolOutcome::InputRequired(InputRequiredSpec::new()))
}

fn wizard_handler(_args: &Value, round: &InputRound) -> Result<PromptOutcome, String> {
    match round.turn {
        1 => Ok(PromptOutcome::InputRequired(
            InputRequiredSpec::new()
                .list_roots("roots")
                .with_state(json!({ "wizard": 1 })),
        )),
        _ => Ok(PromptOutcome::Messages(vec![PromptMessage::user(format!(
            "wizard resumed, responses present: {}",
            round.input_responses.is_some()
        ))])),
    }
}

fn vault_handler(round: &InputRound) -> Result<ResourceOutcome, String> {
    match round.turn {
        1 => Ok(ResourceOutcome::InputRequired(
            InputRequiredSpec::new()
                .elicit("key", "Passphrase?", json!({ "type": "object" }))
                .with_state(json!({ "vault": 1 })),
        )),
        _ => Ok(ResourceOutcome::Data(ResourceData::text(
            "mem://vault",
            None,
            "unlocked",
        ))),
    }
}

// ---------------------------------------------------------------------------
// Harnais
// ---------------------------------------------------------------------------

fn client_caps() -> Value {
    json!({ "elicitation": {}, "sampling": {}, "roots": {} })
}

fn mrtr_core() -> ServerCore {
    ServerBuilder::new("mrtr", "1.0.0")
        .tool_mrtr("booking", "Réservation multi-tour", booking_handler)
        .tool_mrtr("checkpoint", "Point de reprise", checkpoint_handler)
        .tool_mrtr("walker", "Trois tours", walker_handler)
        .tool_mrtr("sampler", "Échantillonnage", sampler_handler)
        .tool_mrtr("roots_ask", "Racines", roots_handler)
        .tool_mrtr("url_elicit", "Élicitation URL", url_elicit_handler)
        .tool_mrtr("bogus_request", "Non embarquable", bogus_handler)
        .tool_mrtr("formless_elicit", "Form sans schéma", formless_handler)
        .tool_mrtr("urlless_elicit", "Url sans URL", urlless_handler)
        .tool_mrtr("big_state", "État surdimensionné", big_state_handler)
        .tool_mrtr("empty_spec", "Spécification vide", empty_spec_handler)
        .prompt_mrtr("wizard", "Assistant multi-tour", wizard_handler)
        .resource_mrtr("mem://vault", "Vault", "Coffre multi-tour", vault_handler)
        .core()
}

fn call(core: &ServerCore, body: Value) -> Value {
    let reply = core
        .dispatch(body.to_string().as_bytes())
        .expect("une réponse est attendue pour une requête");
    serde_json::from_slice(&reply).unwrap()
}

/// Params `tools/call` avec capacités client déclarées (permutation nominale).
fn tools_params(name: &str, arguments: Value, caps: Value) -> Value {
    json!({
        "name": name,
        "arguments": arguments,
        "_meta": { (META_CLIENT_CAPABILITIES): caps },
    })
}

/// Tour 1 nominal sur `tools/call` — renvoie la réponse (doit être
/// `input_required`).
fn round_one(core: &ServerCore, name: &str, id: i64) -> Value {
    let r = call(
        core,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": tools_params(name, json!({ "seat": "12A" }), client_caps()),
        }),
    );
    assert_eq!(
        r["result"]["resultType"], "input_required",
        "tour 1 attendu : {r}"
    );
    r
}

fn error_code(v: &Value) -> i64 {
    v["error"]["code"].as_i64().unwrap_or_else(|| {
        panic!("erreur JSON-RPC attendue : {v}");
    })
}

fn flip_first(s: &str) -> String {
    let mut chars: Vec<char> = s.chars().collect();
    chars[0] = if chars[0] == 'A' { 'B' } else { 'A' };
    chars.into_iter().collect()
}

/// Dispatch strict (profil par défaut) avec en-têtes complets — renvoie
/// (statut HTTP, corps JSON).
fn strict_call(core: &ServerCore, id: i64, method: &str, params: Value) -> (u16, Value) {
    let name = params
        .get("name")
        .or_else(|| params.get("uri"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let headers = McpHeaders {
        protocol_version: Some(PROTOCOL_VERSION.to_string()),
        method: Some(method.to_string()),
        name: Some(name),
        params: Vec::new(),
    };
    let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
    let response = core
        .dispatch_request(body.to_string().as_bytes(), Some(&headers))
        .expect("une réponse est attendue pour une requête");
    let parsed: Value = serde_json::from_slice(&response.body).unwrap();
    (response.status.code(), parsed)
}

fn strict_params(name: &str, arguments: Value, caps: Value) -> Value {
    json!({
        "name": name,
        "arguments": arguments,
        "_meta": {
            (META_PROTOCOL_VERSION): PROTOCOL_VERSION,
            (META_CLIENT_CAPABILITIES): caps,
        },
    })
}

// ---------------------------------------------------------------------------
// Cycle nominal
// ---------------------------------------------------------------------------

#[test]
fn full_round_trip_restores_the_verified_state() {
    let core = mrtr_core();
    let r1 = round_one(&core, "booking", 1);

    // Forme fil du résultat `input_required` (spec : resultType,
    // inputRequests, requestState) + identification serveur (`_meta`).
    let token = r1["result"]["requestState"]
        .as_str()
        .expect("jeton attendu");
    assert!(!token.is_empty() && token.contains('.'));
    let confirm = &r1["result"]["inputRequests"]["confirm"];
    assert_eq!(confirm["method"], "elicitation/create");
    assert_eq!(confirm["params"]["mode"], "form");
    assert_eq!(confirm["params"]["requestedSchema"]["type"], "object");
    assert_eq!(
        r1["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "mrtr"
    );

    // Rejeu : **id différent**, réponses du client, écho exact du jeton.
    let r2 = call(
        &core,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "booking",
                "arguments": { "seat": "12A" },
                "_meta": { (META_CLIENT_CAPABILITIES): client_caps() },
                "inputResponses": {
                    "confirm": { "action": "accept", "content": { "confirm": true } }
                },
                "requestState": token,
            },
        }),
    );
    assert_eq!(r2["id"], 2, "le rejeu répond sous le nouvel id");
    assert_eq!(r2["result"]["resultType"], "complete", "{r2}");
    assert_eq!(r2["result"]["structuredContent"]["booked"], true);
    assert_eq!(r2["result"]["structuredContent"]["seat"], "12A");
    assert_eq!(r2["result"]["structuredContent"]["confirmed"], true);
}

#[test]
fn replay_without_input_responses_reaches_the_handler() {
    // Un rejeu avec le jeton seul (sans `inputResponses`) est admis : le
    // noyau ne dépend d'aucun état implicite — le handler décide.
    let core = mrtr_core();
    let r1 = round_one(&core, "booking", 1);
    let token = r1["result"]["requestState"].as_str().unwrap();

    let r2 = call(
        &core,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "booking",
                "arguments": { "seat": "12A" },
                "_meta": { (META_CLIENT_CAPABILITIES): client_caps() },
                "requestState": token,
            },
        }),
    );
    assert_eq!(r2["result"]["resultType"], "complete", "{r2}");
    assert_eq!(r2["result"]["structuredContent"]["booked"], false);
}

#[test]
fn state_only_result_omits_input_requests() {
    // Un état seul (sans requête embarquée) est un résultat `input_required`
    // valide — la clé `inputRequests` est omise du fil.
    let core = mrtr_core();
    let r1 = call(
        &core,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": tools_params("checkpoint", json!({}), client_caps()),
        }),
    );
    assert_eq!(r1["result"]["resultType"], "input_required", "{r1}");
    assert!(r1["result"]["inputRequests"].is_null(), "{r1}");
    let token = r1["result"]["requestState"].as_str().unwrap();

    let r2 = call(
        &core,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "checkpoint",
                "arguments": {},
                "_meta": { (META_CLIENT_CAPABILITIES): client_caps() },
                "requestState": token,
            },
        }),
    );
    assert_eq!(
        r2["result"]["structuredContent"]["resumed"]["checkpoint"],
        true
    );
}

// ---------------------------------------------------------------------------
// Rejet des rejeux invalides
// ---------------------------------------------------------------------------

#[test]
fn input_responses_without_request_state_is_refused() {
    let core = mrtr_core();
    let r = call(
        &core,
        json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "tools/call",
            "params": {
                "name": "booking",
                "arguments": { "seat": "12A" },
                "_meta": { (META_CLIENT_CAPABILITIES): client_caps() },
                "inputResponses": { "confirm": { "action": "accept" } },
            },
        }),
    );
    assert_eq!(error_code(&r), -32602, "{r}");
}

#[test]
fn malformed_or_oversized_request_state_is_refused() {
    let core = mrtr_core();
    for token in [
        "garbage",
        "AAAA.BBBB",
        ".abc",
        "abc.def.ghi",
        "a".repeat(20_000).as_str(),
    ] {
        let r = call(
            &core,
            json!({
                "jsonrpc": "2.0",
                "id": 9,
                "method": "tools/call",
                "params": {
                    "name": "booking",
                    "arguments": { "seat": "12A" },
                    "_meta": { (META_CLIENT_CAPABILITIES): client_caps() },
                    "requestState": token,
                },
            }),
        );
        assert_eq!(error_code(&r), -32602, "jeton {token:?} : {r}");
    }
    // Jeton non-string.
    let r = call(
        &core,
        json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "tools/call",
            "params": {
                "name": "booking",
                "arguments": { "seat": "12A" },
                "_meta": { (META_CLIENT_CAPABILITIES): client_caps() },
                "requestState": 42,
            },
        }),
    );
    assert_eq!(error_code(&r), -32602, "{r}");
}

#[test]
fn tampered_state_is_refused() {
    let core = mrtr_core();
    let r1 = round_one(&core, "booking", 1);
    let token = r1["result"]["requestState"].as_str().unwrap();
    let (payload, signature) = token.split_once('.').unwrap();

    // 1. Un octet du payload modifié → signature invalide (le premier
    //    caractère porte les bits de poids fort du premier octet : le
    //    remplacement change forcément les octets décodés).
    let tampered_payload = format!("{}.{}", flip_first(payload), signature);
    let r = call(
        &core,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "booking",
                "arguments": { "seat": "12A" },
                "_meta": { (META_CLIENT_CAPABILITIES): client_caps() },
                "inputResponses": { "confirm": { "action": "accept" } },
                "requestState": tampered_payload,
            },
        }),
    );
    assert_eq!(error_code(&r), -32602, "{r}");

    // 2. Un octet de la signature modifié → signature invalide.
    let tampered_signature = format!("{}.{}", payload, flip_first(signature));
    let r = call(
        &core,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "booking",
                "arguments": { "seat": "12A" },
                "_meta": { (META_CLIENT_CAPABILITIES): client_caps() },
                "inputResponses": { "confirm": { "action": "accept" } },
                "requestState": tampered_signature,
            },
        }),
    );
    assert_eq!(error_code(&r), -32602, "{r}");

    // 3. Jeton émis par une instance avec une autre clé (redémarrage sans
    //    clé partagée) → signature invalide.
    let other = ServerBuilder::new("mrtr", "1.0.0")
        .request_state_key(b"another-key-totally-different-32b")
        .tool_mrtr("booking", "Réservation multi-tour", booking_handler)
        .core();
    let r = call(
        &other,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "booking",
                "arguments": { "seat": "12A" },
                "_meta": { (META_CLIENT_CAPABILITIES): client_caps() },
                "inputResponses": { "confirm": { "action": "accept" } },
                "requestState": token,
            },
        }),
    );
    assert_eq!(error_code(&r), -32602, "{r}");
}

#[test]
fn expired_state_is_refused() {
    let core = ServerBuilder::new("mrtr", "1.0.0")
        .request_state_ttl(Duration::ZERO)
        .tool_mrtr("booking", "Réservation multi-tour", booking_handler)
        .core();
    let r1 = round_one(&core, "booking", 1);
    let token = r1["result"]["requestState"].as_str().unwrap();
    let r = call(
        &core,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "booking",
                "arguments": { "seat": "12A" },
                "_meta": { (META_CLIENT_CAPABILITIES): client_caps() },
                "inputResponses": { "confirm": { "action": "accept" } },
                "requestState": token,
            },
        }),
    );
    assert_eq!(error_code(&r), -32602, "{r}");
    assert!(
        r["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("expired")),
        "message d'expiration attendu : {r}"
    );
}

#[test]
fn state_is_bound_to_the_original_method_and_request() {
    let core = mrtr_core();
    let r1 = round_one(&core, "booking", 1);
    let token = r1["result"]["requestState"].as_str().unwrap();
    let responses = json!({ "confirm": { "action": "accept" } });

    let attempt = |id: i64, method: &str, params: Value| {
        let mut obj = params.as_object().unwrap().clone();
        obj.insert("inputResponses".into(), responses.clone());
        obj.insert("requestState".into(), Value::String(token.to_string()));
        call(
            &core,
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": obj }),
        )
    };

    // Méthode différente (jeton lié à `tools/call`).
    let r = attempt(
        2,
        "prompts/get",
        json!({
            "name": "wizard",
            "arguments": {},
            "_meta": { (META_CLIENT_CAPABILITIES): client_caps() },
        }),
    );
    assert_eq!(error_code(&r), -32602, "méthode différente : {r}");

    // Outil différent.
    let r = attempt(
        3,
        "tools/call",
        json!({
            "name": "walker",
            "arguments": { "seat": "12A" },
            "_meta": { (META_CLIENT_CAPABILITIES): client_caps() },
        }),
    );
    assert_eq!(error_code(&r), -32602, "outil différent : {r}");

    // Arguments différents.
    let r = attempt(
        4,
        "tools/call",
        json!({
            "name": "booking",
            "arguments": { "seat": "12B" },
            "_meta": { (META_CLIENT_CAPABILITIES): client_caps() },
        }),
    );
    assert_eq!(error_code(&r), -32602, "arguments différents : {r}");

    // Arguments absents au rejeu (émission : `{"seat":"12A"}`).
    let r = attempt(
        5,
        "tools/call",
        json!({
            "name": "booking",
            "_meta": { (META_CLIENT_CAPABILITIES): client_caps() },
        }),
    );
    assert_eq!(error_code(&r), -32602, "arguments absents : {r}");
}

// ---------------------------------------------------------------------------
// Capacités client et bogues de handler
// ---------------------------------------------------------------------------

#[test]
fn undeclared_client_capability_is_refused_with_32021() {
    let core = mrtr_core();

    // Sans la capacité `sampling` : refus -32021, forme officielle
    // `requiredCapabilities` (objet indexé par capacité).
    let r = call(
        &core,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": tools_params("sampler", json!({}), json!({})),
        }),
    );
    assert_eq!(error_code(&r), -32021, "{r}");
    assert_eq!(
        r["error"]["data"]["requiredCapabilities"]["sampling"],
        json!({})
    );
    assert!(r.get("result").is_none(), "aucun résultat attendu : {r}");

    // Sans la capacité `roots` : même verdict.
    let r = call(
        &core,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": tools_params("roots_ask", json!({}), json!({ "sampling": {} })),
        }),
    );
    assert_eq!(error_code(&r), -32021, "{r}");
    assert_eq!(
        r["error"]["data"]["requiredCapabilities"]["roots"],
        json!({})
    );

    // Avec les capacités déclarées : l'émission passe.
    let r = call(
        &core,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": tools_params("sampler", json!({}), client_caps()),
        }),
    );
    assert_eq!(r["result"]["resultType"], "input_required", "{r}");
    assert_eq!(
        r["result"]["inputRequests"]["gen"]["method"],
        "sampling/createMessage"
    );
}

#[test]
fn handler_bugs_are_internal_errors() {
    let core = mrtr_core();

    // Méthode non embarquable.
    let r = call(
        &core,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": tools_params("bogus_request", json!({}), client_caps()),
        }),
    );
    assert_eq!(error_code(&r), -32603, "méthode non embarquable : {r}");

    // Élicitation `form` sans `requestedSchema`.
    let r = call(
        &core,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": tools_params("formless_elicit", json!({}), client_caps()),
        }),
    );
    assert_eq!(error_code(&r), -32603, "form sans schéma : {r}");

    // Élicitation `url` sans `url`.
    let r = call(
        &core,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": tools_params("urlless_elicit", json!({}), client_caps()),
        }),
    );
    assert_eq!(error_code(&r), -32603, "url sans URL : {r}");

    // Spécification vide (ni requêtes ni état).
    let r = call(
        &core,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": tools_params("empty_spec", json!({}), client_caps()),
        }),
    );
    assert_eq!(error_code(&r), -32603, "spécification vide : {r}");

    // État dépassant la borne (8 Kio).
    let r = call(
        &core,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": tools_params("big_state", json!({}), client_caps()),
        }),
    );
    assert_eq!(error_code(&r), -32603, "état surdimensionné : {r}");
}

#[test]
fn url_elicit_round_trip() {
    let core = mrtr_core();
    let r = call(
        &core,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": tools_params("url_elicit", json!({}), client_caps()),
        }),
    );
    assert_eq!(r["result"]["resultType"], "input_required", "{r}");
    let approve = &r["result"]["inputRequests"]["approve"];
    assert_eq!(approve["method"], "elicitation/create");
    assert_eq!(approve["params"]["mode"], "url");
    assert_eq!(approve["params"]["url"], "https://example.test/approve");
}

// ---------------------------------------------------------------------------
// Multi-tours, champs superflus, prompts et ressources
// ---------------------------------------------------------------------------

#[test]
fn three_turn_cycle_keeps_state_across_rounds() {
    let core = mrtr_core();
    let r1 = call(
        &core,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": tools_params("walker", json!({}), client_caps()),
        }),
    );
    assert_eq!(r1["result"]["resultType"], "input_required");
    let token1 = r1["result"]["requestState"].as_str().unwrap();

    let r2 = call(
        &core,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "walker",
                "arguments": {},
                "_meta": { (META_CLIENT_CAPABILITIES): client_caps() },
                "inputResponses": { "go": { "action": "accept" } },
                "requestState": token1,
            },
        }),
    );
    assert_eq!(r2["result"]["resultType"], "input_required", "{r2}");
    let token2 = r2["result"]["requestState"].as_str().unwrap();
    assert_ne!(token1, token2, "chaque tour émet un nouveau jeton");

    let r3 = call(
        &core,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "walker",
                "arguments": {},
                "_meta": { (META_CLIENT_CAPABILITIES): client_caps() },
                "inputResponses": { "go": { "action": "accept" } },
                "requestState": token2,
            },
        }),
    );
    assert_eq!(r3["result"]["resultType"], "complete", "{r3}");
    assert_eq!(r3["result"]["structuredContent"]["done"], true);
    assert_eq!(r3["result"]["structuredContent"]["step"], 3);
}

#[test]
fn extra_params_and_unknown_response_keys_are_ignored() {
    let core = mrtr_core();
    let r1 = round_one(&core, "booking", 1);
    let token = r1["result"]["requestState"].as_str().unwrap();

    let r2 = call(
        &core,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "booking",
                "arguments": { "seat": "12A" },
                "_meta": {
                    (META_CLIENT_CAPABILITIES): client_caps(),
                    "trace": "un-champ-meta-supplementaire",
                },
                "inputResponses": {
                    "confirm": { "action": "accept", "content": { "confirm": true } },
                    "cle-inconnue": { "nimporte": [1, 2, 3] },
                },
                "requestState": token,
                "champ-superflu": { "ignore": true },
            },
        }),
    );
    assert_eq!(r2["result"]["resultType"], "complete", "{r2}");
    assert_eq!(r2["result"]["structuredContent"]["booked"], true);
}

#[test]
fn prompts_round_trip() {
    let core = mrtr_core();
    let r1 = call(
        &core,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "prompts/get",
            "params": {
                "name": "wizard",
                "_meta": { (META_CLIENT_CAPABILITIES): client_caps() },
            },
        }),
    );
    assert_eq!(r1["result"]["resultType"], "input_required", "{r1}");
    assert_eq!(
        r1["result"]["inputRequests"]["roots"]["method"],
        "roots/list"
    );
    let token = r1["result"]["requestState"].as_str().unwrap();

    let r2 = call(
        &core,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "prompts/get",
            "params": {
                "name": "wizard",
                "_meta": { (META_CLIENT_CAPABILITIES): client_caps() },
                "inputResponses": { "roots": { "roots": [] } },
                "requestState": token,
            },
        }),
    );
    assert_eq!(r2["result"]["resultType"], "complete", "{r2}");
    assert_eq!(
        r2["result"]["messages"][0]["content"]["text"],
        "wizard resumed, responses present: true"
    );
}

#[test]
fn resources_round_trip() {
    let core = mrtr_core();
    let r1 = call(
        &core,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "resources/read",
            "params": {
                "uri": "mem://vault",
                "_meta": { (META_CLIENT_CAPABILITIES): client_caps() },
            },
        }),
    );
    assert_eq!(r1["result"]["resultType"], "input_required", "{r1}");
    let token = r1["result"]["requestState"].as_str().unwrap();

    let r2 = call(
        &core,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "resources/read",
            "params": {
                "uri": "mem://vault",
                "_meta": { (META_CLIENT_CAPABILITIES): client_caps() },
                "inputResponses": { "key": { "action": "accept", "content": { "key": "opensesame" } } },
                "requestState": token,
            },
        }),
    );
    assert_eq!(r2["result"]["resultType"], "complete", "{r2}");
    assert_eq!(r2["result"]["contents"][0]["text"], "unlocked");
}

// ---------------------------------------------------------------------------
// Méthodes non concernées et annonce de capacités
// ---------------------------------------------------------------------------

#[test]
fn non_concerned_methods_ignore_mrtr_fields() {
    let core = mrtr_core();
    for method in ["tools/list", "resources/list", "server/discover"] {
        let r = call(
            &core,
            json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": method,
                "params": {
                    "requestState": "garbage.tampered",
                    "inputResponses": { "confirm": { "action": "accept" } },
                },
            }),
        );
        assert!(
            r.get("error").is_none(),
            "{method} ignore les champs MRTR : {r}"
        );
        assert_eq!(
            r["result"]["resultType"], "complete",
            "{method} ne répond jamais input_required : {r}"
        );
        // Aucun écho du jeton (rien n'a été traité).
        assert!(r["result"].get("requestState").is_none(), "{r}");
    }
}

#[test]
fn mrtr_declares_no_extra_server_capability() {
    let core = mrtr_core();
    let r = call(
        &core,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "server/discover",
            "params": {},
        }),
    );
    let caps = &r["result"]["capabilities"];
    assert!(caps["tools"].is_object(), "{r}");
    assert!(caps["resources"].is_object(), "{r}");
    assert!(caps["prompts"].is_object(), "{r}");
    // Le MRTR n'ajoute aucune capacité serveur : rien n'est servi au-delà
    // des résultats `input_required`, l'annonce ne prétend donc rien.
    for unexpected in ["mrtr", "multiRound", "elicitation", "sampling", "roots"] {
        assert!(
            caps.get(unexpected).is_none(),
            "capacité non servie annoncée : {unexpected} dans {caps}"
        );
    }
}

// ---------------------------------------------------------------------------
// Profil strict : statuts HTTP du cycle MRTR
// ---------------------------------------------------------------------------

#[test]
fn strict_profile_http_statuses() {
    let core = mrtr_core();

    // Tour 1 nominal → 200.
    let (status, r1) = strict_call(
        &core,
        1,
        "tools/call",
        strict_params("booking", json!({ "seat": "12A" }), client_caps()),
    );
    assert_eq!(status, 200, "{r1}");
    assert_eq!(r1["result"]["resultType"], "input_required");
    let token = r1["result"]["requestState"].as_str().unwrap();

    // Rejeu nominal → 200 + résultat terminal.
    let (status, r2) = strict_call(
        &core,
        2,
        "tools/call",
        json!({
            "name": "booking",
            "arguments": { "seat": "12A" },
            "_meta": {
                (META_PROTOCOL_VERSION): PROTOCOL_VERSION,
                (META_CLIENT_CAPABILITIES): client_caps(),
            },
            "inputResponses": { "confirm": { "action": "accept", "content": { "confirm": true } } },
            "requestState": token,
        }),
    );
    assert_eq!(status, 200, "{r2}");
    assert_eq!(r2["result"]["structuredContent"]["booked"], true);

    // État altéré → -32602 + HTTP 400.
    let (payload, signature) = token.split_once('.').unwrap();
    let (status, r3) = strict_call(
        &core,
        3,
        "tools/call",
        json!({
            "name": "booking",
            "arguments": { "seat": "12A" },
            "_meta": {
                (META_PROTOCOL_VERSION): PROTOCOL_VERSION,
                (META_CLIENT_CAPABILITIES): client_caps(),
            },
            "inputResponses": { "confirm": { "action": "accept" } },
            "requestState": format!("{}.{}", flip_first(payload), signature),
        }),
    );
    assert_eq!(status, 400, "{r3}");
    assert_eq!(error_code(&r3), -32602);

    // `inputResponses` sans `requestState` → -32602 + HTTP 400.
    let (status, r4) = strict_call(
        &core,
        4,
        "tools/call",
        json!({
            "name": "booking",
            "arguments": { "seat": "12A" },
            "_meta": {
                (META_PROTOCOL_VERSION): PROTOCOL_VERSION,
                (META_CLIENT_CAPABILITIES): client_caps(),
            },
            "inputResponses": { "confirm": { "action": "accept" } },
        }),
    );
    assert_eq!(status, 400, "{r4}");
    assert_eq!(error_code(&r4), -32602);

    // Capacité non déclarée → -32021 + HTTP 400.
    let (status, r5) = strict_call(
        &core,
        5,
        "tools/call",
        strict_params("sampler", json!({}), json!({})),
    );
    assert_eq!(status, 400, "{r5}");
    assert_eq!(error_code(&r5), -32021);
}
