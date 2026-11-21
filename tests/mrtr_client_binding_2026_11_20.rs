//! MRTR — liaison du `requestState` à l'identité de l'appelant (constat n° 2
//! de `docs/AUDIT-LITEMCP-2026-11-20.md`).
//!
//! Sous autorisation, un jeton émis pour le client A ne doit pas être
//! rejouable par B, même quand B détient le même droit sur l'outil : sinon B
//! hériterait de la charge `state` (une décision propre à A) pendant le TTL.
//! Le rejeu de B est refusé en `-32602` **avant** le handler (compteur
//! inchangé) ; le rejeu de A reprend au tour suivant.
//!
//! Second cas : un jeton signé émis **sans** autorisation (donc sans `sub`)
//! reste cryptographiquement valide, mais il n'est lié à aucun client —
//! rejoué sur une instance qui exige l'autorisation, il est refusé.
//!
//! Couverture : les trois méthodes MRTR (`tools/call`, `resources/read`,
//! `prompts/get`) et les deux entrées du noyau — `dispatch_authenticated`
//! (contexte fourni par le transport) et `dispatch_verified` (contexte établi
//! par le vérificateur, donc liaison de bout en bout avec l'en-tête du client).

use litemcp::auth::{AuthContext, AuthError, ProofMethod, TestVerifier, VerifiedRequest};
use litemcp::protocol::{
    InputRequiredSpec, META_CLIENT_CAPABILITIES, META_PROTOCOL_VERSION, PROTOCOL_VERSION,
    PromptMessage, ResourceData,
};
use litemcp::server::{InputRound, PromptOutcome, ResourceOutcome, ServerCore, ToolOutcome};
use litemcp::{ServerBuilder, ToolError};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const OUTIL: &str = "confirmer";
const DROIT: &str = "confirmer.droit";
const CLE: &[u8] = b"0123456789abcdef0123456789abcdef";

/// Compteur d'entrées dans le handler : prouve qu'un refus n'y entre pas.
#[derive(Clone, Default)]
struct Compteur(Arc<AtomicUsize>);

impl Compteur {
    fn valeur(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }
}

fn contexte(client: &str) -> AuthContext {
    AuthContext::new(client, ProofMethod::Application).with_right(DROIT)
}

/// Outil à deux tours : le tour 1 élicite une confirmation et place dans
/// `state` une décision propre à l'appelant ; le tour 2 la restitue.
fn outil(compteur: Compteur) -> impl Fn(&Value, &InputRound) -> Result<ToolOutcome, ToolError> {
    move |_args, round| {
        compteur.0.fetch_add(1, Ordering::Relaxed);
        if round.turn == 1 {
            Ok(ToolOutcome::InputRequired(
                InputRequiredSpec::new()
                    .elicit(
                        "confirmer",
                        "Confirmer l'opération ?",
                        json!({ "type": "object" }),
                    )
                    .with_state(json!({ "decision": "propre-a-A" })),
            ))
        } else {
            Ok(ToolOutcome::Value(json!({
                "tour": round.turn,
                "etat": round.state,
            })))
        }
    }
}

fn serveur(compteur: Compteur, identite: Option<AuthContext>) -> ServerCore {
    let builder = ServerBuilder::new("mrtr-binding", "1.0.0")
        .tool_mrtr(OUTIL, "Confirmation liée à l'appelant", outil(compteur))
        .require_right(OUTIL, DROIT);
    let builder = match identite {
        Some(identite) => builder.verifier(Arc::new(TestVerifier::accepting(identite))),
        None => builder,
    };
    builder.core()
}

fn meta() -> Value {
    json!({
        (META_PROTOCOL_VERSION): PROTOCOL_VERSION,
        (META_CLIENT_CAPABILITIES): { "elicitation": {} },
    })
}

/// Tour 1 : `tools/call` nominal (arguments identiques au rejeu).
fn tour_un(id: i64) -> Vec<u8> {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": OUTIL,
            "arguments": { "cible": "ligne-42" },
            "_meta": meta(),
        },
    })
    .to_string()
    .into_bytes()
}

/// Rejeu : **id différent** (exigé par la spec), écho exact du jeton.
fn rejeu(id: i64, token: &str) -> Vec<u8> {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": OUTIL,
            "arguments": { "cible": "ligne-42" },
            "_meta": meta(),
            "inputResponses": {
                "confirmer": { "action": "accept", "content": { "ok": true } }
            },
            "requestState": token,
        },
    })
    .to_string()
    .into_bytes()
}

fn reponse(core: &ServerCore, body: &[u8], identite: &AuthContext) -> Value {
    let response = core
        .dispatch_authenticated(body, None, identite)
        .expect("une requête reçoit toujours une réponse");
    serde_json::from_slice(&response.body).expect("corps JSON")
}

#[test]
fn un_jeton_emis_pour_a_ne_sert_pas_a_b() {
    let compteur = Compteur::default();
    let a = contexte("client-a");
    let b = contexte("client-b");
    let core = serveur(compteur.clone(), Some(a.clone()));

    // Tour 1 : A obtient un `input_required` + un `requestState`.
    let r1 = reponse(&core, &tour_un(1), &a);
    assert_eq!(r1["result"]["resultType"], "input_required", "{r1}");
    assert_eq!(compteur.valeur(), 1, "le handler de A a tourné au tour 1");
    let token = r1["result"]["requestState"]
        .as_str()
        .expect("jeton attendu")
        .to_string();

    // B détient le même droit, mais le jeton n'a pas été émis pour lui.
    let rb = reponse(&core, &rejeu(2, &token), &b);
    assert_eq!(rb["error"]["code"], -32602, "{rb}");
    assert!(
        rb["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("another client")),
        "message de liaison à l'appelant attendu : {rb}"
    );
    assert_eq!(
        compteur.valeur(),
        1,
        "le rejeu de B n'atteint jamais le handler"
    );

    // A rejoue : tour 2, handler atteint, état propre à A restauré.
    let ra = reponse(&core, &rejeu(3, &token), &a);
    assert_eq!(ra["result"]["resultType"], "complete", "{ra}");
    assert_eq!(ra["result"]["structuredContent"]["tour"], 2);
    assert_eq!(
        ra["result"]["structuredContent"]["etat"]["decision"],
        "propre-a-A"
    );
    assert_eq!(compteur.valeur(), 2, "le rejeu de A atteint le handler");
}

#[test]
fn un_jeton_sans_identite_est_refuse_sous_autorisation() {
    // Émission sans vérificateur : le jeton signé ne porte pas de `sub`.
    let emission = Compteur::default();
    let sans_auth = ServerBuilder::new("mrtr-binding", "1.0.0")
        .request_state_key(CLE)
        .tool_mrtr(OUTIL, "Confirmation liée à l'appelant", outil(emission))
        .require_right(OUTIL, DROIT)
        .core();
    let r1 = sans_auth
        .dispatch_unchecked(&tour_un(1))
        .expect("une requête reçoit toujours une réponse");
    let r1: Value = serde_json::from_slice(&r1).expect("corps JSON");
    assert_eq!(r1["result"]["resultType"], "input_required", "{r1}");
    let token = r1["result"]["requestState"].as_str().unwrap().to_string();

    // Reprise sur une instance qui exige l'autorisation, même clé : le jeton
    // est valide (HMAC) mais n'est lié à aucun client → refus.
    let compteur = Compteur::default();
    let a = contexte("client-a");
    let avec_auth = ServerBuilder::new("mrtr-binding", "1.0.0")
        .request_state_key(CLE)
        .tool_mrtr(
            OUTIL,
            "Confirmation liée à l'appelant",
            outil(compteur.clone()),
        )
        .require_right(OUTIL, DROIT)
        .verifier(Arc::new(TestVerifier::accepting(a.clone())))
        .core();
    let r = reponse(&avec_auth, &rejeu(2, &token), &a);
    assert_eq!(r["error"]["code"], -32602, "{r}");
    assert_eq!(
        compteur.valeur(),
        0,
        "un jeton sans identité n'atteint jamais le handler"
    );
}

// ===========================================================================
// Couverture des deux entrées du noyau et des deux autres méthodes MRTR.
// ===========================================================================

const ENTETE_CLIENT: &str = "x-client";
const RESSOURCE: &str = "mem://confirmation";
const PROMPT: &str = "confirmation";

/// Vérificateur de test : l'identité vient de l'en-tête `x-client`, comme un
/// transport la tire de son authentification — jamais du corps ni d'un
/// contexte fourni à la main par le test.
fn verificateur() -> Arc<TestVerifier> {
    Arc::new(TestVerifier::new(|request: &VerifiedRequest<'_>| {
        request
            .header(ENTETE_CLIENT)
            .map(contexte)
            .ok_or_else(|| AuthError::new("client_header_missing"))
    }))
}

/// `dispatch_verified` : vérification **puis** dispatch, l'identité étant
/// celle que le vérificateur a établie à partir de `client`. Les en-têtes de
/// routage MCP sont dérivés du corps, comme le transport HTTP les pose (le
/// profil strict les exige et contrôle leur cohérence).
fn reponse_verifiee(core: &ServerCore, body: &[u8], client: &str) -> Value {
    let valeur: Value = serde_json::from_slice(body).expect("corps JSON");
    let methode = valeur["method"].as_str().expect("méthode");
    let nom = valeur["params"]["name"]
        .as_str()
        .or_else(|| valeur["params"]["uri"].as_str())
        .unwrap_or_default();
    let entetes = [
        (ENTETE_CLIENT, client),
        ("MCP-Protocol-Version", PROTOCOL_VERSION),
        ("Mcp-Method", methode),
        ("Mcp-Name", nom),
    ];
    let request = VerifiedRequest::new("POST", "/mcp", "127.0.0.1", &entetes, body);
    let reponse = core.dispatch_verified(&request);
    // `400` est le statut d'une erreur JSON-RPC (rejeu refusé) ; `401`/`403`
    // signaleraient un échec de vérification ou de droits, jamais attendu ici.
    assert!(
        matches!(reponse.status, 200 | 400),
        "dispatch vérifié en échec ({}) : {}",
        reponse.status,
        String::from_utf8_lossy(&reponse.body)
    );
    serde_json::from_slice(&reponse.body).expect("corps JSON")
}

/// Handler de ressource à deux tours : tour 1 élicite, tour 2 rend le contenu.
fn ressource(compteur: Compteur) -> impl Fn(&InputRound) -> Result<ResourceOutcome, String> {
    move |round| {
        compteur.0.fetch_add(1, Ordering::Relaxed);
        if round.turn == 1 {
            Ok(ResourceOutcome::InputRequired(
                InputRequiredSpec::new()
                    .elicit(
                        "confirmer",
                        "Confirmer la lecture ?",
                        json!({ "type": "object" }),
                    )
                    .with_state(json!({ "decision": "propre-a-A" })),
            ))
        } else {
            Ok(ResourceOutcome::Data(ResourceData::text(
                RESSOURCE,
                Some("text/plain".to_string()),
                "contenu",
            )))
        }
    }
}

/// Handler de prompt à deux tours : tour 1 élicite, tour 2 rend un message.
fn prompt(compteur: Compteur) -> impl Fn(&Value, &InputRound) -> Result<PromptOutcome, String> {
    move |_args, round| {
        compteur.0.fetch_add(1, Ordering::Relaxed);
        if round.turn == 1 {
            Ok(PromptOutcome::InputRequired(
                InputRequiredSpec::new()
                    .elicit(
                        "confirmer",
                        "Confirmer le prompt ?",
                        json!({ "type": "object" }),
                    )
                    .with_state(json!({ "decision": "propre-a-A" })),
            ))
        } else {
            Ok(PromptOutcome::Messages(vec![PromptMessage::user(
                "bonjour",
            )]))
        }
    }
}

fn lecture(id: i64) -> Vec<u8> {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "resources/read",
        "params": { "uri": RESSOURCE, "_meta": meta() },
    })
    .to_string()
    .into_bytes()
}

fn relecture(id: i64, token: &str) -> Vec<u8> {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "resources/read",
        "params": {
            "uri": RESSOURCE,
            "_meta": meta(),
            "inputResponses": {
                "confirmer": { "action": "accept", "content": { "ok": true } }
            },
            "requestState": token,
        },
    })
    .to_string()
    .into_bytes()
}

fn demande_prompt(id: i64) -> Vec<u8> {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "prompts/get",
        "params": { "name": PROMPT, "arguments": {}, "_meta": meta() },
    })
    .to_string()
    .into_bytes()
}

fn rejoue_prompt(id: i64, token: &str) -> Vec<u8> {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "prompts/get",
        "params": {
            "name": PROMPT,
            "arguments": {},
            "_meta": meta(),
            "inputResponses": {
                "confirmer": { "action": "accept", "content": { "ok": true } }
            },
            "requestState": token,
        },
    })
    .to_string()
    .into_bytes()
}

/// `dispatch_verified` : l'identité est établie par le vérificateur puis le
/// dispatch la lie au jeton (`sub`) — B est refusé avant le handler, A reprend.
#[test]
fn le_verificateur_etablit_l_identite_du_jeton() {
    let compteur = Compteur::default();
    let verificateur = verificateur();
    let core = ServerBuilder::new("mrtr-binding", "1.0.0")
        .tool_mrtr(
            OUTIL,
            "Confirmation liée à l'appelant",
            outil(compteur.clone()),
        )
        .require_right(OUTIL, DROIT)
        .verifier(verificateur.clone())
        .core();

    // Tour 1 : identité tirée de l'en-tête par le vérificateur, pas fournie
    // à la main au dispatch.
    let r1 = reponse_verifiee(&core, &tour_un(1), "client-a");
    assert_eq!(r1["result"]["resultType"], "input_required", "{r1}");
    assert_eq!(
        verificateur.calls(),
        1,
        "la vérification précède le dispatch"
    );
    assert_eq!(compteur.valeur(), 1, "le handler de A a tourné au tour 1");
    let token = r1["result"]["requestState"]
        .as_str()
        .expect("jeton attendu")
        .to_string();

    // B est authentifié par le vérificateur, mais le jeton a été émis pour A.
    let rb = reponse_verifiee(&core, &rejeu(2, &token), "client-b");
    assert_eq!(rb["error"]["code"], -32602, "{rb}");
    assert!(
        rb["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("another client")),
        "message de liaison à l'appelant attendu : {rb}"
    );
    assert_eq!(
        compteur.valeur(),
        1,
        "le rejeu de B n'atteint jamais le handler"
    );
    assert_eq!(verificateur.calls(), 2);

    // A reprend : le contexte du vérificateur porte bien le même client.
    let ra = reponse_verifiee(&core, &rejeu(3, &token), "client-a");
    assert_eq!(ra["result"]["resultType"], "complete", "{ra}");
    assert_eq!(ra["result"]["structuredContent"]["tour"], 2);
    assert_eq!(compteur.valeur(), 2);
    assert_eq!(verificateur.calls(), 3);
}

/// `resources/read` : la liaison `sub` couvre la méthode MRTR de lecture
/// aussi — un jeton émis pour A est refusé pour B **avant** le handler.
#[test]
fn la_liaison_couvre_resources_read() {
    let compteur = Compteur::default();
    let a = contexte("client-a");
    let b = contexte("client-b");
    let core = ServerBuilder::new("mrtr-binding", "1.0.0")
        .resource_mrtr(
            RESSOURCE,
            "Confirmation",
            "Lecture à confirmer par le client",
            ressource(compteur.clone()),
        )
        .require_resource_right(RESSOURCE, DROIT)
        .verifier(Arc::new(TestVerifier::accepting(a.clone())))
        .core();

    let r1 = reponse(&core, &lecture(1), &a);
    assert_eq!(r1["result"]["resultType"], "input_required", "{r1}");
    assert_eq!(compteur.valeur(), 1, "le handler de A a tourné au tour 1");
    let token = r1["result"]["requestState"]
        .as_str()
        .expect("jeton attendu")
        .to_string();

    let rb = reponse(&core, &relecture(2, &token), &b);
    assert_eq!(rb["error"]["code"], -32602, "{rb}");
    assert!(
        rb["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("another client")),
        "message de liaison à l'appelant attendu : {rb}"
    );
    assert_eq!(
        compteur.valeur(),
        1,
        "le rejeu de B n'atteint jamais le handler de ressource"
    );

    let ra = reponse(&core, &relecture(3, &token), &a);
    assert_eq!(ra["result"]["resultType"], "complete", "{ra}");
    assert_eq!(ra["result"]["contents"][0]["text"], "contenu", "{ra}");
    assert_eq!(compteur.valeur(), 2, "le rejeu de A atteint le handler");
}

/// `prompts/get` : même liaison — jeton de A, rejeu de B refusé en `-32602`
/// avant le handler, rejeu de A accepté.
#[test]
fn la_liaison_couvre_prompts_get() {
    let compteur = Compteur::default();
    let a = contexte("client-a");
    let b = contexte("client-b");
    let core = ServerBuilder::new("mrtr-binding", "1.0.0")
        .prompt_mrtr(
            PROMPT,
            "Prompt à confirmer par le client",
            prompt(compteur.clone()),
        )
        .require_prompt_right(PROMPT, DROIT)
        .verifier(Arc::new(TestVerifier::accepting(a.clone())))
        .core();

    let r1 = reponse(&core, &demande_prompt(1), &a);
    assert_eq!(r1["result"]["resultType"], "input_required", "{r1}");
    assert_eq!(compteur.valeur(), 1, "le handler de A a tourné au tour 1");
    let token = r1["result"]["requestState"]
        .as_str()
        .expect("jeton attendu")
        .to_string();

    let rb = reponse(&core, &rejoue_prompt(2, &token), &b);
    assert_eq!(rb["error"]["code"], -32602, "{rb}");
    assert_eq!(
        compteur.valeur(),
        1,
        "le rejeu de B n'atteint jamais le handler de prompt"
    );

    let ra = reponse(&core, &rejoue_prompt(3, &token), &a);
    assert_eq!(ra["result"]["resultType"], "complete", "{ra}");
    assert_eq!(ra["result"]["messages"][0]["content"]["text"], "bonjour");
    assert_eq!(compteur.valeur(), 2, "le rejeu de A atteint le handler");
}
