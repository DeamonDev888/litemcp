//! Parité README ↔ code : chaque exemple documenté est rejoué **de bout en
//! bout** (vraies sockets) et la réponse obtenue est comparée à celle que le
//! README affiche. Si l'un de ces tests échoue, soit le code a changé et le
//! README doit être mis à jour, soit le README promet quelque chose que le
//! code ne fait plus — dans les deux cas, l'écart est intercepté avant
//! publication.

use litemcp::ServerBuilder;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};

/// Serveur identique à l'exemple « Intégration » du README.
fn spawn_readme_server() -> SocketAddr {
    let bound = ServerBuilder::new("calc", "1.0.0")
        .legacy(true)
        .tool("add", "Additionne deux nombres", |args| {
            let a = args["a"].as_i64().ok_or("a manquant")?;
            let b = args["b"].as_i64().ok_or("b manquant")?;
            Ok(serde_json::json!({ "somme": a + b }))
        })
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = bound.local_addr().unwrap();
    std::thread::spawn(move || bound.serve().unwrap());
    addr
}

fn post_body(addr: SocketAddr, body: &str) -> (u16, String) {
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: test\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    let (head, rest) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (status, rest.to_string())
}

#[test]
fn readme_integration_example_returns_the_documented_response() {
    let addr = spawn_readme_server();
    // Le corps exact du curl documenté dans le README.
    let (status, body) = post_body(
        addr,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"add","arguments":{"a":5,"b":7}}}"#,
    );
    assert_eq!(status, 200);
    // La réponse documentée dans le README — octet pour octet (l'ordre des
    // champs compte pour un exemple affiché : un lecteur qui compare la
    // sortie réelle au README ne doit pas voir de divergence).
    let documented = r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","content":[{"type":"text","text":"{\"somme\":12}"}],"structuredContent":{"somme":12},"_meta":{"io.modelcontextprotocol/serverInfo":{"name":"calc","version":"1.0.0"}}}}"#;
    assert_eq!(body, documented, "la réponse réelle diffère du README");
}

#[test]
fn readme_zod_concept_example_publishes_and_enforces_the_schema() {
    // Exemple « Concepts — Outils » du README : `tool_parse` avec un champ
    // requis (query, 2..=64) et un champ optionnel (limit, 1..=100).
    let bound = ServerBuilder::new("recherche", "1.0.0")
        .legacy(true)
        .tool_parse(
            "search",
            "Recherche indexée",
            litemcp::zod::object()
                .field("query", litemcp::zod::string().min(2).max(64))
                .opt("limit", litemcp::zod::integer().min(1).max(100)),
            |args: SearchArgs| {
                Ok(json!({ "hits": args.query.len() as i64 + args.limit.unwrap_or(0) as i64 }))
            },
        )
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = bound.local_addr().unwrap();
    std::thread::spawn(move || bound.serve().unwrap());

    // `tools/list` publie l'inputSchema dérivé : les contraintes deviennent
    // les mots-clés JSON Schema documentés (minLength/maxLength/minimum/maximum).
    let (_, body) = post_body(addr, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let reply: Value = serde_json::from_str(&body).unwrap();
    let tool = &reply["result"]["tools"][0];
    assert_eq!(tool["name"], "search");
    assert_eq!(
        tool["inputSchema"],
        json!({
            "type": "object",
            "properties": {
                "limit": { "type": "integer", "minimum": 1, "maximum": 100 },
                "query": { "type": "string", "minLength": 2, "maxLength": 64 }
            },
            "required": ["query"]
        })
    );

    // Appel dans le schéma : le handler tourne.
    let (_, body) = post_body(
        addr,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search","arguments":{"query":"rust","limit":5}}}"#,
    );
    let reply: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(reply["result"]["structuredContent"], json!({ "hits": 9 }));

    // Appel hors schéma : -32602, issues dans error.data.issues, et le
    // handler NE tourne PAS (promise « validation avant exécution »).
    let (_, body) = post_body(
        addr,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search","arguments":{"query":"x"}}}"#,
    );
    let reply: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(reply["error"]["code"], -32602);
    assert!(
        reply["error"]["data"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue.as_str().unwrap().contains("arguments.query")),
        "l'issue doit citer le chemin du champ fautif : {reply}"
    );
}

#[derive(serde::Deserialize)]
struct SearchArgs {
    query: String,
    limit: Option<u32>,
}

#[test]
fn readme_default_example_is_published_and_injected() {
    // Exemple « Valeurs par défaut » du README : `default(10)` est publié
    // (`default: 10`, champ sorti de `required`) **et** injecté si absent.
    let bound = ServerBuilder::new("defauts", "1.0.0")
        .legacy(true)
        .tool_parse(
            "recherche",
            "Avec défaut",
            litemcp::zod::object()
                .field("query", litemcp::zod::string())
                .opt("limit", litemcp::zod::integer().default(10)),
            |args: DefaultArgs| Ok(json!({ "limit": args.limit })),
        )
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = bound.local_addr().unwrap();
    std::thread::spawn(move || bound.serve().unwrap());

    // Publié : `default: 10`, et `limit` hors `required`.
    let (_, body) = post_body(addr, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let reply: Value = serde_json::from_str(&body).unwrap();
    let schema = &reply["result"]["tools"][0]["inputSchema"];
    assert_eq!(schema["properties"]["limit"]["default"], 10);
    assert_eq!(schema["required"], json!(["query"]));

    // Appliqué : sans "limit", le handler reçoit 10.
    let (_, body) = post_body(
        addr,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"recherche","arguments":{"query":"ok"}}}"#,
    );
    let reply: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(reply["result"]["structuredContent"], json!({ "limit": 10 }));

    // Présent : la valeur du client gagne.
    let (_, body) = post_body(
        addr,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"recherche","arguments":{"query":"ok","limit":42}}}"#,
    );
    let reply: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(reply["result"]["structuredContent"], json!({ "limit": 42 }));
}

#[derive(serde::Deserialize)]
struct DefaultArgs {
    #[allow(dead_code)]
    query: String,
    limit: Option<u32>,
}

#[test]
fn readme_record_example_validates_every_value() {
    // Exemple « Dictionnaires » du README : `record(string())` valide chaque
    // valeur ; `{"a": 1}` est refusé avec le chemin `arguments.labels.a`.
    let bound = ServerBuilder::new("dict", "1.0.0")
        .legacy(true)
        .tool_typed(
            "labels",
            "Dictionnaire",
            litemcp::zod::object().field("labels", litemcp::zod::record(litemcp::zod::string())),
            |args| Ok(args.clone()),
        )
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = bound.local_addr().unwrap();
    std::thread::spawn(move || bound.serve().unwrap());

    let (_, body) = post_body(
        addr,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"labels","arguments":{"labels":{"a":"x"}}}}"#,
    );
    let reply: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        reply["result"]["structuredContent"],
        json!({ "labels": { "a": "x" } })
    );

    let (_, body) = post_body(
        addr,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"labels","arguments":{"labels":{"a":1}}}}"#,
    );
    let reply: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(reply["error"]["code"], -32602);
    let issues: Vec<String> = reply["error"]["data"]["issues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i.as_str().unwrap().to_string())
        .collect();
    assert!(
        issues.iter().any(|i| i.contains("arguments.labels.a")),
        "le chemin d'issue doit pointer la clé fautive : {issues:?}"
    );
}

#[test]
fn readme_health_probe_answers_ok() {
    // Le README promet `GET /health` → `200` « ok » sans JSON-RPC.
    let addr = spawn_readme_server();
    let request = "GET /health HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n";
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    assert!(raw.starts_with("HTTP/1.1 200"), "{raw:?}");
    assert!(raw.contains("Content-Type: text/plain"), "{raw:?}");
    let (_, body) = raw.split_once("\r\n\r\n").unwrap();
    assert_eq!(body, "ok");
}
