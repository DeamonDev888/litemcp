//! L1 — autorisation : matrice des droits (C1 à C3).
//!
//! Matrice testée : droits × outil (autorisé / interdit / non déclaré) ×
//! liste / appel × avec et sans [`Gateway`] × avec et sans vérificateur.
//!
//! Invariant central : **un refus n'entre jamais dans le handler** (compteurs
//! atomiques à zéro) et **sans vérificateur rien ne change**.

use litemcp::Gateway;
use litemcp::auth::{AuthContext, ProofMethod, TestVerifier};
use litemcp::server::{DispatchResponse, HttpStatus, ServerBuilder, ServerCore};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

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
    AuthContext::new("client-demo", ProofMethod::Application)
        .with_label("Test")
        .with_rights(rights.iter().copied())
}

/// Compteurs d'exécution : prouvent qu'un refus n'appelle **jamais** le
/// handler.
#[derive(Clone, Default)]
struct Compteurs {
    autorise: Arc<AtomicUsize>,
    interdit: Arc<AtomicUsize>,
    sans_droit: Arc<AtomicUsize>,
    ressource: Arc<AtomicUsize>,
    prompt: Arc<AtomicUsize>,
}

fn build(compteurs: &Compteurs, avec_verifier: bool) -> ServerBuilder {
    let (a, i, s) = (
        Arc::clone(&compteurs.autorise),
        Arc::clone(&compteurs.interdit),
        Arc::clone(&compteurs.sans_droit),
    );
    let (ressource, prompt) = (
        Arc::clone(&compteurs.ressource),
        Arc::clone(&compteurs.prompt),
    );
    let builder = ServerBuilder::new("matrice", "1.0.0")
        .tool("autorise", "droit détenu", move |_args| {
            a.fetch_add(1, Ordering::Relaxed);
            Ok(json!({ "ok": true }))
        })
        .tool("interdit", "autre droit", move |_args| {
            i.fetch_add(1, Ordering::Relaxed);
            Ok(json!({ "ok": true }))
        })
        .tool("sans_droit", "aucun droit déclaré", move |_args| {
            s.fetch_add(1, Ordering::Relaxed);
            Ok(json!({ "ok": true }))
        })
        .resource("memo://secret", "secret", "réservée", move || {
            ressource.fetch_add(1, Ordering::Relaxed);
            Ok(litemcp::protocol::ResourceContents {
                uri: "memo://secret".to_string(),
                mime_type: Some("text/plain".to_string()),
                text: "contenu".to_string(),
            })
        })
        .resource("memo://public", "public", "ouverte", || {
            Ok(litemcp::protocol::ResourceContents {
                uri: "memo://public".to_string(),
                mime_type: Some("text/plain".to_string()),
                text: "contenu".to_string(),
            })
        })
        .resource_template("memo://{id}", "gabarit réservé", "réservé")
        .resource_template("memo://public/{id}", "gabarit ouvert", "ouvert")
        .prompt("reserve", "prompt réservé", move |_args| {
            prompt.fetch_add(1, Ordering::Relaxed);
            Ok(vec![litemcp::protocol::PromptMessage::user("bonjour")])
        })
        .prompt("public", "prompt ouvert", |_args| {
            Ok(vec![litemcp::protocol::PromptMessage::user("bonjour")])
        })
        .require_right("autorise", "db.read")
        .require_right("interdit", "db.write")
        .require_resource_right("memo://secret", "memo.secret")
        .require_resource_right("memo://public", "memo.public")
        .require_resource_right("memo://{id}", "memo.template")
        .require_resource_right("memo://public/{id}", "memo.public")
        .require_prompt_right("reserve", "prompt.reserve")
        .require_prompt_right("public", "prompt.public");
    if avec_verifier {
        builder.verifier(Arc::new(TestVerifier::accepting(contexte(&[
            "db.read",
            "memo.public",
            "prompt.public",
        ]))))
    } else {
        builder
    }
}

/// Les deux surfaces de dispatch : noyau nu, ou gateway (catalogue dynamique).
enum Surface {
    Core(Box<ServerCore>),
    Gateway(Arc<Gateway>),
}

impl Surface {
    fn of(builder: ServerBuilder, via_gateway: bool) -> Self {
        if via_gateway {
            Self::Gateway(Gateway::shared(builder))
        } else {
            Self::Core(Box::new(builder.core()))
        }
    }

    fn dispatch(&self, body: &[u8], context: Option<&AuthContext>) -> DispatchResponse {
        let reply = match (self, context) {
            (Self::Core(core), Some(context)) => core.dispatch_authenticated(body, None, context),
            (Self::Core(core), None) => core.dispatch_request(body, None),
            (Self::Gateway(gateway), Some(context)) => {
                gateway.dispatch_authenticated(body, None, context)
            }
            (Self::Gateway(gateway), None) => gateway.dispatch_request(body, None),
        };
        reply.expect("une requête reçoit toujours une réponse")
    }

    fn reply(&self, body: &[u8], context: Option<&AuthContext>) -> (Value, HttpStatus) {
        let response = self.dispatch(body, context);
        (
            serde_json::from_slice(&response.body).expect("corps JSON"),
            response.status,
        )
    }

    fn names(&self, method: &str, field: &str, context: Option<&AuthContext>) -> Vec<String> {
        let (value, status) = self.reply(&listing(method), context);
        assert_eq!(status, HttpStatus::Ok, "{method} : {value}");
        value["result"][field]
            .as_array()
            .unwrap_or_else(|| panic!("{method} sans tableau `{field}` : {value}"))
            .iter()
            .map(|element| match field {
                "resources" => element["uri"].as_str().unwrap_or("").to_string(),
                "resourceTemplates" => element["uriTemplate"].as_str().unwrap_or("").to_string(),
                _ => element["name"].as_str().unwrap_or("").to_string(),
            })
            .collect()
    }
}

#[test]
fn matrice_droits_x_outil_x_liste_appel_x_gateway_x_verificateur() {
    for via_gateway in [false, true] {
        for avec_verifier in [false, true] {
            let compteurs = Compteurs::default();
            let surface = Surface::of(build(&compteurs, avec_verifier), via_gateway);
            let context = contexte(&["db.read", "memo.public", "prompt.public"]);
            // Sans vérificateur, aucun contexte n'est fourni : c'est le
            // chemin historique, qui doit rester identique à l'octet près.
            let filtre = avec_verifier.then_some(&context);
            let trace = format!("gateway={via_gateway} verifier={avec_verifier}");

            // ---- liste : filtrée avec autorisation, complète sans ----
            let attendus: Vec<&str> = if avec_verifier {
                vec!["autorise"]
            } else {
                vec!["autorise", "interdit", "sans_droit"]
            };
            assert_eq!(
                surface.names("tools/list", "tools", filtre),
                attendus,
                "tools/list — {trace}"
            );

            // ---- appel autorisé : exécuté ----
            let (value, status) = surface.reply(&call("autorise"), filtre);
            assert!(
                value.get("result").is_some(),
                "appel permis — {trace} : {value}"
            );
            assert_eq!(status, HttpStatus::Ok, "{trace}");
            assert_eq!(
                compteurs.autorise.load(Ordering::Relaxed),
                1,
                "le handler autorisé doit avoir tourné — {trace}"
            );

            // ---- appel interdit : refusé, handler jamais appelé ----
            let (value, status) = surface.reply(&call("interdit"), filtre);
            if avec_verifier {
                assert_eq!(value["error"]["code"], -32023, "{trace} : {value}");
                assert_eq!(value["error"]["data"]["reason"], "right_missing", "{trace}");
                assert_eq!(status, HttpStatus::Forbidden, "{trace}");
                assert_eq!(
                    compteurs.interdit.load(Ordering::Relaxed),
                    0,
                    "le handler interdit a tourné — {trace}"
                );
            } else {
                assert!(
                    value.get("result").is_some(),
                    "sans verifier — {trace} : {value}"
                );
                assert_eq!(compteurs.interdit.load(Ordering::Relaxed), 1, "{trace}");
            }

            // ---- élément sans droit déclaré : refusé (échec fermé) ----
            let (value, status) = surface.reply(&call("sans_droit"), filtre);
            if avec_verifier {
                assert_eq!(value["error"]["code"], -32023, "{trace} : {value}");
                assert_eq!(value["error"]["data"]["reason"], "right_missing", "{trace}");
                assert_eq!(status, HttpStatus::Forbidden, "{trace}");
                assert_eq!(
                    compteurs.sans_droit.load(Ordering::Relaxed),
                    0,
                    "un outil sans droit déclaré a tourné — {trace}"
                );
            } else {
                assert!(value.get("result").is_some(), "{trace} : {value}");
                assert_eq!(compteurs.sans_droit.load(Ordering::Relaxed), 1, "{trace}");
            }

            // ---- élément inexistant : l'existence des noms n'est pas un secret ----
            let (value, status) = surface.reply(&call("fantome"), filtre);
            assert_eq!(value["error"]["code"], -32602, "{trace} : {value}");
            // Statut inchangé pour `-32602` (le noyau rend `200` pour une
            // erreur de paramètres, refus d'autorisation excepté).
            assert_eq!(status, HttpStatus::Ok, "{trace}");
            assert!(
                value["error"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("Unknown tool")),
                "{trace} : {value}"
            );

            // ---- ressources : lecture et liste ----
            let lecture = rpc("resources/read", json!({ "uri": "memo://secret" }));
            let (value, status) = surface.reply(&lecture, filtre);
            if avec_verifier {
                assert_eq!(
                    value["error"]["code"], -32023,
                    "ressource réservée — {trace}"
                );
                assert_eq!(value["error"]["data"]["reason"], "right_missing", "{trace}");
                assert_eq!(status, HttpStatus::Forbidden, "{trace}");
                assert_eq!(compteurs.ressource.load(Ordering::Relaxed), 0, "{trace}");
            } else {
                assert!(value.get("result").is_some(), "{trace} : {value}");
                assert_eq!(compteurs.ressource.load(Ordering::Relaxed), 1, "{trace}");
            }
            let attendues: Vec<&str> = if avec_verifier {
                vec!["memo://public"]
            } else {
                vec!["memo://public", "memo://secret"]
            };
            assert_eq!(
                surface.names("resources/list", "resources", filtre),
                attendues,
                "resources/list — {trace}"
            );

            // ---- gabarits : même règle ----
            let attendus: Vec<&str> = if avec_verifier {
                vec!["memo://public/{id}"]
            } else {
                vec!["memo://public/{id}", "memo://{id}"]
            };
            assert_eq!(
                surface.names("resources/templates/list", "resourceTemplates", filtre),
                attendus,
                "resources/templates/list — {trace}"
            );

            // ---- prompts : liste et get ----
            let prompt = rpc("prompts/get", json!({ "name": "reserve", "arguments": {} }));
            let (value, status) = surface.reply(&prompt, filtre);
            if avec_verifier {
                assert_eq!(value["error"]["code"], -32023, "prompt réservé — {trace}");
                assert_eq!(status, HttpStatus::Forbidden, "{trace}");
                assert_eq!(compteurs.prompt.load(Ordering::Relaxed), 0, "{trace}");
            } else {
                assert!(value.get("result").is_some(), "{trace} : {value}");
                assert_eq!(compteurs.prompt.load(Ordering::Relaxed), 1, "{trace}");
            }
            let attendus: Vec<&str> = if avec_verifier {
                vec!["public"]
            } else {
                vec!["public", "reserve"]
            };
            assert_eq!(
                surface.names("prompts/list", "prompts", filtre),
                attendus,
                "prompts/list — {trace}"
            );
        }
    }
}

#[test]
fn une_liste_vide_reste_une_reponse_complete_et_valide() {
    let compteurs = Compteurs::default();
    let core = build(&compteurs, true).core();
    let sans_droit = contexte(&[]);
    let (value, status) = {
        let response = core
            .dispatch_authenticated(&listing("tools/list"), None, &sans_droit)
            .expect("réponse");
        (
            serde_json::from_slice::<Value>(&response.body).expect("json"),
            response.status,
        )
    };
    assert_eq!(status, HttpStatus::Ok, "{value}");
    assert_eq!(value["result"]["tools"], json!([]), "{value}");
    assert_eq!(value["result"]["resultType"], "complete", "{value}");
    assert_eq!(value["result"]["ttlMs"], 60_000, "{value}");
    assert!(value["result"]["_meta"].is_object(), "{value}");
}

#[test]
fn plusieurs_droits_sur_un_meme_outil_sont_tous_exiges() {
    let compteur = Arc::new(AtomicUsize::new(0));
    let compte = Arc::clone(&compteur);
    let core = ServerBuilder::new("et", "1.0.0")
        .tool("double", "deux droits", move |_args| {
            compte.fetch_add(1, Ordering::Relaxed);
            Ok(json!({ "ok": true }))
        })
        .require_right("double", "a.one")
        .require_right("double", "a.two")
        .verifier(Arc::new(TestVerifier::accepting(contexte(&[]))))
        .core();

    for detenus in [vec![], vec!["a.one"], vec!["a.two"]] {
        let context = contexte(&detenus);
        let response = core
            .dispatch_authenticated(&call("double"), None, &context)
            .expect("réponse");
        let value: Value = serde_json::from_slice(&response.body).expect("json");
        assert_eq!(
            value["error"]["code"], -32023,
            "droits {detenus:?} : les deux sont exigés (ET) — {value}"
        );
        assert_eq!(compteur.load(Ordering::Relaxed), 0, "droits {detenus:?}");
    }

    let context = contexte(&["a.one", "a.two", "a.trois"]);
    let response = core
        .dispatch_authenticated(&call("double"), None, &context)
        .expect("réponse");
    let value: Value = serde_json::from_slice(&response.body).expect("json");
    assert!(value.get("result").is_some(), "{value}");
    assert_eq!(compteur.load(Ordering::Relaxed), 1);
}

#[test]
fn une_declaration_est_independante_de_l_ordre_d_enregistrement() {
    // `.require_right` AVANT `.tool` : même verdict, même liste.
    let core = ServerBuilder::new("ordre", "1.0.0")
        .require_right("tard", "droit.tard")
        .resource_template("memo://{id}", "g", "d")
        .require_resource_right("memo://{id}", "droit.gabarit")
        .tool("tard", "enregistré après", |_args| {
            Ok(json!({ "ok": true }))
        })
        .verifier(Arc::new(TestVerifier::accepting(contexte(&["droit.tard"]))))
        .core();
    let context = contexte(&["droit.tard", "droit.gabarit"]);
    let response = core
        .dispatch_authenticated(&call("tard"), None, &context)
        .expect("réponse");
    let value: Value = serde_json::from_slice(&response.body).expect("json");
    assert!(value.get("result").is_some(), "{value}");

    let response = core
        .dispatch_authenticated(&listing("resources/templates/list"), None, &context)
        .expect("réponse");
    let value: Value = serde_json::from_slice(&response.body).expect("json");
    assert_eq!(
        value["result"]["resourceTemplates"][0]["uriTemplate"], "memo://{id}",
        "{value}"
    );
}

#[test]
fn un_snapshot_de_gateway_applique_les_droits_du_catalogue_courant() {
    let compteur = Arc::new(AtomicUsize::new(0));
    let compte = Arc::clone(&compteur);
    let gateway = Gateway::new(
        ServerBuilder::new("gw", "1.0.0")
            .tool("initial", "d", move |_args| {
                compte.fetch_add(1, Ordering::Relaxed);
                Ok(json!({ "ok": true }))
            })
            .require_right("initial", "gw.read")
            .verifier(Arc::new(TestVerifier::accepting(contexte(&["gw.read"])))),
    );
    let context = contexte(&["gw.read"]);

    // L'outil ajouté sans droit déclaré est refusé : l'échec fermé suit le
    // snapshot courant, pas la déclaration d'origine.
    gateway.edit(|builder| builder.tool("ajoute", "sans droit", |_args| Ok(json!({}))));
    let response = gateway
        .dispatch_authenticated(&call("ajoute"), None, &context)
        .expect("réponse");
    let value: Value = serde_json::from_slice(&response.body).expect("json");
    assert_eq!(value["error"]["code"], -32023, "{value}");
    assert_eq!(value["error"]["data"]["reason"], "right_missing", "{value}");

    // Et il n'apparaît pas dans la liste filtrée du snapshot courant.
    let response = gateway
        .dispatch_authenticated(&listing("tools/list"), None, &context)
        .expect("réponse");
    let value: Value = serde_json::from_slice(&response.body).expect("json");
    let noms: Vec<&str> = value["result"]["tools"]
        .as_array()
        .expect("tableau")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert_eq!(noms, vec!["initial"], "{value}");
}

#[test]
fn cout_du_filtrage_par_requete() {
    // Le cache `OnceLock` est conservé sans autorisation ; avec autorisation,
    // la liste est reconstruite par requête (le filtrage dépend de
    // l'appelant). Mesure : 200 outils, 2 000 `tools/list`.
    use std::time::Instant;

    fn catalogue(avec_auth: bool) -> ServerCore {
        let mut builder = ServerBuilder::new("cout", "1.0.0");
        for index in 0..200 {
            let nom = format!("outil-{index:03}");
            builder = builder
                .tool(&nom, "outil de mesure", |_args| Ok(json!({})))
                .require_right(&nom, "cout.read");
        }
        if avec_auth {
            builder
                .verifier(Arc::new(TestVerifier::accepting(contexte(&["cout.read"]))))
                .core()
        } else {
            builder.core()
        }
    }

    const TOURS: u32 = 2_000;
    let sans = catalogue(false);
    let avec = catalogue(true);
    let body = listing("tools/list");
    let context = contexte(&["cout.read"]);

    // Chauffe : le premier appel remplit le cache (ou le construit).
    let _ = sans.dispatch(&body);
    let _ = avec.dispatch_authenticated(&body, None, &context);

    let debut = Instant::now();
    for _ in 0..TOURS {
        let _ = sans.dispatch(&body);
    }
    let duree_sans = debut.elapsed();

    let debut = Instant::now();
    for _ in 0..TOURS {
        let _ = avec.dispatch_authenticated(&body, None, &context);
    }
    let duree_avec = debut.elapsed();

    // Les deux chemins servent bien les 200 outils attendus.
    let response = sans.dispatch(&body).expect("réponse");
    let value: Value = serde_json::from_slice(&response).expect("json");
    assert_eq!(
        value["result"]["tools"].as_array().expect("tableau").len(),
        200
    );
    let response = avec
        .dispatch_authenticated(&body, None, &context)
        .expect("réponse");
    let value: Value = serde_json::from_slice(&response.body).expect("json");
    assert_eq!(
        value["result"]["tools"].as_array().expect("tableau").len(),
        200
    );

    eprintln!(
        "tools/list 200 outils — sans autorisation (cache OnceLock) : {:?}/requête ; \
         avec autorisation (liste reconstruite) : {:?}/requête",
        duree_sans / TOURS,
        duree_avec / TOURS
    );
    // Aucun seuil : une mesure de machine n'est pas un contrat. Le test
    // vérifie seulement que les deux chemins servent le même catalogue.
}

#[test]
fn ressource_prompt_et_gabarit_sans_droit_declare_sont_refuses_et_invisibles() {
    // Échec fermé sur les trois autres surfaces : un élément existant mais
    // sans droit déclaré est refusé à l'usage et absent de sa liste.
    let compteur = Arc::new(AtomicUsize::new(0));
    let (ressource, prompt) = (Arc::clone(&compteur), Arc::clone(&compteur));
    let core = ServerBuilder::new("ferme", "1.0.0")
        .resource("memo://nu", "nu", "sans droit", move || {
            ressource.fetch_add(1, Ordering::Relaxed);
            Ok(litemcp::protocol::ResourceContents {
                uri: "memo://nu".to_string(),
                mime_type: None,
                text: "secret".to_string(),
            })
        })
        .resource_template("memo://nu/{id}", "nu", "sans droit")
        .prompt("nu", "sans droit", move |_args| {
            prompt.fetch_add(1, Ordering::Relaxed);
            Ok(vec![litemcp::protocol::PromptMessage::user("secret")])
        })
        .verifier(Arc::new(TestVerifier::accepting(contexte(&[]))))
        .core();
    let context = contexte(&[]);

    for (method, params) in [
        ("resources/read", json!({ "uri": "memo://nu" })),
        ("prompts/get", json!({ "name": "nu", "arguments": {} })),
    ] {
        let response = core
            .dispatch_authenticated(&rpc(method, params), None, &context)
            .expect("réponse");
        let value: Value = serde_json::from_slice(&response.body).expect("json");
        assert_eq!(value["error"]["code"], -32023, "{method} : {value}");
        assert_eq!(
            value["error"]["data"]["reason"], "right_missing",
            "{method}"
        );
        assert!(value.get("result").is_none(), "aucun contenu : {value}");
    }
    assert_eq!(
        compteur.load(Ordering::Relaxed),
        0,
        "aucun handler de ressource ni de prompt n'a tourné"
    );

    for (method, field) in [
        ("resources/list", "resources"),
        ("resources/templates/list", "resourceTemplates"),
        ("prompts/list", "prompts"),
    ] {
        let response = core
            .dispatch_authenticated(&listing(method), None, &context)
            .expect("réponse");
        let value: Value = serde_json::from_slice(&response.body).expect("json");
        assert_eq!(value["result"][field], json!([]), "{method} : {value}");
    }
}

/// P3 #3 — un gabarit protégé couvre les URI **concrètes** de sa famille :
/// `resources/read` et `completion/complete` exigent alors ses droits, et le
/// gabarit n'est visible qu'avec eux. Une URI déclarée exactement garde la
/// priorité (pas de sur-restriction par un gabarit plus large).
#[test]
fn un_gabarit_protege_couvre_les_uri_concretes_de_sa_famille() {
    let lectures = Arc::new(AtomicUsize::new(0));
    let completions = Arc::new(AtomicUsize::new(0));
    let (lire, completer) = (Arc::clone(&lectures), Arc::clone(&completions));
    let core = ServerBuilder::new("gabarits", "1.0.0")
        .resource(
            "file:///secret.txt",
            "exact",
            "déclaré exactement",
            move || {
                lire.fetch_add(1, Ordering::Relaxed);
                Ok(litemcp::protocol::ResourceContents {
                    uri: "file:///secret.txt".to_string(),
                    mime_type: None,
                    text: "SECRET-EXACT".to_string(),
                })
            },
        )
        .require_resource_right("file:///secret.txt", "res.exact")
        .resource_template("file:///{name}", "famille", "famille protégée")
        .require_resource_right("file:///{name}", "res.famille")
        .completions(move |_request| {
            completer.fetch_add(1, Ordering::Relaxed);
            Ok(litemcp::protocol::Completion::new(vec![
                "SUGGESTION-FAMILLE".to_string(),
            ]))
        })
        .verifier(Arc::new(TestVerifier::accepting(contexte(&[
            "res.famille",
        ]))))
        .core();

    let lire_uri = |uri: &str, droits: &[&str]| -> (Value, HttpStatus) {
        let response = core
            .dispatch_authenticated(
                &rpc("resources/read", json!({ "uri": uri })),
                None,
                &contexte(droits),
            )
            .expect("réponse");
        (
            serde_json::from_slice(&response.body).expect("json"),
            response.status,
        )
    };
    let completer_uri = |uri: &str, droits: &[&str]| -> (Value, HttpStatus) {
        let response = core
            .dispatch_authenticated(
                &rpc(
                    "completion/complete",
                    json!({
                        "ref": { "type": "ref/resource", "uri": uri },
                        "argument": { "name": "name", "value": "a" }
                    }),
                ),
                None,
                &contexte(droits),
            )
            .expect("réponse");
        (
            serde_json::from_slice(&response.body).expect("json"),
            response.status,
        )
    };

    // Sans le droit du gabarit : refus sur les deux surfaces, zéro appel.
    for (value, status) in [
        lire_uri("file:///autre.txt", &[]),
        completer_uri("file:///autre.txt", &[]),
    ] {
        assert_eq!(value["error"]["code"], -32023, "{value}");
        assert_eq!(value["error"]["data"]["reason"], "right_missing", "{value}");
        assert_eq!(status, HttpStatus::Forbidden, "{value}");
    }
    assert_eq!(
        lectures.load(Ordering::Relaxed),
        0,
        "handler appelé sans droit"
    );
    assert_eq!(
        completions.load(Ordering::Relaxed),
        0,
        "fournisseur appelé sans droit"
    );

    // Avec le droit du gabarit : la complétion est servie.
    let (value, status) = completer_uri("file:///autre.txt", &["res.famille"]);
    assert_eq!(status, HttpStatus::Ok, "{value}");
    assert!(
        value["result"].to_string().contains("SUGGESTION-FAMILLE"),
        "{value}"
    );
    assert_eq!(completions.load(Ordering::Relaxed), 1);
    // La lecture passe le contrôle de droits mais reste inconnue au registre
    // (le gabarit n'a pas de handler) : -32602, jamais 403.
    let (value, status) = lire_uri("file:///autre.txt", &["res.famille"]);
    assert_eq!(value["error"]["code"], -32602, "{value}");
    assert_eq!(status, HttpStatus::Ok, "{value}");
    assert_eq!(lectures.load(Ordering::Relaxed), 0);

    // L'URI déclarée exactement ne dépend que de ses propres droits…
    let (value, status) = lire_uri("file:///secret.txt", &["res.famille"]);
    assert_eq!(value["error"]["data"]["reason"], "right_missing", "{value}");
    assert_eq!(status, HttpStatus::Forbidden, "{value}");
    // … et le droit exact suffit, sans celui du gabarit.
    let (value, status) = lire_uri("file:///secret.txt", &["res.exact"]);
    assert_eq!(status, HttpStatus::Ok, "{value}");
    assert_eq!(value["result"]["contents"][0]["text"], "SECRET-EXACT");
    assert_eq!(lectures.load(Ordering::Relaxed), 1);

    // `resources/templates/list` : le gabarit n'apparaît qu'avec son droit.
    let liste = |droits: &[&str]| -> Value {
        let response = core
            .dispatch_authenticated(
                &listing("resources/templates/list"),
                None,
                &contexte(droits),
            )
            .expect("réponse");
        serde_json::from_slice(&response.body).expect("json")
    };
    assert_eq!(liste(&[])["result"]["resourceTemplates"], json!([]));
    assert_eq!(
        liste(&["res.famille"])["result"]["resourceTemplates"][0]["uriTemplate"],
        "file:///{name}"
    );
}

/// Complétion d'une référence ressource, avec un jeu de droits donné — le
/// fournisseur rend un marqueur, donc `200` = le contrôle de droits est passé.
fn completer_ressource(core: &ServerCore, uri: &str, droits: &[&str]) -> (Value, HttpStatus) {
    let response = core
        .dispatch_authenticated(
            &rpc(
                "completion/complete",
                json!({
                    "ref": { "type": "ref/resource", "uri": uri },
                    "argument": { "name": "name", "value": "a" }
                }),
            ),
            None,
            &contexte(droits),
        )
        .expect("réponse");
    (
        serde_json::from_slice(&response.body).expect("json"),
        response.status,
    )
}

/// P2 — une URI couverte par **plusieurs** gabarits exige l'**union** de leurs
/// droits : le droit du seul gabarit le plus large ne suffit pas, et le verdict
/// ne dépend pas de l'ordre lexicographique des clés du `BTreeMap`. L'URI
/// déclarée exactement garde ses droits propres (aucun héritage de gabarit).
#[test]
fn gabarits_chevauchants_exigent_l_union_des_droits_de_tous_les_couvrants() {
    let lectures = Arc::new(AtomicUsize::new(0));
    let lire = Arc::clone(&lectures);
    let core = ServerBuilder::new("chevauchement", "1.0.0")
        .resource(
            "file:///exact/item",
            "exact",
            "déclaré exactement",
            move || {
                lire.fetch_add(1, Ordering::Relaxed);
                Ok(litemcp::protocol::ResourceContents {
                    uri: "file:///exact/item".to_string(),
                    mime_type: None,
                    text: "EXACT".to_string(),
                })
            },
        )
        .require_resource_right("file:///exact/item", "res.exact")
        // Chevauchement « large trié avant étroit » : `{x}` < `~{y}`.
        .resource_template("file:///pub/{x}", "pub large", "large")
        .require_resource_right("file:///pub/{x}", "res.broad")
        .resource_template("file:///pub/~{y}", "pub étroit", "étroit")
        .require_resource_right("file:///pub/~{y}", "res.narrow")
        // Même chevauchement, ordre inverse : `0{y}` trie AVANT `{x}`.
        .resource_template("file:///sec/{x}", "sec large", "large")
        .require_resource_right("file:///sec/{x}", "res.broad")
        .resource_template("file:///sec/0{y}", "sec étroit", "étroit")
        .require_resource_right("file:///sec/0{y}", "res.narrow")
        // Ces deux gabarits couvrent aussi l'URI déclarée exactement.
        .resource_template("file:///exact/{p}", "exact large", "large")
        .require_resource_right("file:///exact/{p}", "res.broad")
        .resource_template("file:///exact/i{p}", "exact étroit", "étroit")
        .require_resource_right("file:///exact/i{p}", "res.narrow")
        .completions(|_request| {
            Ok(litemcp::protocol::Completion::new(vec![
                "SUGGESTION-PROTEGEE".to_string(),
            ]))
        })
        .verifier(Arc::new(TestVerifier::accepting(contexte(&[]))))
        .core();

    // Deux URI couvertes par deux gabarits (ordres de clés opposés) et le
    // témoin de forme `0secret` : mêmes verdicts, aucun droit seul ne suffit.
    for uri in [
        "file:///pub/~secret",
        "file:///sec/0secret",
        "file:///pub/normal",
    ] {
        let chevauche = uri != "file:///pub/normal";
        let (value, status) = completer_ressource(&core, uri, &["res.broad"]);
        if chevauche {
            assert_eq!(status, HttpStatus::Forbidden, "{uri} : {value}");
            assert_eq!(
                value["error"]["data"]["reason"], "right_missing",
                "{uri} : {value}"
            );
        } else {
            // Un seul gabarit couvre : pas de sur-restriction.
            assert_eq!(status, HttpStatus::Ok, "{uri} : {value}");
        }
        let (value, status) = completer_ressource(&core, uri, &["res.narrow"]);
        assert_eq!(status, HttpStatus::Forbidden, "{uri} : {value}");
        assert_eq!(
            value["error"]["data"]["reason"], "right_missing",
            "{uri} : {value}"
        );
        let (value, status) = completer_ressource(&core, uri, &[]);
        assert_eq!(status, HttpStatus::Forbidden, "{uri} : {value}");
        let (value, status) = completer_ressource(&core, uri, &["res.broad", "res.narrow"]);
        assert_eq!(status, HttpStatus::Ok, "{uri} : {value}");
        assert!(
            value["result"].to_string().contains("SUGGESTION-PROTEGEE"),
            "{uri} : {value}"
        );
    }

    // URI déclarée exactement : ses seuls droits, sans ceux des gabarits.
    let (value, status) = completer_ressource(&core, "file:///exact/item", &["res.broad"]);
    assert_eq!(status, HttpStatus::Forbidden, "{value}");
    let (value, status) = completer_ressource(&core, "file:///exact/item", &["res.exact"]);
    assert_eq!(status, HttpStatus::Ok, "{value}");
    // Et la lecture passe le contrôle puis sert le handler.
    let response = core
        .dispatch_authenticated(
            &rpc("resources/read", json!({ "uri": "file:///exact/item" })),
            None,
            &contexte(&["res.exact"]),
        )
        .expect("réponse");
    let value: Value = serde_json::from_slice(&response.body).expect("json");
    assert_eq!(response.status, HttpStatus::Ok, "{value}");
    assert_eq!(value["result"]["contents"][0]["text"], "EXACT", "{value}");
    assert_eq!(lectures.load(Ordering::Relaxed), 1);
}

/// P3 — motifs non simples : `{?x}`, `{+x}`, `{a,b}` couvrent de façon
/// **conservatrice**, donc une famille ainsi protégée n'est plus servie sans
/// droit (l'inverse était un fail-open). Un motif qui ne couvre pas une URI ne
/// la sur-restreint pas.
#[test]
fn motifs_non_simples_couvrent_de_facon_conservatrice() {
    fn core_motifs(motifs: &[(&str, &str)]) -> ServerCore {
        let mut builder = ServerBuilder::new("motifs", "1.0.0").completions(|_request| {
            Ok(litemcp::protocol::Completion::new(vec![
                "SUGGESTION-PROTEGEE".to_string(),
            ]))
        });
        for (motif, droit) in motifs {
            builder = builder
                .resource_template(motif, "gabarit", "motif protégé")
                .require_resource_right(motif, droit);
        }
        builder
            .verifier(Arc::new(TestVerifier::accepting(contexte(&[]))))
            .core()
    }

    for (motif, droit, uri) in [
        ("file:///q{?id}", "res.q", "file:///q?id=1"),
        ("file://{+path}", "res.path", "file:///r/a/b"),
        ("file:///{a,b}", "res.choix", "file:///s"),
        ("memo://w{+p}/fin", "res.w", "memo://w/a/b/fin"),
    ] {
        let core = core_motifs(&[(motif, droit)]);
        let (value, status) = completer_ressource(&core, uri, &[]);
        assert_eq!(status, HttpStatus::Forbidden, "{motif} → {uri} : {value}");
        assert_eq!(
            value["error"]["data"]["reason"], "right_missing",
            "{motif} → {uri} : {value}"
        );
        let (value, status) = completer_ressource(&core, uri, &[droit]);
        assert_eq!(status, HttpStatus::Ok, "{motif} → {uri} : {value}");
        assert!(
            value["result"].to_string().contains("SUGGESTION-PROTEGEE"),
            "{motif} → {uri} : {value}"
        );
        // Hors de la famille : aucune restriction (témoin 200 sans droit).
        let (value, status) = completer_ressource(&core, "memo://libre", &[]);
        assert_eq!(status, HttpStatus::Ok, "{motif} : {value}");
    }
    // Suffixe littéral respecté : `memo://w{+p}/fin` ne couvre que `…/fin`.
    let core = core_motifs(&[("memo://w{+p}/fin", "res.w")]);
    let (value, status) = completer_ressource(&core, "memo://w/a/b", &[]);
    assert_eq!(status, HttpStatus::Ok, "{value}");

    // Deux motifs non simples couvrent la même URI : union exigée.
    let core = core_motifs(&[("file:///q{?id}", "res.q"), ("file:///{a,b}", "res.choix")]);
    let (value, status) = completer_ressource(&core, "file:///q?id=1", &["res.q"]);
    assert_eq!(status, HttpStatus::Forbidden, "{value}");
    let (value, status) = completer_ressource(&core, "file:///q?id=1", &["res.q", "res.choix"]);
    assert_eq!(status, HttpStatus::Ok, "{value}");
}
