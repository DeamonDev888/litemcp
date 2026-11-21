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
//!   même depuis un handler détaché après expiration du délai ;
//! * l'**annulation coopérative** : la fermeture du flux SSE par le client
//!   lève le drapeau `litemcp::cancel` et un handler qui le consulte sort de
//!   sa boucle (spec Streamable HTTP `2026-07-28`, MUST).
//!
//! Les tests parlent HTTP brut (socket réel), pas `ServerCore` : c'est le
//! transport qui est en cause, pas le dispatch.

#[path = "../examples/conformance_server/fixture.rs"]
mod fixture;

use litemcp::ServerBuilder;
use litemcp::protocol::{META_CLIENT_CAPABILITIES, META_PROTOCOL_VERSION, PROTOCOL_VERSION};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{Duration, Instant};

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

#[test]
fn closing_the_sse_stream_cancels_the_handler() {
    // Spec Streamable HTTP `2026-07-28` (MUST) : la fermeture du flux SSE est
    // traitée comme une annulation. Le transport ne peut pas interrompre un
    // fil : il lève le drapeau `litemcp::cancel` à la première écriture qui
    // échoue, et le handler sort de sa boucle à son prochain point sûr.
    //
    // Budget borné dans les deux sens : le handler abandonne de lui-même au
    // bout de 5 s (échec propre du test si le drapeau n'arrive jamais), le
    // test attend le verdict 6 s au plus.
    let observed = std::sync::Arc::new(AtomicBool::new(false));
    let drapeau = std::sync::Arc::clone(&observed);
    let addr = spawn(ServerBuilder::new("cancel-sse", "1.0.0").tool_result(
        "boucle",
        "Boucle jusqu'à la fermeture du flux",
        move |_args| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if litemcp::cancel::is_cancelled() {
                    drapeau.store(true, Ordering::SeqCst);
                    return Ok(litemcp::protocol::CallToolResult::from_content([
                        litemcp::protocol::ContentBlock::text("cancelled"),
                    ]));
                }
                // Chaque rapport est une écriture SSE : c'est elle qui
                // révèle la déconnexion du client.
                let _ = litemcp::progress::report(1.0, Some(1000.0), None);
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(litemcp::protocol::CallToolResult::from_content([
                litemcp::protocol::ContentBlock::text("budget épuisé"),
            ]))
        },
    ));

    let request = tool_call("boucle", Some("tok-cancel"));
    let body = request.to_string();
    let raw = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: text/event-stream\r\n\
         MCP-Protocol-Version: {PROTOCOL_VERSION}\r\nMcp-Method: tools/call\r\n\
         Mcp-Name: boucle\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(addr).expect("connexion");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout de lecture");
    stream.write_all(raw.as_bytes()).expect("écriture");

    // On lit jusqu'au premier événement : la tête est partie et le handler
    // tourne déjà quand on raccroche.
    let mut reader = BufReader::new(stream.try_clone().expect("clone du socket"));
    let mut line = String::new();
    reader.read_line(&mut line).expect("ligne de statut");
    assert!(line.starts_with("HTTP/1.1 200 OK"), "{line}");
    let mut seen_progress = false;
    while !seen_progress {
        line.clear();
        if reader.read_line(&mut line).expect("ligne du flux") == 0 {
            break;
        }
        seen_progress = line.starts_with("data: ");
    }
    assert!(seen_progress, "aucune notification lue avant la fermeture");

    // Fermeture brutale du flux SSE (les deux poignées du socket).
    drop(reader);
    drop(stream);

    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline && !observed.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        observed.load(Ordering::SeqCst),
        "la fermeture du flux SSE doit lever le drapeau d'annulation vu par le handler"
    );
}

#[test]
fn a_json_client_never_sees_a_cancellation() {
    // Chemin nominal : aucun flux SSE, donc aucun jeton — `is_cancelled` est
    // une non-action (`false`) et le handler ne peut pas croire à tort que
    // son client est parti.
    let observed = std::sync::Arc::new(AtomicBool::new(false));
    let vue = std::sync::Arc::clone(&observed);
    let addr = spawn(ServerBuilder::new("cancel-json", "1.0.0").tool_result(
        "sonde",
        "Lit le drapeau d'annulation",
        move |_args| {
            vue.store(litemcp::cancel::is_cancelled(), Ordering::SeqCst);
            assert!(
                litemcp::cancel::token().is_none(),
                "aucun jeton hors flux SSE"
            );
            Ok(litemcp::protocol::CallToolResult::from_content([
                litemcp::protocol::ContentBlock::text("ok"),
            ]))
        },
    ));
    let request = tool_call("sonde", None);
    let (head, body) = post(addr, "application/json", &request);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    let response: Value = serde_json::from_str(body.trim()).expect("JSON-RPC");
    assert_eq!(response["id"], request["id"]);
    assert!(
        !observed.load(Ordering::SeqCst),
        "le handler ne doit voir aucune annulation sur le chemin JSON"
    );
}

/// Le budget d'écriture ne borne **pas la durée de vie** du flux : un handler
/// plus long que `write_deadline` qui rapporte garde son flux **et** sa
/// réponse finale. Seul le temps réellement passé à écrire est décompté —
/// sinon `no_tool_timeout()` deviendrait inutilisable dès qu'un outil dépasse
/// le budget (client sain, flux coupé, réponse perdue).
#[test]
fn un_handler_long_garde_son_flux_et_sa_reponse() {
    let annule = std::sync::Arc::new(AtomicBool::new(false));
    let vu_annulation = annule.clone();
    let addr = spawn(
        ServerBuilder::new("echeance-sse", "1.0.0")
            .write_deadline(Duration::from_secs(1))
            .no_tool_timeout()
            .tool_result("rapporte", "Rapporte après l'échéance", move |_args| {
                assert!(
                    litemcp::progress::report(0.0, Some(100.0), None),
                    "première émission dans le budget"
                );
                std::thread::sleep(Duration::from_millis(1300));
                let seconde = litemcp::progress::report(50.0, Some(100.0), None);
                vu_annulation.store(litemcp::cancel::is_cancelled(), Ordering::SeqCst);
                assert!(
                    seconde,
                    "le silence du handler ne consomme pas le budget d'écriture"
                );
                Ok(litemcp::protocol::CallToolResult::from_content([
                    litemcp::protocol::ContentBlock::text("fin"),
                ]))
            }),
    );
    let request = tool_call("rapporte", Some("tok-echeance"));
    let body = request.to_string();
    let raw = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: text/event-stream\r\n\
         MCP-Protocol-Version: {PROTOCOL_VERSION}\r\nMcp-Method: tools/call\r\n\
         Mcp-Name: rapporte\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(addr).expect("connexion");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("timeout de lecture");
    stream.write_all(raw.as_bytes()).expect("écriture");

    // Lecture jusqu'à la fermeture du flux.
    let mut recu = String::new();
    let mut tampon = [0u8; 1024];
    loop {
        match stream.read(&mut tampon) {
            Ok(0) => break,
            Ok(n) => recu.push_str(&String::from_utf8_lossy(&tampon[..n])),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                ) =>
            {
                break;
            }
            Err(error) => panic!("lecture du flux : {error}"),
        }
    }

    let (head, flux) = recu.split_once("\r\n\r\n").expect("tête HTTP complète");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert!(
        flux.contains("\"result\""),
        "la réponse finale doit être écrite malgré le dépassement du budget : {flux}"
    );
    assert_eq!(
        progress_events(&events(flux)).len(),
        2,
        "les deux émissions (avant et après le silence) doivent partir : {flux}"
    );
    assert!(
        !annule.load(Ordering::SeqCst),
        "un handler long vers un client qui lit ne doit pas être annulé"
    );
}
