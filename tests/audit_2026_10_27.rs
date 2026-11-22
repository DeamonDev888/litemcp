//! Test de régression — audit 2026-10-27.
//! Chaque test rejoue une SONDE qui avait PROUVÉ un bug avant correction.
//! Un test qui passe ici = le bug est corrigé (et le restera).

use serde_json::json;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

// ---------------------------------------------------------------- C2
// Résidu de corps : `Content-Length: 0` + 36 octets parasites, puis une
// requête valide. AVANT : la réponse à `id=2` était détruite par un 400
// parasite. APRÈS : le résidu non-conforme est attribué à la requête qui
// a mal annoncé sa longueur → 400 sur CETTE requête, la requête suivante
// est servie normalement.
// ---------------------------------------------------------------- C2
// Résidu de corps : `Content-Length: 0` + des octets qui ne ressemblent à
// aucune ligne de requête.
//
// AVANT : le résidu était relu comme une LIGNE DE REQUÊTE au tour suivant →
// la requête courante recevait SA réponse (correcte) puis un `400` PARASITE
// s'intercalait, et la file de réponses du client était décalée (2 réponses
// pour 1 requête).
//
// APRÈS : le résidu non conforme est attribué à la requête qui a mal annoncé
// sa longueur → UNE seule réponse, un `400` explicite (« request body larger
// than Content-Length »), et la connexion est fermée.
//
// Note protocolaire : une erreur de cadrage impose la fermeture (RFC 9112
// §6.3) — les requêtes pipelinées qui suivent sont perdues par construction,
// pour tout serveur HTTP. Ce qui est corrigé, c'est le DÉCALAGE de la file
// de réponses (le client ne reçoit plus une réponse parasite en plus de la
// sienne).
#[test]
fn c2_residu_non_conforme_ne_cree_pas_de_reponse_parasite() {
    let addr = spawn();
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

    s.write_all(
        b"POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n",
    )
    .unwrap();
    s.write_all(b"@@@ PAS UNE LIGNE DE REQUETE @@@@").unwrap();
    s.flush().unwrap();

    let mut raw = String::new();
    let _ = s.read_to_string(&mut raw);

    assert_eq!(
        raw.matches("HTTP/1.1").count(),
        1,
        "UNE seule réponse pour la requête fautive (pas de 400 parasite en plus) : {raw:?}"
    );
    assert!(
        raw.contains("request body larger than Content-Length"),
        "le 400 doit nommer la cause exacte : {raw:?}"
    );
    assert!(
        raw.contains("Connection: close"),
        "une erreur de cadrage ferme la connexion : {raw:?}"
    );
}

// Le pipelining LÉGITIME (deux requêtes d'un bloc) doit rester servi.
#[test]
fn c2_pipelining_legitime_reste_servi() {
    let addr = spawn();
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let body1 = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    let body2 = r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#;
    let mut pipeline = String::new();
    for body in [body1, body2] {
        pipeline.push_str(&format!(
            "POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ));
    }
    s.write_all(pipeline.as_bytes()).unwrap();
    let mut raw = String::new();
    let _ = s.read_to_string(&mut raw);
    assert_eq!(
        raw.matches("HTTP/1.1 200 OK").count(),
        2,
        "le pipelining légitime doit rester servi : {raw:?}"
    );
    assert!(raw.contains("\"id\":1") && raw.contains("\"id\":2"));
}

// ---------------------------------------------------------------- M1
// Le timeout de lecture du socket ne doit plus fuir entre requêtes.
#[test]
fn m1_timeout_de_lecture_nest_plus_herite() {
    let addr = spawn_with_timeout(Duration::from_secs(2));
    let mut a = TcpStream::connect(addr).unwrap();
    a.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

    // read_timeout R = 2 s, budget de requête 2R = 4 s. Le resserrement
    // (`tighten_read_timeout`) n'agit qu'avant un `fill_buf` lancé quand il
    // reste moins de R au budget, et pose alors le timeout au budget restant.
    //
    // Requête 1 : tout le message sauf trois octets part tout de suite, puis
    // un octet à 1,5 s, un à S = 2,6 s et le dernier 100 ms plus tard (pour
    // qu'il n'arrive pas dans le même segment). Chaque attente du serveur
    // (1,5 s, 1,1 s, 0,1 s) reste sous le timeout par opération R ; le
    // `fill_buf` lancé à S, lui, démarre avec moins de R au budget et
    // resserre le timeout à T = 4 − 2,6 ≈ 1,4 s.
    //
    // Pourquoi cette forme : la gigue d'un runner (macOS : coalescence des
    // timers) ne fait qu'allonger S, donc baisser T ; S garde 1,3 s de marge
    // sous le budget de 4 s, chaque attente 0,5 s sous R. L'ancien dribble en
    // tranches régulières terminait au mieux avec T ≈ 1,6–2 s : un gap de
    // 1,5 s passait même timeout hérité — le test ne détectait plus la
    // régression (vérifié en supprimant la restauration dans
    // `handle_connection`), et la version encore antérieure (R = 400 ms)
    // dépassait le budget sur macOS (CI 2026-11-19, 2026-11-21, 2026-11-22).
    let corps = br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    let head = format!(
        "POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        corps.len()
    );
    let mut buf = head.into_bytes();
    buf.extend_from_slice(corps);
    let (debut, fin) = buf.split_at(buf.len() - 3);
    a.write_all(debut).unwrap();
    for (pause_ms, octet) in [(1500, 0), (1100, 1), (100, 2)] {
        std::thread::sleep(Duration::from_millis(pause_ms));
        a.write_all(&fin[octet..=octet]).unwrap();
    }
    let (st1, _) = read_response(&mut a);
    assert_eq!(st1, "200", "la requête 1 doit réussir");

    // Requête 2 : gap G = 1,7 s entre la ligne de requête et la fin des
    // en-têtes. Légal sous R = 2 s (0,3 s de marge, la gigue d'un `sleep` se
    // compte en millisecondes), illégal sous le T ≤ 1,4 s hérité : le test
    // ne passe que si le timeout est restauré entre les requêtes.
    let gap = Duration::from_millis(1700);
    a.write_all(b"GET /health HTTP/1.1\r\n").unwrap();
    std::thread::sleep(gap);
    a.write_all(b"Host: t\r\n\r\n").unwrap();
    let (st2, _) = read_response(&mut a);
    assert_eq!(
        st2, "200",
        "la requête 2 (gap légal de 1700 ms) doit être servie : le timeout de socket \
         ne doit pas être hérité de la requête précédente"
    );
}

// ---------------------------------------------------------------- M2
// Les doublons `Authorization` / `Cookie` / `X-Forwarded-For` doivent être
// refusés (400) : le verdict d'auth ne doit pas dépendre de l'ordre.
#[test]
fn m2_doublons_de_en_tetes_didentite_refuses() {
    let addr = spawn();
    for (nom, valeur1, valeur2) in [
        ("Authorization", "Bearer GOOD", "Bearer ATTACKER"),
        ("Cookie", "a=1", "b=2"),
        ("X-Forwarded-For", "1.1.1.1", "2.2.2.2"),
    ] {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let requete = format!(
            "GET /health HTTP/1.1\r\nHost: t\r\n{nom}: {valeur1}\r\n{nom}: {valeur2}\r\nConnection: close\r\n\r\n"
        );
        s.write_all(requete.as_bytes()).unwrap();
        let mut raw = String::new();
        let _ = s.read_to_string(&mut raw);
        assert!(
            raw.contains("HTTP/1.1 400 Bad Request"),
            "un doublon de `{nom}` doit être refusé en 400 : {raw:?}"
        );
    }
}

// Un en-tête d'identité NON dupliqué doit passer.
#[test]
fn m2_en_tete_didentite_unique_passe() {
    let addr = spawn();
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(
        b"GET /health HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer GOOD\r\nConnection: close\r\n\r\n",
    )
    .unwrap();
    let mut raw = String::new();
    let _ = s.read_to_string(&mut raw);
    assert!(
        raw.contains("HTTP/1.1 200 OK"),
        "un en-tête d'identité unique doit passer : {raw:?}"
    );
}

// ---------------------------------------------------------------- helpers

fn spawn() -> std::net::SocketAddr {
    spawn_with_timeout(Duration::from_secs(30))
}

fn spawn_with_timeout(rt: Duration) -> std::net::SocketAddr {
    let bound = litemcp::ServerBuilder::new("probe", "1.0")
        .legacy(true)
        .tool("ok", "ok", |_a| Ok(json!({})))
        .read_timeout(rt)
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = bound.local_addr().unwrap();
    std::thread::spawn(move || {
        let _ = bound.serve();
    });
    addr
}

fn read_response(s: &mut TcpStream) -> (String, String) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(idx) = text.find("\r\n\r\n") {
                    let head = &text[..idx];
                    let cl = head
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                        .and_then(|l| l.split(':').nth(1))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if buf.len() >= idx + 4 + cl {
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&buf).to_string();
    let status = text
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .nth(1)
        .unwrap_or("AUCUNE")
        .to_string();
    (status, text)
}

// ---------------------------------------------------------------- m3
// Une notification inconnue (ni action, ni `notifications/*`) doit être
// REFUSÉE explicitement — pas acceptée tacitement comme notification de
// protocole (la distinction documentée dans `protocol.rs` était inapplyée).
#[test]
fn m3_notification_inconnue_refusee_explicitement() {
    let core = litemcp::ServerBuilder::new("probe", "1.0")
        .tool("ok", "ok", |_a| Ok(json!({})))
        .core();

    // `notifications/*` : acceptée et ignorée (202, aucune réponse).
    assert!(
        core.dispatch_unchecked(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .is_none(),
        "une notification de protocole doit rester sans réponse"
    );

    // Méthode inconnue en notification : refus explicite.
    let reply = core
        .dispatch_unchecked(br#"{"jsonrpc":"2.0","method":"foo/bar"}"#)
        .expect("une notification inconnue doit produire une réponse d\'erreur");
    let v: serde_json::Value = serde_json::from_slice(&reply).unwrap();
    assert_eq!(
        v["error"]["code"], -32600,
        "notification inconnue → -32600 : {v}"
    );
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown notification"),
        "le message doit nommer la cause : {v}"
    );
}

// ---------------------------------------------------------------- m4
// La ligne de statut doit refléter la version HTTP de la requête.
#[test]
fn m4_ligne_de_statut_reflete_la_version_http() {
    let addr = spawn();

    // HTTP/1.1 → HTTP/1.1
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(b"GET /health HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut raw = String::new();
    let _ = s.read_to_string(&mut raw);
    assert!(raw.starts_with("HTTP/1.1 200 OK"), "{raw:?}");

    // HTTP/1.0 → HTTP/1.0 (un client HTTP/1.0 strict refuse HTTP/1.1)
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(b"GET /health HTTP/1.0\r\nHost: t\r\n\r\n")
        .unwrap();
    let mut raw = String::new();
    let _ = s.read_to_string(&mut raw);
    assert!(raw.starts_with("HTTP/1.0 200 OK"), "{raw:?}");
}

// ---------------------------------------------------------------- m2
// Le 413 doit être LIVRÉ au client, même quand le corps annoncé dépasse
// le budget de drain (le RST purgait la réponse).
#[test]
fn m2_le_413_est_livre_meme_avec_un_gros_corps() {
    let addr = spawn();
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    // Annonce 1 Mio + 1 octet (MAX_BODY = 1 Mio, donc strictement au-delà)
    // et n'envoie que l'en-tête : le serveur doit répondre 413 sans attendre
    // le corps.
    s.write_all(
        b"POST /mcp HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: 1048577\r\n\r\n",
    )
    .unwrap();
    let mut raw = String::new();
    let _ = s.read_to_string(&mut raw);
    assert!(
        raw.contains("HTTP/1.1 413 Payload Too Large"),
        "le 413 doit être livré : {raw:?}"
    );
}

// ---------------------------------------------------------------- m1
// Le plafond de réponse doit être configurable.
#[test]
fn m1_plafond_de_reponse_configurable() {
    let core = litemcp::ServerBuilder::new("probe", "1.0")
        .tool("gros", "gros", |_a| Ok(json!({ "data": "x".repeat(5000) })))
        .max_response_bytes(1000)
        .core();
    let reply = core
        .dispatch_unchecked(
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"gros","arguments":{}}}"#,
        )
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&reply).unwrap();
    assert_eq!(
        v["error"]["code"], -32603,
        "une réponse au-delà du plafond doit être remplacée par une erreur : {v}"
    );
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("exceeds maximum"),
        "le message doit nommer le plafond : {v}"
    );
}
