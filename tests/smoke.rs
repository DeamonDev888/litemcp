//! Tests de fumée : le crate se compile et expose son contrat minimal.

#[test]
fn crate_version_is_exposed() {
    assert!(!litemcp::VERSION.is_empty());
}

#[test]
fn tool_parse_deserialize_failure_is_protocol_error_32602() {
    // Schéma : integer sans borne (accepte 1e2) ; struct : u32.
    // Un f64 entier hors u32 doit donner -32602 (protocole), pas isError.
    let core = litemcp::ServerBuilder::new("t", "0.2.0")
        .tool_parse(
            "pick",
            "Prend un entier",
            litemcp::zod::object().field("n", litemcp::zod::integer()),
            |args: PickArgs| Ok(serde_json::json!({ "n": args.n })),
        )
        .core();

    #[derive(serde::Deserialize)]
    struct PickArgs {
        n: u32,
    }

    let reply = core
        .dispatch(br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"pick","arguments":{"n":4294967296}}}"#)
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&reply).unwrap();
    assert_eq!(
        v["error"]["code"], -32602,
        "désérialisation typée = erreur protocole : {v}"
    );
    assert!(v.get("result").is_none());
}

#[test]
fn handler_panic_becomes_internal_error_not_connection_drop() {
    use std::io::Write;
    // Le catch_unwind vit dans le transport HTTP : test via une vraie socket.
    let bound = litemcp::ServerBuilder::new("t", "0.2.0")
        .legacy(true)
        .tool("boom", "Panique exprès", |_args| {
            panic!("boom volontaire");
        })
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = bound.local_addr().unwrap();
    std::thread::spawn(move || bound.serve().unwrap());

    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    let body =
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"boom","arguments":{}}}"#;
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = String::new();
    std::io::Read::read_to_string(&mut stream, &mut raw).unwrap();
    assert!(
        raw.contains("-32603"),
        "panique du handler → -32603 via HTTP : {raw:?}"
    );
    assert!(
        raw.contains("HTTP/1.1 200 OK"),
        "le panic est rendu comme résultat (-32603), connexion propre : {raw:?}"
    );
}

#[test]
fn zod_string_counts_characters_not_bytes() {
    use litemcp::zod::{Issues, Schema};
    // "é" = 1 caractère, 2 octets : max(3) doit accepter "ééé" (3 caractères)
    let schema = litemcp::zod::object().field("s", litemcp::zod::string().max(3));
    let mut issues = Issues::new();
    let mut path = litemcp::zod::Path::root("arguments");
    schema.validate(&mut path, &serde_json::json!({ "s": "ééé" }), &mut issues);
    assert!(
        issues.is_empty(),
        "3 caractères (6 octets) doit passer : {:?}",
        issues.render()
    );
}

#[test]
fn zod_integer_accepts_integral_floats_and_normalizes_them() {
    use litemcp::zod::{Issues, Schema};
    let schema = litemcp::zod::object().field("n", litemcp::zod::integer().max(200));

    // Les entiers JSON syntaxiques passent.
    let mut issues = Issues::new();
    let mut path = litemcp::zod::Path::root("arguments");
    schema.validate(&mut path, &serde_json::json!({ "n": 100 }), &mut issues);
    assert!(
        issues.is_empty(),
        "100 entier doit passer : {:?}",
        issues.render()
    );
    let mut issues = Issues::new();
    let mut path = litemcp::zod::Path::root("arguments");
    schema.validate(&mut path, &serde_json::json!({ "n": -3 }), &mut issues);
    assert!(issues.is_empty(), "-3 doit passer : {:?}", issues.render());

    // Sémantiques JSON Schema (draft 2020-12) : un nombre mathématiquement
    // intégral (`3.0`, `1e2`) est un `integer` — accepté, puis **normalisé
    // en entier exact** avant le handler (`Schema::normalize_integers`) :
    // la garantie `as_i64()` reste inconditionnelle, sans casser la
    // conformité du type publié.
    let mut issues = Issues::new();
    let mut path = litemcp::zod::Path::root("arguments");
    schema.validate(&mut path, &serde_json::json!({ "n": 1e2 }), &mut issues);
    assert!(
        issues.is_empty(),
        "1e2 (100 intégral) doit passer : {:?}",
        issues.render()
    );
    let mut issues = Issues::new();
    let mut path = litemcp::zod::Path::root("arguments");
    schema.validate(&mut path, &serde_json::json!({ "n": 3.0 }), &mut issues);
    assert!(
        issues.is_empty(),
        "3.0 (intégral) doit passer : {:?}",
        issues.render()
    );
    let mut issues = Issues::new();
    let mut path = litemcp::zod::Path::root("arguments");
    schema.validate(&mut path, &serde_json::json!({ "n": 1.5 }), &mut issues);
    assert!(!issues.is_empty(), "1.5 n'est pas entier");
    // Un intégral hors de la plage **publiée** (maximum : 200) est refusé
    // comme violation de borne — jamais converti ni saturé.
    let mut issues = Issues::new();
    let mut path = litemcp::zod::Path::root("arguments");
    schema.validate(&mut path, &serde_json::json!({ "n": 1e3 }), &mut issues);
    assert!(
        !issues.is_empty(),
        "1e3 (1000) dépasse le maximum publié (200) : {:?}",
        issues.render()
    );

    // La normalisation donne au handler un entier exact : `as_i64()`
    // réussit sur une valeur `3.0` validée.
    let mut value = serde_json::json!({ "n": 3.0 });
    schema.normalize_integers(&mut value);
    assert_eq!(value["n"].as_i64(), Some(3));
}
