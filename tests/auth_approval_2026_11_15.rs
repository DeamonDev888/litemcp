//! L1 — confirmation externe (C5) : accordée, refusée, en attente.
//!
//! Ce qui est **approuvé** est exactement ce qui s'exécute : les octets des
//! arguments capturés par la politique et ceux reçus par le handler sont
//! identiques. Rien n'est mis en file, rien n'est conservé.

use litemcp::auth::{
    ApprovalPolicy, ApprovalRequest, ApprovalVerdict, AuthContext, ProofMethod, TestVerifier,
};
use litemcp::server::{HttpStatus, ServerBuilder};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params })
        .to_string()
        .into_bytes()
}

fn call(name: &str, arguments: Value) -> Vec<u8> {
    rpc(
        "tools/call",
        json!({ "name": name, "arguments": arguments }),
    )
}

fn contexte(rights: &[&str]) -> AuthContext {
    AuthContext::new("client-demo", ProofMethod::Application).with_rights(rights.iter().copied())
}

/// Politique pilotée par le test : verdict fixé, demandes capturées.
struct Politique {
    verdict: Mutex<ApprovalVerdict>,
    demandes: Mutex<Vec<(String, String, Vec<u8>)>>,
    appels: AtomicUsize,
}

impl Politique {
    fn new(verdict: ApprovalVerdict) -> Self {
        Self {
            verdict: Mutex::new(verdict),
            demandes: Mutex::new(Vec::new()),
            appels: AtomicUsize::new(0),
        }
    }

    fn appels(&self) -> usize {
        self.appels.load(Ordering::Relaxed)
    }
}

impl ApprovalPolicy for Politique {
    fn decide(&self, request: &ApprovalRequest<'_>) -> ApprovalVerdict {
        self.appels.fetch_add(1, Ordering::Relaxed);
        self.demandes.lock().expect("verrou").push((
            request.client.to_string(),
            request.tool.to_string(),
            serde_json::to_vec(request.arguments).expect("arguments sérialisables"),
        ));
        self.verdict.lock().expect("verrou").clone()
    }
}

/// Un serveur dont l'outil `sensible` est soumis à confirmation, avec le
/// compteur d'exécution et les octets exacts reçus par le handler.
fn serveur(
    politique: Arc<Politique>,
    compteur: Arc<AtomicUsize>,
    vus: Arc<Mutex<Vec<u8>>>,
) -> litemcp::server::ServerCore {
    let compte_libre = Arc::clone(&compteur);
    ServerBuilder::new("approbation", "1.0.0")
        .tool("sensible", "soumis à confirmation", move |arguments| {
            compteur.fetch_add(1, Ordering::Relaxed);
            *vus.lock().expect("verrou") = serde_json::to_vec(arguments).expect("sérialisable");
            Ok(json!({ "ok": true }))
        })
        .tool("libre", "sans confirmation", move |_args| {
            compte_libre.fetch_add(1, Ordering::Relaxed);
            Ok(json!({ "ok": true }))
        })
        .require_right("sensible", "tool.sensible")
        .require_right("libre", "tool.libre")
        .require_approval("sensible")
        .approval_policy(politique)
        .verifier(Arc::new(TestVerifier::accepting(contexte(&[
            "tool.sensible",
            "tool.libre",
        ]))))
        .core()
}

fn dispatch(core: &litemcp::server::ServerCore, body: &[u8]) -> (Value, HttpStatus) {
    let context = contexte(&["tool.sensible", "tool.libre"]);
    let response = core
        .dispatch_authenticated(body, None, &context)
        .expect("réponse");
    (
        serde_json::from_slice(&response.body).expect("json"),
        response.status,
    )
}

#[test]
fn approuve_le_handler_s_execute_avec_exactement_les_arguments_soumis() {
    let politique = Arc::new(Politique::new(ApprovalVerdict::Approved));
    let compteur = Arc::new(AtomicUsize::new(0));
    let vus = Arc::new(Mutex::new(Vec::new()));
    let core = serveur(
        Arc::clone(&politique),
        Arc::clone(&compteur),
        Arc::clone(&vus),
    );
    let arguments = json!({ "sql": "select 1", "n": 3, "flag": true });
    let (value, status) = dispatch(&core, &call("sensible", arguments.clone()));
    assert!(value.get("result").is_some(), "{value}");
    assert_eq!(status, HttpStatus::Ok);
    assert_eq!(compteur.load(Ordering::Relaxed), 1);
    assert_eq!(politique.appels(), 1);

    // Ce que la politique a vu et ce que le handler a reçu : mêmes octets.
    let demandes = politique.demandes.lock().expect("verrou");
    let (client, tool, octets_soumis) = &demandes[0];
    assert_eq!(client, "client-demo");
    assert_eq!(tool, "sensible");
    assert_eq!(
        *octets_soumis,
        vus.lock().expect("verrou").clone(),
        "les arguments exécutés doivent être exactement ceux soumis"
    );
    assert_eq!(
        serde_json::from_slice::<Value>(octets_soumis).expect("json"),
        arguments
    );
}

#[test]
fn refuse_rend_403_sans_executer_ni_conserver_d_etat() {
    let politique = Arc::new(Politique::new(ApprovalVerdict::Denied));
    let compteur = Arc::new(AtomicUsize::new(0));
    let core = serveur(
        Arc::clone(&politique),
        Arc::clone(&compteur),
        Arc::new(Mutex::new(Vec::new())),
    );
    let (value, status) = dispatch(&core, &call("sensible", json!({ "sql": "drop" })));
    assert_eq!(value["error"]["code"], -32023, "{value}");
    assert_eq!(
        value["error"]["data"]["reason"], "approval_denied",
        "{value}"
    );
    assert!(value["error"]["data"].get("retryable").is_none(), "{value}");
    assert_eq!(status, HttpStatus::Forbidden);
    assert_eq!(
        compteur.load(Ordering::Relaxed),
        0,
        "un refus n'exécute jamais le handler"
    );
    assert!(
        value.get("result").is_none(),
        "aucun contenu produit : {value}"
    );
}

#[test]
fn en_attente_rend_403_retryable_avec_le_delai_fourni() {
    let politique = Arc::new(Politique::new(ApprovalVerdict::Pending {
        retry_after: Some(Duration::from_millis(1500)),
    }));
    let compteur = Arc::new(AtomicUsize::new(0));
    let core = serveur(
        Arc::clone(&politique),
        Arc::clone(&compteur),
        Arc::new(Mutex::new(Vec::new())),
    );
    let (value, status) = dispatch(&core, &call("sensible", json!({})));
    assert_eq!(value["error"]["code"], -32023, "{value}");
    assert_eq!(
        value["error"]["data"]["reason"], "approval_pending",
        "{value}"
    );
    assert_eq!(value["error"]["data"]["retryable"], true, "{value}");
    assert_eq!(value["error"]["data"]["retryAfterMs"], 1500, "{value}");
    assert_eq!(status, HttpStatus::Forbidden);
    assert_eq!(compteur.load(Ordering::Relaxed), 0);

    // Rien n'est conservé : le même appel est re-soumis à la politique.
    let _ = dispatch(&core, &call("sensible", json!({})));
    assert_eq!(politique.appels(), 2);
}

#[test]
fn en_attente_sans_delai_ne_fixe_pas_de_retry_after() {
    let politique = Arc::new(Politique::new(ApprovalVerdict::Pending {
        retry_after: None,
    }));
    let core = serveur(
        politique,
        Arc::new(AtomicUsize::new(0)),
        Arc::new(Mutex::new(Vec::new())),
    );
    let (value, _status) = dispatch(&core, &call("sensible", json!({})));
    assert_eq!(value["error"]["data"]["retryable"], true, "{value}");
    assert!(
        value["error"]["data"].get("retryAfterMs").is_none(),
        "{value}"
    );
}

#[test]
fn la_politique_n_est_pas_consultee_sans_droit_ni_sans_marquage() {
    let politique = Arc::new(Politique::new(ApprovalVerdict::Approved));
    let compteur = Arc::new(AtomicUsize::new(0));
    let core = serveur(
        Arc::clone(&politique),
        Arc::clone(&compteur),
        Arc::new(Mutex::new(Vec::new())),
    );

    // Outil marqué, mais appelant sans le droit : les droits passent avant.
    let sans_droit = contexte(&["tool.libre"]);
    let response = core
        .dispatch_authenticated(&call("sensible", json!({})), None, &sans_droit)
        .expect("réponse");
    let value: Value = serde_json::from_slice(&response.body).expect("json");
    assert_eq!(value["error"]["data"]["reason"], "right_missing", "{value}");
    assert_eq!(
        politique.appels(),
        0,
        "la politique ne voit pas un refus de droit"
    );

    // Outil non marqué : la politique n'est pas interrogée non plus.
    let (value, _status) = dispatch(&core, &call("libre", json!({})));
    assert!(value.get("result").is_some(), "{value}");
    assert_eq!(politique.appels(), 0);
    assert_eq!(compteur.load(Ordering::Relaxed), 1);
}

#[test]
fn une_politique_qui_panique_est_un_refus_et_le_service_continue() {
    struct Panique;
    impl ApprovalPolicy for Panique {
        fn decide(&self, _request: &ApprovalRequest<'_>) -> ApprovalVerdict {
            panic!("bug de la politique");
        }
    }
    let compteur = Arc::new(AtomicUsize::new(0));
    let motifs = Arc::new(Mutex::new(Vec::new()));
    let journal = Arc::clone(&motifs);
    let compte = Arc::clone(&compteur);
    let core = ServerBuilder::new("panique", "1.0.0")
        .tool("sensible", "d", move |_args| {
            compte.fetch_add(1, Ordering::Relaxed);
            Ok(json!({}))
        })
        .require_right("sensible", "tool.sensible")
        .require_approval("sensible")
        .approval_policy(Arc::new(Panique))
        .verifier(Arc::new(TestVerifier::accepting(contexte(&[
            "tool.sensible",
        ]))))
        .on_auth_decision(move |event| {
            journal
                .lock()
                .expect("verrou")
                .push((event.verdict, event.reason.map(str::to_string)));
        })
        .core();

    let (value, status) = dispatch(&core, &call("sensible", json!({})));
    assert_eq!(value["error"]["code"], -32023, "{value}");
    assert_eq!(
        value["error"]["data"]["reason"], "approval_denied",
        "{value}"
    );
    assert_eq!(status, HttpStatus::Forbidden);
    assert_eq!(compteur.load(Ordering::Relaxed), 0);

    // Le motif précis reste côté application.
    let motifs = motifs.lock().expect("verrou");
    assert!(
        motifs.iter().any(|(verdict, reason)| {
            *verdict == litemcp::auth::AuthVerdict::ApprovalDenied
                && reason.as_deref() == Some("approval_policy_panicked")
        }),
        "motif précis attendu : {motifs:?}"
    );
}

#[test]
fn require_approval_sans_politique_est_un_refus_ferme() {
    let compteur = Arc::new(AtomicUsize::new(0));
    let compte = Arc::clone(&compteur);
    let core = ServerBuilder::new("sans-politique", "1.0.0")
        .tool("sensible", "d", move |_args| {
            compte.fetch_add(1, Ordering::Relaxed);
            Ok(json!({}))
        })
        .require_right("sensible", "tool.sensible")
        .require_approval("sensible")
        .verifier(Arc::new(TestVerifier::accepting(contexte(&[
            "tool.sensible",
        ]))))
        .core();
    let (value, status) = dispatch(&core, &call("sensible", json!({})));
    assert_eq!(value["error"]["code"], -32023, "{value}");
    assert_eq!(
        value["error"]["data"]["reason"], "approval_denied",
        "{value}"
    );
    assert_eq!(status, HttpStatus::Forbidden);
    assert_eq!(compteur.load(Ordering::Relaxed), 0);
}

#[test]
fn sans_verificateur_l_approbation_est_inerte() {
    let politique = Arc::new(Politique::new(ApprovalVerdict::Denied));
    let compteur = Arc::new(AtomicUsize::new(0));
    let compte = Arc::clone(&compteur);
    // Autorisation inactive : ni droits, ni confirmation — comportement
    // historique, y compris pour un outil marqué.
    let core = ServerBuilder::new("inerte", "1.0.0")
        .tool("sensible", "d", move |_args| {
            compte.fetch_add(1, Ordering::Relaxed);
            Ok(json!({ "ok": true }))
        })
        .require_approval("sensible")
        .approval_policy(Arc::clone(&politique) as Arc<dyn ApprovalPolicy>)
        .core();
    let response = core
        .dispatch(&call("sensible", json!({})))
        .expect("réponse");
    let value: Value = serde_json::from_slice(&response).expect("json");
    assert!(value.get("result").is_some(), "{value}");
    assert_eq!(compteur.load(Ordering::Relaxed), 1);
    assert_eq!(politique.appels(), 0, "aucune politique sans vérificateur");
}
