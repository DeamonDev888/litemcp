//! Test de régression — audit 2026-11-25, B7 : la langue des messages
//! destinés aux clients émis par le gateway.
//!
//! `src/lib.rs` garantit que « tous les messages destinés aux clients
//! (validation, protocole, HTTP) sont en anglais ». La garde de récursion du
//! gateway (`recursion_limit_response`, `src/gateway.rs`) renvoyait son
//! `error.message` en français sur le fil. Ce test fixe le contrat sur le
//! texte **exact** émis au client : anglais, avec la profondeur maximale
//! (`MAX_DISPATCH_DEPTH` = 32) formatée dynamiquement dans le message.
//!
//! Montage : un outil `auto` re-dispatche sur son propre gateway
//! (`dispatch_unchecked`) ; le dispatch entrant au-delà de la borne est
//! refusé en `-32603` et son `error.message` remonte jusqu'à la réponse
//! externe via un drapeau compact (même patron que `gateway_e2e.rs` § 11 —
//! le handler ne ré-encapsule pas la réponse entière, sinon la taille
//! croîtrait exponentiellement à chaque niveau).

use litemcp::Gateway;
use litemcp::prelude::*;
use serde_json::{Value, json};
use std::sync::Arc;

/// Corps de l'auto-appel (id 2) : identique à chaque niveau de récursion.
const CORPS_AUTO: &[u8] =
    br#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"auto","arguments":{}}}"#;

/// Le message attendu sur le fil : en anglais, la borne 32 étant le rendu de
/// `{MAX_DISPATCH_DEPTH}`.
const MESSAGE_ATTENDU: &str = "gateway recursion limit exceeded (more than 32 nested dispatches on the same thread, across all gateways, from within a handler)";

#[test]
fn le_message_client_de_la_garde_de_recursion_est_en_anglais() {
    let gw = Arc::new(Gateway::new(ServerBuilder::new("gw-lang-b7", "1.0.0")));
    let interieur = Arc::clone(&gw);
    gw.edit(move |b| {
        let interieur = Arc::clone(&interieur);
        b.tool(
            "auto",
            "s'auto-dispatche sur son propre gateway",
            move |_a| {
                let reponse = interieur
                    .dispatch_unchecked(CORPS_AUTO)
                    .and_then(|octets| serde_json::from_slice::<Value>(&octets).ok());
                match reponse {
                    // L'appel imbriqué vient d'être REFUSÉ par la garde : c'est
                    // CE message client que le test audite — le remonter tel
                    // quel, avec son code.
                    Some(v) if v["error"]["code"] == json!(-32603) => Ok(json!({
                        "garde": true,
                        "code": v["error"]["code"].clone(),
                        "message": v["error"]["message"].clone(),
                    })),
                    // Le refus a déjà été remonté par le niveau du dessous :
                    // poursuivre la remontée sans ré-encapsuler.
                    Some(v) if v["result"]["structuredContent"].get("garde").is_some() => {
                        Ok(v["result"]["structuredContent"].clone())
                    }
                    // Enveloppe d'erreur d'un AUTRE code : drapeau quand même,
                    // pour que l'assertion échoue proprement au lieu de laisser
                    // la récursion filer jusqu'au débordement de pile.
                    Some(v) if v.get("error").is_some() => Ok(json!({
                        "garde": true,
                        "code": v["error"]["code"].clone(),
                        "message": v["error"]["message"].clone(),
                    })),
                    // La récursion continue d'un niveau.
                    _ => Ok(json!({ "suit": true })),
                }
            },
        )
    });

    let reply = gw
        .dispatch_unchecked(CORPS_AUTO)
        .expect("la requête externe doit recevoir une réponse (pas une notification)");
    let v: Value = serde_json::from_slice(&reply).expect("réponse JSON");
    let drapeau = &v["result"]["structuredContent"];

    assert_eq!(
        drapeau["garde"], true,
        "la garde a coupé l'auto-appel et le drapeau remonte : {v}"
    );
    assert_eq!(
        drapeau["code"],
        json!(-32603),
        "le refus porte le code interne `-32603`, et lui seul : {v}"
    );
    assert_eq!(
        drapeau["message"],
        json!(MESSAGE_ATTENDU),
        "le message client de la garde doit être en anglais, avec la \
         profondeur maximale formatée dynamiquement : {v}"
    );
}
