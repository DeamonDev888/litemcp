//! Canal `notifications/progress` **request-scoped** — régression du
//! transport SSE ouvert pendant l'exécution d'un handler.
//!
//! La spec MCP réserve `notifications/progress` aux requêtes qui fournissent
//! `params._meta.progressToken` ; la réponse doit alors être un flux
//! `text/event-stream` sur le POST, les notifications précédant l'événement
//! de réponse. Ces tests figent :
//!
//! * l'émission réelle des trois notifications du scénario officiel
//!   `tools-call-with-progress` (0/50/100, jeton repris tel quel) avant la
//!   réponse finale ;
//! * le chemin nominal inchangé (JSON en un seul write) sans jeton ;
//! * la propagation du canal quand le handler tourne dans un fil détaché
//!   (`tool_timeout`) ;
//! * la fermeture du canal : aucune notification ne peut suivre la réponse,
//!   même depuis un handler détaché après expiration du délai.
//!
//! Les tests parlent HTTP brut (socket réel), pas `ServerCore` : c'est le
//! transport qui est en cause, pas le dispatch.

#[path = "../examples/conformance_server/fixture.rs"]
mod fixture;

use litemcp::ServerBuilder;
use litemcp::protocol::{META_CLIENT_CAPABILITIES, META_PROTOCOL_VERSION, PROTOCOL_VERSION};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

fn next_id() -> i64 {
    static ID: AtomicI64 = AtomicI64::new(1);
    ID.fetch_add(1, Ordering::Relaxed)
}

fn spawn(builder: ServerBuilder) -> SocketAddr {
    let bound = builder.bind("127.0.0.1:0").expect("bind éphémère");
    let addr = bound.local_addr().expect("adresse locale");
    std::thread::spawn(move || bound.serve().expect("boucle de service"));
    addr
}

fn spawn_fixture() -> SocketAddr {
    let bound = fixture::build_fixture("127.0.0.1:0").expect("fixture liée");
    let addr = bound.local_addr().expect("adresse locale");
    std::thread::spawn(move || bound.serve().expect("boucle de service"));
    addr
}

/// POST brut ; rend `(head HTTP, corps)`.
fn post(addr: SocketAddr, accept: &str, body: &Value) -> (String, String) {
    let body = body.to_string();
    let method = body_method(&body);
    let name = body_name(&body);
    let mut headers =
        format!("MCP-Protocol-Version: {PROTOCOL_VERSION}\r\nMcp-Method: {method}\r\n");
    if let Some(name) = name {
        headers.push_str(&format!("Mcp-Name: {name}\r\n"));
    }
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: {accept}\r\n\
         {headers}Content-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(addr).expect("connexion");
    stream.write_all(request.as_bytes()).expect("écriture");
    let mut raw = String::new();
    stream.read_to_string(&mut raw).expect("lecture");
    let (head, body) = raw.split_once("\r\n\r\n").expect("réponse HTTP complète");
    (head.to_string(), body.to_string())
}

fn body_method(body: &str) -> String {
    serde_json::from_str::<Value>(body).unwrap()["method"]
        .as_str()
        .unwrap()
        .to_string()
}

fn body_name(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).unwrap();
    value["params"]["name"].as_str().map(str::to_owned)
}

/// Événements `data:` du flux SSE, dans l'ordre d'arrivée.
fn events(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|payload| serde_json::from_str(payload).expect("événement JSON"))
        .collect()
}

fn progress_events(events: &[Value]) -> Vec<&Value> {
    events
        .iter()
        .filter(|event| event["method"] == "notifications/progress")
        .collect()
}

fn tool_call(name: &str, progress_id: Option<&str>) -> Value {
    let mut params = json!({ "name": name, "arguments": {} });
    params["_meta"] = json!({
        META_PROTOCOL_VERSION: PROTOCOL_VERSION,
        META_CLIENT_CAPABILITIES: { "sampling": {}, "elicitation": {}, "roots": {} },
    });
    if let Some(id) = progress_id {
        params["_meta"]["progressToken"] = json!(id);
    }
    json!({
        "jsonrpc": "2.0",
        "id": next_id(),
        "method": "tools/call",
        "params": params,
    })
}

#[test]
fn embedded_dispatch_forwards_reports_to_the_channel() {
    // API publique pour un hôte qui possède déjà son transport : le canal
    // est fourni au dispatch, les handlers le joignent via `progress::report`.
    type Call = (f64, Option<f64>, Option<String>);
    struct Recorder {
        calls: std::sync::Mutex<Vec<Call>>,
    }
    impl litemcp::progress::ProgressChannel for Recorder {
        fn progress(&self, progress: f64, total: Option<f64>, message: Option<&str>) -> bool {
            self.calls.lock().unwrap().push((
                progress,
                total,
                message.map(std::string::ToString::to_string),
            ));
            true
        }
    }

    let core = ServerBuilder::new("embedded", "1.0.0")
        .tool_result("emit", "Rapporte sa progression", |_args| {
            assert!(litemcp::progress::report(25.0, Some(50.0), Some("quart")));
            Ok(litemcp::protocol::CallToolResult::from_content([
                litemcp::protocol::ContentBlock::text("ok"),
            ]))
        })
        .core();
    let body = tool_call("emit", Some("tok-embedded"));
    let headers = litemcp::McpHeaders {
        protocol_version: Some(PROTOCOL_VERSION.to_string()),
        method: Some("tools/call".to_string()),
        name: Some("emit".to_string()),
        params: Vec::new(),
    };
    let recorder = std::sync::Arc::new(Recorder {
        calls: std::sync::Mutex::new(Vec::new()),
    });
    let response = core
        .dispatch_request_with_progress(
            body.to_string().as_bytes(),
            Some(&headers),
            recorder.clone(),
        )
        .expect("une réponse pour une requête");
    assert_eq!(response.status.as_str(), "200 OK");
    let calls = recorder.calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "une notification par report");
    assert_eq!(calls[0].0, 25.0);
    assert_eq!(calls[0].1, Some(50.0));
    assert_eq!(calls[0].2.as_deref(), Some("quart"));
}

#[test]
fn progress_notifications_stream_before_the_response() {
    let addr = spawn_fixture();
    let request = tool_call("test_tool_with_progress", Some("progress-test-1"));
    let (head, body) = post(addr, "application/json, text/event-stream", &request);

    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("content-type: text/event-stream"),
        "flux SSE attendu : {head}"
    );
    assert!(
        !head.to_ascii_lowercase().contains("content-length"),
        "un flux ouvert ne peut pas annoncer sa taille : {head}"
    );

    let events = events(&body);
    let progress = progress_events(&events);
    assert!(
        progress.len() >= 3,
        "au moins trois notifications (scénario officiel) : {body}"
    );
    for notification in &progress {
        assert_eq!(
            notification["params"]["progressToken"], "progress-test-1",
            "le jeton de la requête est repris tel quel : {notification}"
        );
        assert_eq!(notification["params"]["total"], 100.0);
    }
    let values: Vec<f64> = progress
        .iter()
        .filter_map(|event| event["params"]["progress"].as_f64())
        .collect();
    assert_eq!(
        &values[..3],
        &[0.0, 50.0, 100.0],
        "progression croissante attendue : {values:?}"
    );
    assert!(
        values.windows(2).all(|pair| pair[0] <= pair[1]),
        "les valeurs ne régressent jamais : {values:?}"
    );

    // La réponse finale est le dernier événement, avec l'id de la requête.
    let last = events.last().expect("événement de réponse");
    assert_eq!(last["id"], request["id"], "id corrélé : {last}");
    assert_eq!(
        last["result"]["content"][0]["text"], "Progress tool executed.",
        "{last}"
    );
    assert!(
        events
            .iter()
            .rposition(|event| event["method"] == "notifications/progress")
            < Some(events.len() - 1),
        "les notifications précèdent la réponse : {body}"
    );
}

#[test]
fn without_a_token_the_response_stays_json() {
    let addr = spawn_fixture();
    let request = tool_call("test_tool_with_progress", None);
    let (head, body) = post(addr, "application/json, text/event-stream", &request);

    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("content-type: application/json"),
        "sans jeton, réponse JSON unique : {head}"
    );
    let response: Value = serde_json::from_str(body.trim()).expect("JSON-RPC");
    assert_eq!(response["id"], request["id"]);
    assert_eq!(
        response["result"]["content"][0]["text"],
        "Progress tool executed."
    );
}

#[test]
fn json_only_client_gets_no_stream_even_with_a_token() {
    let addr = spawn_fixture();
    let request = tool_call("test_tool_with_progress", Some("progress-test-2"));
    let (head, body) = post(addr, "application/json", &request);

    assert!(
        head.to_ascii_lowercase()
            .contains("content-type: application/json"),
        "un client JSON n'obtient pas de flux : {head}"
    );
    let response: Value = serde_json::from_str(body.trim()).expect("JSON-RPC");
    assert_eq!(
        response["result"]["content"][0]["text"],
        "Progress tool executed."
    );
}

#[test]
fn progress_channel_crosses_the_tool_timeout_thread() {
    // Avec `tool_timeout`, le handler tourne dans un fil détaché : le canal
    // doit y être réinstallé, sinon `report` serait une non-action.
    let addr = spawn(
        ServerBuilder::new("progress-timeout", "1.0.0")
            .tool_timeout(Duration::from_secs(5))
            .tool_result("emit", "Rapporte sa progression", |_args| {
                for value in [0.0, 50.0, 100.0] {
                    assert!(
                        litemcp::progress::report(value, Some(100.0), None),
                        "le canal doit être disponible dans le fil du handler"
                    );
                }
                Ok(litemcp::protocol::CallToolResult::from_content([
                    litemcp::protocol::ContentBlock::text("done"),
                ]))
            }),
    );
    let request = tool_call("emit", Some("tok-thread"));
    let (head, body) = post(addr, "text/event-stream", &request);
    assert!(head.contains("text/event-stream"), "{head}");

    let events = events(&body);
    let progress = progress_events(&events);
    assert_eq!(progress.len(), 3, "{body}");
    assert_eq!(progress[1]["params"]["progress"], 50.0);
    assert_eq!(
        events.last().unwrap()["result"]["content"][0]["text"],
        "done"
    );
}

#[test]
fn no_progress_after_the_response_even_from_a_detached_handler() {
    // Le handler dépasse le délai puis tente de rapporter : le canal est
    // fermé avant l'écriture de la réponse, l'émission tardive est ignorée
    // et ne peut pas se glisser après l'événement final.
    let addr = spawn(
        ServerBuilder::new("progress-late", "1.0.0")
            .tool_timeout(Duration::from_millis(50))
            .tool_result("slow", "Dépasse le délai", |_args| {
                std::thread::sleep(Duration::from_millis(150));
                assert!(
                    !litemcp::progress::report(99.0, Some(100.0), None),
                    "le canal doit être fermé après la réponse"
                );
                Ok(litemcp::protocol::CallToolResult::from_content([
                    litemcp::protocol::ContentBlock::text("late"),
                ]))
            }),
    );
    let request = tool_call("slow", Some("tok-late"));
    let (head, body) = post(addr, "text/event-stream", &request);
    assert!(head.contains("text/event-stream"), "{head}");

    let events = events(&body);
    assert!(
        progress_events(&events).is_empty(),
        "aucune notification ne suit la réponse : {body}"
    );
    let last = events.last().expect("réponse");
    assert_eq!(
        last["result"]["isError"], true,
        "le délai dépassé est rendu en erreur d'outil : {last}"
    );
}
