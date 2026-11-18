//! Un même noyau, un même verdict : `ServerCore::dispatch` applique le
//! profil configuré pour tout ce qui ne dépend pas des en-têtes HTTP.
//!
//! Avant correction, `dispatch` court-circuitait **tout** le profil strict
//! (il passait `strict = false` en dur) : le même corps nu rendait `200
//! tools/list` par `dispatch` et `400 -32602` par `dispatch_request`. Ces
//! tests figent l'invariant : pour un même corps, `dispatch` et
//! `dispatch_request(.., None)` rendent exactement la même réponse.

use litemcp::{HttpStatus, ServerBuilder, ServerCore};
use serde_json::Value;

fn core() -> ServerCore {
    ServerBuilder::new("verdict", "0.1.0")
        .tool("echo", "Écho", |args| Ok(args.clone()))
        .core()
}

fn legacy_core() -> ServerCore {
    ServerBuilder::new("verdict", "0.1.0")
        .legacy(true)
        .tool("echo", "Écho", |args| Ok(args.clone()))
        .core()
}

const META: &str = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}"#;

fn corps_avec_meta(id: &str, method: &str, extra: &str) -> Vec<u8> {
    format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{{{META}{extra}}}}}"#)
        .into_bytes()
}

fn corps_nu(id: &str, method: &str) -> Vec<u8> {
    format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{{}}}}"#).into_bytes()
}

fn json(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).expect("réponse JSON-RPC lisible")
}

/// Tous les corps partagent le même sort, qu'on passe par `dispatch` ou par
/// `dispatch_request` sans en-têtes : c'est le contrat d'un noyau unique.
///
/// L'équivalence seule serait tautologique (les deux entrées partagent le même
/// noyau) : chaque cas fixe donc aussi **les octets exacts** attendus sur le
/// fil, enveloppe JSON-RPC comprise — une régression de sérialisation (ordre
/// des clés, champ ajouté, `id` non écho-té) fait rougir ce test même quand les
/// deux entrées restent d'accord entre elles.
#[test]
fn dispatch_et_dispatch_request_sans_entetes_rendent_le_meme_corps() {
    let core = core();
    // (corps envoyé, réponse attendue octet pour octet ; `None` = notification)
    let cases: Vec<(Vec<u8>, Option<&str>)> = vec![
        (
            corps_nu("1", "tools/list"),
            Some(
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"missing required `_meta` object in params"}}"#,
            ),
        ),
        (
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}".to_vec(),
            Some(
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"params object is required and must contain `_meta`"}}"#,
            ),
        ),
        (
            corps_avec_meta("2", "tools/list", ""),
            Some(
                r#"{"jsonrpc":"2.0","id":2,"result":{"_meta":{"io.modelcontextprotocol/serverInfo":{"name":"verdict","version":"0.1.0"}},"cacheScope":"private","resultType":"complete","tools":[{"description":"Écho","inputSchema":{"type":"object"},"name":"echo"}],"ttlMs":60000}}"#,
            ),
        ),
        (
            b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/list\",\"params\":{\"_meta\":{\"io.modelcontextprotocol/protocolVersion\":\"1999-01-01\",\"io.modelcontextprotocol/clientCapabilities\":{}}}}".to_vec(),
            Some(
                r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32022,"message":"Unsupported protocol version: 1999-01-01","data":{"requested":"1999-01-01","supported":["2026-07-28"]}}}"#,
            ),
        ),
        (
            b"{\"jsonrpc\":\"2.0\",\"id\":null,\"method\":\"tools/list\",\"params\":{}}".to_vec(),
            Some(
                r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"request id must be a string or a number (null is not permitted in modern MCP 2026-07-28)"}}"#,
            ),
        ),
        (
            b"not json".to_vec(),
            Some(r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"invalid JSON"}}"#),
        ),
        (
            b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}".to_vec(),
            None,
        ),
    ];
    for (body, attendu) in cases {
        let envoye = String::from_utf8_lossy(&body).into_owned();
        let direct = core.dispatch(&body);
        let structure = core.dispatch_request(&body, None).map(|r| r.body);
        assert_eq!(direct, structure, "verdicts divergents pour {envoye}");
        match attendu {
            Some(attendu) => {
                let octets = direct.expect("une requête reçoit une réponse");
                assert_eq!(
                    String::from_utf8_lossy(&octets),
                    attendu,
                    "octets fil inattendus pour {envoye}"
                );
            }
            None => assert!(
                direct.is_none(),
                "une notification ne reçoit aucune réponse : {envoye}"
            ),
        }
    }
}

/// Un corps nu (sans `_meta`) est refusé par `dispatch` en profil strict —
/// exactement comme par `dispatch_request`.
#[test]
fn dispatch_refuse_un_corps_nu_en_profil_strict() {
    let core = core();
    let body = corps_nu("1", "tools/list");
    let reply = core
        .dispatch(&body)
        .expect("une requête reçoit une réponse");
    let v = json(&reply);
    assert_eq!(
        v["error"]["code"], -32602,
        "corps nu accepté par dispatch : {v}"
    );
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("missing required `_meta` object in params"),
        "{v}"
    );
}

/// La version de protocole et l'`id` nul sont vérifiés par `dispatch` aussi.
#[test]
fn dispatch_refuse_une_version_inconnue_et_un_id_nul() {
    let core = core();
    let inconnue = r#"{"jsonrpc":"2.0","id":4,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"1999-01-01","io.modelcontextprotocol/clientCapabilities":{}}}}"#;
    let v = json(&core.dispatch(inconnue.as_bytes()).expect("réponse"));
    assert_eq!(v["error"]["code"], -32022, "{v}");
    assert_eq!(v["error"]["data"]["requested"], "1999-01-01", "{v}");

    let id_nul = r#"{"jsonrpc":"2.0","id":null,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}"#;
    let v = json(&core.dispatch(id_nul.as_bytes()).expect("réponse"));
    assert_eq!(v["error"]["code"], -32600, "{v}");
}

/// Un corps strictement conforme passe sans en-têtes : `dispatch` est le
/// point d'entrée embarqué (pas de transport HTTP, donc pas d'en-têtes).
#[test]
fn dispatch_accepte_un_corps_strict_complet_sans_entetes() {
    let core = core();
    let v = json(
        &core
            .dispatch(&corps_avec_meta("5", "tools/list", ""))
            .expect("réponse"),
    );
    assert_eq!(v["result"]["resultType"], "complete", "{v}");
    assert_eq!(v["result"]["tools"][0]["name"], "echo", "{v}");
}

/// Le point d'entrée « sans contrôle » existe, mais sous un nom explicite.
#[test]
fn dispatch_unchecked_accepte_un_corps_nu() {
    let core = core();
    let v = json(
        &core
            .dispatch_unchecked(&corps_nu("1", "tools/list"))
            .expect("réponse"),
    );
    assert_eq!(v["result"]["tools"][0]["name"], "echo", "{v}");
}

/// Le profil legacy reste permissif : `dispatch` y accepte le corps nu.
#[test]
fn dispatch_en_profil_legacy_accepte_le_corps_nu() {
    let core = legacy_core();
    let v = json(
        &core
            .dispatch(&corps_nu("1", "tools/list"))
            .expect("réponse"),
    );
    assert_eq!(v["result"]["tools"][0]["name"], "echo", "{v}");
}

/// Le statut HTTP rendu par `dispatch_request` reste cohérent avec le corps :
/// refus de profil → 400, jamais 200.
#[test]
fn dispatch_request_sans_entetes_rend_400_en_profil_strict() {
    let core = core();
    let response = core
        .dispatch_request(&corps_nu("1", "tools/list"), None)
        .expect("une requête reçoit une réponse");
    assert_eq!(response.status, HttpStatus::BadRequest);
}
