//! L1 — vérificateur, identité et échec fermé (C1, C4) : contexte absent,
//! paniques contenues, héritage du dispatch imbriqué, snapshot échangé,
//! journal d'audit.

use litemcp::Gateway;
use litemcp::auth::{self, AuthContext, AuthVerdict, ProofMethod, TestVerifier, VerifiedRequest};
use litemcp::jsonrpc::ErrorObject;
use litemcp::server::{HttpStatus, ServerBuilder, ServerCore};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const VERSION: &str = "2026-07-28";

fn rpc(method: &str, params: Value) -> Vec<u8> {
    let mut params = params.as_object().cloned().unwrap_or_default();
    params.insert(
        "_meta".to_string(),
        json!({
            "io.modelcontextprotocol/protocolVersion": VERSION,
            "io.modelcontextprotocol/clientCapabilities": {}
        }),
    );
    json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })
        .to_string()
        .into_bytes()
}

fn listing(method: &str) -> Vec<u8> {
    rpc(method, json!({}))
}

fn call(name: &str) -> Vec<u8> {
    rpc("tools/call", json!({ "name": name, "arguments": {} }))
}

fn contexte(rights: &[&str]) -> AuthContext {
    AuthContext::new("client-demo", ProofMethod::Application).with_rights(rights.iter().copied())
}

fn core_avec(compteur: Arc<AtomicUsize>, verifier: bool) -> ServerCore {
    let compte = Arc::clone(&compteur);
    let builder = ServerBuilder::new("verif", "1.0.0")
        .tool("outil", "d", move |_args| {
            compte.fetch_add(1, Ordering::Relaxed);
            Ok(json!({ "ok": true }))
        })
        .resource("memo://etat", "état", "d", || {
            Ok(litemcp::protocol::ResourceContents {
                uri: "memo://etat".to_string(),
                mime_type: None,
                text: "{}".to_string(),
            })
        })
        .prompt("salut", "d", |_args| {
            Ok(vec![litemcp::protocol::PromptMessage::user("bonjour")])
        })
        .require_right("outil", "tool.read")
        .require_resource_right("memo://etat", "memo.read")
        .require_prompt_right("salut", "prompt.read");
    if verifier {
        builder
            .verifier(Arc::new(TestVerifier::accepting(contexte(&[
                "tool.read",
                "memo.read",
                "prompt.read",
            ]))))
            .core()
    } else {
        builder.core()
    }
}

#[test]
fn sans_identite_l_autorisation_active_refuse_tout_avant_tout_handler() {
    let compteur = Arc::new(AtomicUsize::new(0));
    let core = core_avec(Arc::clone(&compteur), true);
    let corps = [
        listing("tools/list"),
        listing("resources/list"),
        listing("resources/templates/list"),
        listing("prompts/list"),
        call("outil"),
        rpc("resources/read", json!({ "uri": "memo://etat" })),
        rpc("prompts/get", json!({ "name": "salut", "arguments": {} })),
    ];
    for body in corps {
        let response = core.dispatch_request(&body, None).expect("réponse due");
        let value: Value = serde_json::from_slice(&response.body).expect("json");
        assert_eq!(value["error"]["code"], -32023, "{value}");
        assert_eq!(
            value["error"]["data"]["reason"], "unauthenticated",
            "{value}"
        );
        assert_eq!(response.status, HttpStatus::Forbidden, "{value}");
        // Aucun détail interne : `data` ne porte que le motif.
        let data = value["error"]["data"].as_object().expect("objet data");
        assert_eq!(data.len(), 1, "data ne porte que `reason` : {value}");
    }
    assert_eq!(
        compteur.load(Ordering::Relaxed),
        0,
        "aucun handler ne tourne sans identité"
    );

    // `dispatch_unchecked` n'est pas un contournement non plus.
    let response = core
        .dispatch_unchecked(br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
        .expect("réponse due");
    let value: Value = serde_json::from_slice(&response).expect("json");
    assert_eq!(value["error"]["code"], -32023, "{value}");
    assert_eq!(compteur.load(Ordering::Relaxed), 0);
}

#[test]
fn une_notification_sans_identite_ne_recoit_aucune_reponse_et_n_execute_rien() {
    let compteur = Arc::new(AtomicUsize::new(0));
    let core = core_avec(Arc::clone(&compteur), true);
    let notification = br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    assert!(
        core.dispatch_request(notification, None).is_none(),
        "une notification n'a pas de réponse"
    );
    assert_eq!(compteur.load(Ordering::Relaxed), 0);
}

#[test]
fn sans_verificateur_aucun_refus_et_aucune_identite_requise() {
    let compteur = Arc::new(AtomicUsize::new(0));
    let core = core_avec(Arc::clone(&compteur), false);
    assert!(!core.authorization_active());
    let response = core.dispatch(&call("outil")).expect("réponse");
    let value: Value = serde_json::from_slice(&response).expect("json");
    assert!(value.get("result").is_some(), "{value}");
    assert_eq!(compteur.load(Ordering::Relaxed), 1);
    // Le catalogue complet est servi, comme avant l'autorisation.
    let response = core.dispatch(&listing("tools/list")).expect("réponse");
    let value: Value = serde_json::from_slice(&response).expect("json");
    assert_eq!(value["result"]["tools"][0]["name"], "outil", "{value}");
}

#[test]
fn le_verificateur_n_est_appele_que_par_authenticate_et_sa_panique_est_contenue() {
    let verificateur = Arc::new(TestVerifier::new(|_request: &VerifiedRequest<'_>| {
        panic!("bug du vérificateur");
    }));
    let core = ServerBuilder::new("panique", "1.0.0")
        .verifier(Arc::clone(&verificateur) as Arc<dyn auth::Verifier>)
        .core();
    let requete = VerifiedRequest::new("POST", "/mcp", "127.0.0.1:23080", &[], b"{}");
    let refus = core
        .authenticate(&requete)
        .expect("autorisation active")
        .expect_err("une panique est un refus");
    assert_eq!(refus.reason(), "verifier_panicked");
    assert_eq!(verificateur.calls(), 1);

    // Le dispatch avec identité ne consulte pas le vérificateur : l'identité
    // vient du transport, jamais du corps JSON-RPC.
    let _ = core.dispatch_authenticated(&listing("tools/list"), None, &contexte(&[]));
    assert_eq!(verificateur.calls(), 1);

    // Et sans vérificateur configuré, `authenticate` ne rend rien.
    let core = core_avec(Arc::new(AtomicUsize::new(0)), false);
    assert!(core.authenticate(&requete).is_none());
}

#[test]
fn un_verificateur_de_test_accepte_et_compte() {
    let verificateur = TestVerifier::accepting(contexte(&["tool.read"]));
    let core = ServerBuilder::new("ok", "1.0.0")
        .verifier(Arc::new(verificateur))
        .core();
    let entetes: [(&str, &str); 1] = [("x-client", "client-demo")];
    let requete = VerifiedRequest::new("POST", "/mcp", "127.0.0.1:23080", &entetes, b"{}");
    let context = core
        .authenticate(&requete)
        .expect("actif")
        .expect("accepté");
    assert_eq!(context.client(), "client-demo");
    assert!(context.has_right("tool.read"));
}

#[test]
fn le_handler_et_le_hook_on_call_lisent_l_identite_du_fil() {
    let vus_handler = Arc::new(Mutex::new(None));
    let vus_hook = Arc::new(Mutex::new(None));
    let (handler, hook) = (Arc::clone(&vus_handler), Arc::clone(&vus_hook));
    let core = ServerBuilder::new("fil", "1.0.0")
        .tool("outil", "d", move |_args| {
            *handler.lock().expect("verrou") =
                auth::current().map(|context| context.client().to_string());
            Ok(json!({}))
        })
        .require_right("outil", "tool.read")
        .verifier(Arc::new(TestVerifier::accepting(contexte(&["tool.read"]))))
        .on_call(move |event| {
            if event.method == "tools/call" {
                *hook.lock().expect("verrou") =
                    auth::current().map(|context| context.client().to_string());
            }
        })
        .core();
    assert!(
        auth::current().is_none(),
        "hors dispatch, aucune identité n'est liée"
    );
    let _ = core.dispatch_authenticated(&call("outil"), None, &contexte(&["tool.read"]));
    assert_eq!(
        vus_handler.lock().expect("verrou").as_deref(),
        Some("client-demo"),
        "le handler lit l'identité (y compris dans le fil du timeout)"
    );
    assert_eq!(
        vus_hook.lock().expect("verrou").as_deref(),
        Some("client-demo"),
        "le hook on_call lit la même identité"
    );
    assert!(
        auth::current().is_none(),
        "le garde a délié l'identité au retour du dispatch"
    );
}

#[test]
fn un_dispatch_imbrique_herite_du_contexte_du_fil() {
    let gateway = Arc::new(Gateway::new(
        ServerBuilder::new("gw", "1.0.0")
            .tool("inner", "interne", |_args| Ok(json!({ "inner": true })))
            .require_right("inner", "inner.read")
            .verifier(Arc::new(TestVerifier::accepting(contexte(&["inner.read"])))),
    ));
    let imbrique = Arc::clone(&gateway);
    gateway.edit(move |builder| {
        builder
            .tool("outer", "appelle inner", move |_args| {
                let reply = imbrique
                    .dispatch(&call("inner"))
                    .expect("réponse imbriquée");
                Ok(json!({
                    "imbrique": serde_json::from_slice::<Value>(&reply).unwrap_or(Value::Null)
                }))
            })
            .require_right("outer", "outer.read")
    });

    let context = contexte(&["outer.read", "inner.read"]);
    let response = gateway
        .dispatch_authenticated(&call("outer"), None, &context)
        .expect("réponse");
    let value: Value = serde_json::from_slice(&response.body).expect("json");
    let imbrique = &value["result"]["structuredContent"]["imbrique"];
    assert!(
        imbrique.get("error").is_none(),
        "le dispatch imbriqué ne doit pas être refusé : {value}"
    );
    assert_eq!(
        imbrique["result"]["structuredContent"]["inner"], true,
        "le dispatch imbriqué hérite des droits : {value}"
    );
}

#[test]
fn un_dispatch_imbrique_sans_heritage_serait_refuse() {
    // Même montage, mais l'identité extérieure ne détient PAS le droit de
    // l'outil imbriqué : le dispatch imbriqué hérite donc d'un refus.
    let gateway = Arc::new(Gateway::new(
        ServerBuilder::new("gw", "1.0.0")
            .tool("inner", "interne", |_args| Ok(json!({ "inner": true })))
            .require_right("inner", "inner.read")
            .verifier(Arc::new(TestVerifier::accepting(contexte(&["inner.read"])))),
    ));
    let imbrique = Arc::clone(&gateway);
    gateway.edit(move |builder| {
        builder
            .tool("outer", "appelle inner", move |_args| {
                let reply = imbrique
                    .dispatch(&call("inner"))
                    .expect("réponse imbriquée");
                Ok(json!({
                    "imbrique": serde_json::from_slice::<Value>(&reply).unwrap_or(Value::Null)
                }))
            })
            .require_right("outer", "outer.read")
    });

    let context = contexte(&["outer.read"]);
    let response = gateway
        .dispatch_authenticated(&call("outer"), None, &context)
        .expect("réponse");
    let value: Value = serde_json::from_slice(&response.body).expect("json");
    let imbrique = &value["result"]["structuredContent"]["imbrique"];
    assert_eq!(
        imbrique["error"]["code"], -32023,
        "le contexte hérité ne détient pas inner.read : {value}"
    );
    assert_eq!(
        imbrique["error"]["data"]["reason"], "right_missing",
        "{value}"
    );
}

#[test]
fn un_snapshot_echange_pendant_l_appel_ne_vaut_que_pour_le_dispatch_suivant() {
    let gateway = Arc::new(Gateway::new(
        ServerBuilder::new("gw", "1.0.0")
            .tool("inner", "interne", |_args| Ok(json!({ "inner": true })))
            .require_right("inner", "inner.read")
            .verifier(Arc::new(TestVerifier::accepting(contexte(&["inner.read"])))),
    ));
    let imbrique = Arc::clone(&gateway);
    gateway.edit(move |builder| {
        builder
            .tool("outer", "échange le catalogue", move |_args| {
                let avant = imbrique.dispatch(&call("inner")).expect("réponse");
                // L'appel en cours garde son snapshot ; le suivant voit le
                // nouveau catalogue.
                imbrique.edit(|mut builder| {
                    builder.remove_tool("inner");
                    builder
                });
                let apres = imbrique.dispatch(&call("inner")).expect("réponse");
                Ok(json!({
                    "avant": serde_json::from_slice::<Value>(&avant).unwrap_or(Value::Null),
                    "apres": serde_json::from_slice::<Value>(&apres).unwrap_or(Value::Null),
                }))
            })
            .require_right("outer", "outer.read")
    });

    let context = contexte(&["outer.read", "inner.read"]);
    let response = gateway
        .dispatch_authenticated(&call("outer"), None, &context)
        .expect("réponse");
    let value: Value = serde_json::from_slice(&response.body).expect("json");
    let observations = &value["result"]["structuredContent"];
    assert_eq!(
        observations["avant"]["result"]["structuredContent"]["inner"], true,
        "le premier dispatch imbriqué voit inner : {value}"
    );
    assert_eq!(
        observations["apres"]["error"]["code"], -32602,
        "le second dispatch voit le nouveau snapshot : {value}"
    );
}

#[test]
fn une_panique_de_hook_d_audit_est_contencee_et_les_hooks_sont_cumulables() {
    let journal = Arc::new(Mutex::new(Vec::new()));
    let trace = Arc::clone(&journal);
    let core = ServerBuilder::new("hooks", "1.0.0")
        .tool("outil", "d", |_args| Ok(json!({ "ok": true })))
        .require_right("outil", "tool.read")
        .verifier(Arc::new(TestVerifier::accepting(contexte(&["tool.read"]))))
        .on_auth_decision(|_event| panic!("hook défaillant"))
        .on_auth_decision(move |event| {
            trace.lock().expect("verrou").push((
                event.method.to_string(),
                event.name.map(str::to_string),
                event.client.map(str::to_string),
                event.verdict,
                event.reason.map(str::to_string),
            ));
        })
        .core();

    let response = core
        .dispatch_authenticated(&call("outil"), None, &contexte(&["tool.read"]))
        .expect("réponse");
    let value: Value = serde_json::from_slice(&response.body).expect("json");
    assert!(
        value.get("result").is_some(),
        "le service continue malgré le hook : {value}"
    );

    let journal = journal.lock().expect("verrou");
    assert_eq!(journal.len(), 1, "{journal:?}");
    let (method, name, client, verdict, reason) = &journal[0];
    assert_eq!(method, "tools/call");
    assert_eq!(name.as_deref(), Some("outil"));
    assert_eq!(client.as_deref(), Some("client-demo"));
    assert_eq!(*verdict, AuthVerdict::Allowed);
    assert_eq!(*reason, None);
}

#[test]
fn le_motif_precis_reste_cote_application_et_jamais_cote_reseau() {
    let journal = Arc::new(Mutex::new(Vec::new()));
    let trace = Arc::clone(&journal);
    let core = ServerBuilder::new("motif", "1.0.0")
        .tool("outil", "d", |_args| Ok(json!({})))
        .require_right("outil", "droit.tres.secret")
        .verifier(Arc::new(TestVerifier::accepting(contexte(&[]))))
        .on_auth_decision(move |event| {
            trace.lock().expect("verrou").push((
                event.verdict,
                event.reason.map(str::to_string),
                event.client.map(str::to_string),
            ));
        })
        .core();

    let response = core
        .dispatch_authenticated(&call("outil"), None, &contexte(&[]))
        .expect("réponse");
    let texte = String::from_utf8(response.body.clone()).expect("utf8");
    let value: Value = serde_json::from_slice(&response.body).expect("json");
    assert_eq!(value["error"]["code"], -32023, "{value}");
    assert_eq!(value["error"]["data"]["reason"], "right_missing", "{value}");
    assert!(
        !texte.contains("droit.tres.secret"),
        "le réseau ne connaît pas le droit exigé : {texte}"
    );
    assert!(
        !value["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("outil"),
        "le message reste générique : {value}"
    );

    let journal = journal.lock().expect("verrou");
    let (verdict, reason, client) = &journal[0];
    assert_eq!(*verdict, AuthVerdict::RightMissing);
    assert_eq!(reason.as_deref(), Some("right_missing"));
    assert_eq!(client.as_deref(), Some("client-demo"));
}

#[test]
fn les_codes_et_statuts_d_autorisation_sont_ceux_de_la_specification() {
    assert_eq!(ErrorObject::FORBIDDEN, -32023);
    assert_eq!(
        HttpStatus::from_error_code(ErrorObject::FORBIDDEN),
        HttpStatus::Forbidden
    );
    assert_eq!(HttpStatus::Forbidden.code(), 403);
    assert_eq!(HttpStatus::Forbidden.as_str(), "403 Forbidden");
    // Les codes existants gardent leur statut.
    assert_eq!(
        HttpStatus::from_error_code(ErrorObject::INVALID_PARAMS),
        HttpStatus::Ok
    );
    assert_eq!(
        HttpStatus::from_error_code(ErrorObject::METHOD_NOT_FOUND),
        HttpStatus::NotFound
    );
}

#[test]
fn le_journal_audit_voit_aussi_les_refus_sans_identite() {
    let journal = Arc::new(Mutex::new(Vec::new()));
    let trace = Arc::clone(&journal);
    let core = ServerBuilder::new("audit", "1.0.0")
        .tool("outil", "d", |_args| Ok(json!({})))
        .require_right("outil", "tool.read")
        .verifier(Arc::new(TestVerifier::accepting(contexte(&["tool.read"]))))
        .on_auth_decision(move |event| {
            trace.lock().expect("verrou").push((
                event.method.to_string(),
                event.name.map(str::to_string),
                event.client.map(str::to_string),
                event.verdict,
                event.reason.map(str::to_string),
            ));
        })
        .core();

    let response = core
        .dispatch_request(&call("outil"), None)
        .expect("réponse due");
    assert_eq!(response.status, HttpStatus::Forbidden);
    let journal = journal.lock().expect("verrou");
    assert_eq!(journal.len(), 1, "{journal:?}");
    let (method, name, client, verdict, reason) = &journal[0];
    assert_eq!(method, "tools/call");
    assert_eq!(name.as_deref(), Some("outil"));
    assert_eq!(
        *client, None,
        "aucune identité : aucun client à journaliser"
    );
    assert_eq!(*verdict, AuthVerdict::Unauthenticated);
    assert_eq!(reason.as_deref(), Some("unauthenticated"));
}
