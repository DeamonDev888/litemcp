//! Tests de l'auto-attribution de port (`ServerBuilder::bind_default`) :
//! premier port libre de la tranche LiteMCP (`DEFAULT_PORT_RANGE`,
//! 23080..=23099), repli sur un port éphémère si la tranche entière est
//! occupée — jamais d'échec faute de place.
//!
//! Les scénarios vivent dans **un seul** test séquentiel : les tests du
//! binaire tournent en parallèle et partagent la même tranche de ports.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};

fn build() -> litemcp::ServerBuilder {
    litemcp::ServerBuilder::new("t", "0.1.0").tool("add", "Additionne", |args| {
        let a = args["a"].as_i64().unwrap_or(0);
        let b = args["b"].as_i64().unwrap_or(0);
        Ok(serde_json::json!({ "somme": a + b }))
    })
}

/// Sonde de vie : `GET /health` doit répondre « ok » sur un serveur **servi**
/// (le bind seul n'accepte personne — la boucle `serve()` doit tourner).
fn health_ok(addr: std::net::SocketAddr) -> bool {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    raw.contains("HTTP/1.1 200 OK") && raw.contains("ok")
}

#[test]
fn bind_default_scans_the_range_then_falls_back_to_ephemeral() {
    // 1. Les cinq premiers ports sont occupés : bind_default prend le
    //    **premier libre** de la tranche (23085 ici), jamais un port pris.
    let mut held = Vec::new();
    for port in 23080..=23084 {
        held.push(TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap());
    }
    let bound = build().bind_default().unwrap();
    let first = bound.local_addr().unwrap().port();
    assert!(
        (23085..=23099).contains(&first),
        "le premier libre de la tranche doit gagner : {first}"
    );
    drop(bound); // Port rendu avant le remplissage complet de la tranche.

    // 2. La tranche entière est occupée : repli **éphémère** — le bind
    //    réussit quand même, hors tranche.
    for port in 23085..=23099 {
        held.push(TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap());
    }
    let bound = build().bind_default().unwrap();
    let second = bound.local_addr().unwrap().port();
    assert_ne!(second, 0, "port éphémère effectif");
    assert!(
        !(23080..=23099).contains(&second),
        "le repli sort de la tranche : {second}"
    );
    drop(bound);

    // 3. Deux serveurs côte à côte sans coordination : ports distincts,
    //    boucles serve() réelles, sonde /health répondante.
    drop(held);
    let a = build().bind_default().unwrap();
    let b = build().bind_default().unwrap();
    let (addr_a, addr_b) = (a.local_addr().unwrap(), b.local_addr().unwrap());
    assert_ne!(
        addr_a.port(),
        addr_b.port(),
        "deux serveurs, deux ports : {} vs {}",
        addr_a.port(),
        addr_b.port()
    );
    for bound in [a, b] {
        let addr = bound.local_addr().unwrap();
        std::thread::spawn(move || bound.serve().unwrap());
        assert!(health_ok(addr), "sonde /health sur {addr}");
    }
}

#[test]
fn bind_default_validates_the_route_like_bind() {
    // Route invalide : refusée avant tout bind, comme `.bind(addr)`.
    let error = match build().route("invalid-route").bind_default() {
        Err(error) => error,
        Ok(_) => panic!("une route sans '/' doit être refusée"),
    };
    assert_eq!(
        error.kind(),
        std::io::ErrorKind::InvalidInput,
        "route sans '/' refusée : {error}"
    );
}
