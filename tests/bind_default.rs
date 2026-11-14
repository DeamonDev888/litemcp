//! Tests de l'auto-attribution de port (`ServerBuilder::bind_default`) :
//! premier port libre de la tranche LiteMCP (`DEFAULT_PORT_RANGE`,
//! 23080..=23099), repli sur un port éphémère si la tranche entière est
//! occupée — jamais d'échec faute de place.
//!
//! Les scénarios vivent dans **un seul** test séquentiel : les tests du
//! binaire tournent en parallèle et partagent la même tranche de ports.
//!
//! Le test ne suppose **aucun** port libre : un serveur résiduel
//! (`target\debug\examples\hello.exe` du dépôt, autre outil…) peut écouter
//! sur 23080. Il photographie donc la tranche en réservant lui-même les ports
//! libres (`try_hold`) — 23080 compris quand il est disponible — note ceux
//! qu'un autre processus détient, puis calcule le premier port libre attendu
//! à partir de cette photographie.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};

/// Réserve `port` sur loopback s'il est libre. `None` : port déjà détenu par
/// un autre processus (ou refus du système) — dans les deux cas indisponible
/// pour `bind_default`, qui doit passer au suivant. Un port réservé le reste
/// tant que le `TcpListener` rendu vit.
fn try_hold(port: u16) -> Option<TcpListener> {
    TcpListener::bind((Ipv4Addr::LOCALHOST, port)).ok()
}

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
    // 0. Photographie de la tranche, sans hypothèse sur l'état de la machine :
    //    on réserve tous les ports libres (23080 inclus dès qu'il l'est) et on
    //    note ceux qu'un autre processus détient déjà. Aucun `unwrap` sur un
    //    bind de port fixe : un port pris n'est plus une panique, c'est une
    //    donnée d'entrée.
    let range = litemcp::DEFAULT_PORT_RANGE;
    let mut held: Vec<(u16, TcpListener)> = Vec::new();
    let mut elsewhere: Vec<u16> = Vec::new();
    for port in range.clone() {
        match try_hold(port) {
            Some(listener) => held.push((port, listener)),
            None => elsewhere.push(port),
        }
    }
    eprintln!(
        "tranche {}..={} : libres={:?} déjà occupés={elsewhere:?}",
        range.start(),
        range.end(),
        held.iter().map(|(port, _)| *port).collect::<Vec<_>>()
    );

    if held.is_empty() {
        // Les vingt ports sont détenus par d'autres processus : le contrat
        // « premier libre » n'a pas d'instance à vérifier ici, seul le repli
        // l'est. On le dit à voix haute plutôt que de rendre le test muet.
        eprintln!(
            "tranche entièrement occupée par d'autres processus : scénario « premier libre » sans objet"
        );
        let bound = build().bind_default().unwrap();
        let addr = bound.local_addr().unwrap();
        assert!(
            addr.ip().is_loopback(),
            "le repli reste sur loopback : {addr}"
        );
        assert!(
            !range.contains(&addr.port()),
            "le repli sort de la tranche : {addr}"
        );
        return;
    }

    // 1. Premier port libre : tout ce qui est **sous** `target` est occupé
    //    (par nous ou par un autre processus). On rend `target` à l'OS juste
    //    avant l'appel : c'est alors le seul port libre de la tranche, et
    //    bind_default doit tomber exactement dessus — il saute donc bien les
    //    ports pris, y compris 23080 quand un serveur résiduel y écoute.
    let (target, target_hold) = held.remove(0);
    drop(target_hold);
    let bound = build().bind_default().unwrap();
    let addr = bound.local_addr().unwrap();
    assert_eq!(
        addr.port(),
        target,
        "le premier port libre de la tranche doit gagner (libres observés={target}, occupés={elsewhere:?})"
    );
    assert!(
        addr.ip().is_loopback(),
        "bind_default écoute sur loopback : {addr}"
    );

    // 2. Tranche entière occupée : `bound` détient `target`, nous détenons
    //    tout le reste de la tranche → repli **éphémère**, hors tranche, et
    //    toujours sur loopback.
    let fallback = build().bind_default().unwrap();
    let fallback_addr = fallback.local_addr().unwrap();
    assert_ne!(fallback_addr.port(), 0, "port éphémère effectif");
    assert!(
        !range.contains(&fallback_addr.port()),
        "le repli sort de la tranche : {fallback_addr}"
    );
    assert!(
        fallback_addr.ip().is_loopback(),
        "le repli reste sur loopback : {fallback_addr}"
    );
    drop(fallback);
    drop(bound);

    // 3. Deux serveurs côte à côte sans coordination : ports distincts,
    //    boucles serve() réelles, sonde /health répondante.
    drop(held); // Ports rendus à l'OS : 23080..=23099 redevient disponible.
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
