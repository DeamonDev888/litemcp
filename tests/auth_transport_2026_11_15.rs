//! Transport HTTP authentifié — bout en bout sur le loopback (lot L3).
//!
//! Ce que ces tests prouvent, avec un **vrai** client TCP :
//!
//! * une requête signée (profil `docs/auth.md` §2) reçoit `200` ;
//! * sans signature, avec une signature d'une autre clé sous un `keyid`
//!   connu, ou en rejeu : `401` + `WWW-Authenticate` **exact** et corps
//!   **constant** (le même pour les trois) ;
//! * un outil dont le droit manque : `403` et `-32023` ;
//! * `completion/complete` est filtré comme les listes (il révèle des noms
//!   d'arguments) ;
//! * `GET /health` reste ouvert et **identique** avec un vérificateur
//!   configuré (hors identité d'instance, qui est par nature unique) ;
//! * `404`, `405`, `415` passent **avant** la vérification (un refus de
//!   transport n'est pas un refus d'identité) ;
//! * `dispatch_verified` (C8) vérifie toujours : aucune requête réseau ne
//!   peut être dispatchée sans vérification quand l'autorisation est active.
#![cfg(feature = "auth-sign")]

use litemcp::auth::sign::{
    ClientKey, KeyStore, NonceCache, RequestToSign, SignatureVerifier, Signer, SigningKey,
    TestKeyStore,
};
use litemcp::auth::{
    AuthContext, PinnedServer, ProofMethod, TestVerifier, VerifiedRequest, WWW_AUTHENTICATE,
    unauthorized_body,
};
use litemcp::prelude::*;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const PROTO: &str = "2026-07-28";
const METHODE: &str = "tools/call";

/// Corps d'un `tools/call` conforme au profil strict.
fn corps_appel(nom: &str, id: u64) -> Vec<u8> {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": nom,
            "arguments": {},
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": PROTO,
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }
    })
    .to_string()
    .into_bytes()
}

/// Corps d'un `completion/complete` sur un prompt (sans `Mcp-Name`).
fn corps_completion(prompt: &str, id: u64) -> Vec<u8> {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "completion/complete",
        "params": {
            "ref": { "type": "ref/prompt", "name": prompt },
            "argument": { "name": "filtre", "value": "a" },
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": PROTO,
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }
    })
    .to_string()
    .into_bytes()
}

/// En-têtes de base d'une requête (ceux que la signature couvre).
fn entetes_de_base(addr: SocketAddr, methode: &str, nom: Option<&str>) -> Vec<(String, String)> {
    let mut entetes = vec![
        ("Host".to_string(), addr.to_string()),
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Mcp-Method".to_string(), methode.to_string()),
    ];
    if let Some(nom) = nom {
        entetes.push(("Mcp-Name".to_string(), nom.to_string()));
    }
    entetes
}

fn paires(entetes: &[(String, String)]) -> Vec<(&str, &str)> {
    entetes
        .iter()
        .map(|(nom, valeur)| (nom.as_str(), valeur.as_str()))
        .collect()
}

/// Envoie une requête HTTP brute et rend la réponse complète.
fn echanger(addr: SocketAddr, chemin: &str, entetes: &[(String, String)], corps: &[u8]) -> Vec<u8> {
    let mut flux = TcpStream::connect(addr).expect("connexion");
    flux.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    flux.set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut requete = format!("POST {chemin} HTTP/1.1\r\n");
    for (nom, valeur) in entetes {
        requete.push_str(&format!("{nom}: {valeur}\r\n"));
    }
    requete.push_str(&format!(
        "MCP-Protocol-Version: {PROTO}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        corps.len()
    ));
    let mut octets = requete.into_bytes();
    octets.extend_from_slice(corps);
    flux.write_all(&octets).expect("écriture");
    let mut reponse = Vec::new();
    flux.read_to_end(&mut reponse).expect("lecture");
    reponse
}

fn echanger_get(addr: SocketAddr, chemin: &str) -> Vec<u8> {
    let mut flux = TcpStream::connect(addr).expect("connexion");
    flux.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    flux.write_all(
        format!("GET {chemin} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .expect("écriture");
    let mut reponse = Vec::new();
    flux.read_to_end(&mut reponse).expect("lecture");
    reponse
}

fn statut(reponse: &[u8]) -> u16 {
    let ligne = String::from_utf8_lossy(reponse);
    ligne
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}

fn corps_de(reponse: &[u8]) -> &[u8] {
    match reponse
        .windows(4)
        .position(|fenetre| fenetre == b"\r\n\r\n")
    {
        Some(index) => &reponse[index + 4..],
        None => &[],
    }
}

fn en_tete(reponse: &[u8], nom: &str) -> Option<String> {
    let tete = String::from_utf8_lossy(&reponse[..reponse.len().min(4096)]).to_string();
    tete.lines()
        .take_while(|ligne| !ligne.is_empty())
        .find_map(|ligne| {
            let (cle, valeur) = ligne.split_once(':')?;
            if cle.eq_ignore_ascii_case(nom) {
                Some(valeur.trim().to_string())
            } else {
                None
            }
        })
}

/// Décision d'audit retenue par le banc : `(méthode, élément, motif)`.
type Decision = (String, Option<String>, String);

/// Serveur de test : autorisation active, deux outils, un prompt protégé.
struct Banc {
    addr: SocketAddr,
    signer: Signer,
    handle: litemcp::http::ServerHandle,
    decisions: Arc<Mutex<Vec<Decision>>>,
    completions: Arc<AtomicUsize>,
}

fn banc() -> Banc {
    let signer = Signer::new(SigningKey::generate()).with_keyid("client-demo");
    let store: Arc<TestKeyStore> = Arc::new(TestKeyStore::new());
    store.insert(
        "client-demo",
        ClientKey::new("client-demo", signer.verifying_key().to_bytes())
            .with_label("Poste de test")
            .with_rights(["db.read", "res.secret"]),
    );
    let keys: Arc<dyn KeyStore> = store;
    let verifier = SignatureVerifier::new(keys, NonceCache::with_default_policy());
    let decisions = Arc::new(Mutex::new(Vec::new()));
    let journal = Arc::clone(&decisions);
    let completions = Arc::new(AtomicUsize::new(0));
    let compteur = Arc::clone(&completions);
    let bound = ServerBuilder::new("auth-demo", "1.0.0")
        .tool("db.query", "Lit la base", |_args| {
            Ok(serde_json::json!({ "rows": [] }))
        })
        .require_right("db.query", "db.read")
        .tool("db.drop", "Efface la base", |_args| {
            Ok(serde_json::json!({}))
        })
        .require_right("db.drop", "db.write")
        .prompt("secret.prompt", "Prompt protégé", |_args| {
            Ok(vec![litemcp::protocol::PromptMessage {
                role: "user",
                content: litemcp::protocol::ContentBlock::Text(litemcp::protocol::TextContent {
                    kind: "text",
                    text: "secret".to_string(),
                }),
            }])
        })
        .require_prompt_right("secret.prompt", "prompt.read")
        .resource(
            "file:///secret.txt",
            "Secret",
            "Ressource protégée",
            || {
                Ok(litemcp::protocol::ResourceContents {
                    uri: "file:///secret.txt".to_string(),
                    mime_type: None,
                    text: "SECRET-RESOURCE".to_string(),
                })
            },
        )
        .require_resource_right("file:///secret.txt", "res.secret")
        .completions(move |_request| {
            compteur.fetch_add(1, Ordering::Relaxed);
            Ok(litemcp::protocol::Completion::new(vec![
                "secret".to_string(),
            ]))
        })
        .verifier(Arc::new(verifier))
        .on_auth_decision(move |event| {
            journal.lock().unwrap().push((
                event.method.to_string(),
                event.name.map(str::to_string),
                event.reason.unwrap_or("allowed").to_string(),
            ));
        })
        .bind("127.0.0.1:0")
        .expect("bind");
    let addr = bound.local_addr().unwrap();
    let handle = bound.handle();
    std::thread::spawn(move || {
        let _ = bound.serve();
    });
    Banc {
        addr,
        signer,
        handle,
        decisions,
        completions,
    }
}

impl Drop for Banc {
    fn drop(&mut self) {
        self.handle.shutdown();
    }
}

/// Requête signée : en-têtes de base + les trois en-têtes du profil.
fn signer_requete(
    addr: SocketAddr,
    signer: &Signer,
    methode: &str,
    nom: Option<&str>,
    corps: &[u8],
) -> Vec<(String, String)> {
    let base = entetes_de_base(addr, methode, nom);
    let authority = addr.to_string();
    let paires_base = paires(&base);
    let requete = RequestToSign::new("POST", "/mcp", &authority, &paires_base, corps);
    let signes = signer.headers(&requete);
    assert!(signes.is_complete(), "requête signable");
    let mut entetes = base;
    for (nom, valeur) in signes.iter() {
        entetes.push((nom.to_string(), valeur.to_string()));
    }
    entetes
}

#[test]
fn une_requete_signee_est_servie() {
    let banc = banc();
    let corps = corps_appel("db.query", 1);
    let entetes = signer_requete(banc.addr, &banc.signer, METHODE, Some("db.query"), &corps);
    let reponse = echanger(banc.addr, "/mcp", &entetes, &corps);
    assert_eq!(statut(&reponse), 200, "réponse : {reponse:?}");
    let valeur: serde_json::Value = serde_json::from_slice(corps_de(&reponse)).unwrap();
    assert_eq!(valeur["result"]["content"][0]["text"], "{\"rows\":[]}");
    // Le journal a bien vu une décision d'accès.
    let decisions = banc.decisions.lock().unwrap();
    assert!(
        decisions
            .iter()
            .any(|(methode, nom, motif)| methode == METHODE
                && nom.as_deref() == Some("db.query")
                && motif == "allowed"),
        "{decisions:?}"
    );
}

#[test]
fn sans_signature_le_401_est_exact_et_constant() {
    let banc = banc();
    let corps = corps_appel("db.query", 2);
    let entetes = entetes_de_base(banc.addr, METHODE, Some("db.query"));
    let reponse = echanger(banc.addr, "/mcp", &entetes, &corps);
    assert_eq!(statut(&reponse), 401);
    assert_eq!(
        en_tete(&reponse, "WWW-Authenticate").as_deref(),
        Some(WWW_AUTHENTICATE)
    );
    // Corps constant : celui des rejets de transport (code -32600, id null).
    assert_eq!(corps_de(&reponse), unauthorized_body().as_slice());
    let valeur: serde_json::Value = serde_json::from_slice(corps_de(&reponse)).unwrap();
    assert_eq!(valeur["error"]["code"], -32600);
    assert!(valeur["id"].is_null());
    // Aucun motif dans le corps.
    let texte = String::from_utf8_lossy(corps_de(&reponse));
    for interdit in [
        "signature_missing",
        "key_unknown",
        "invalid",
        "signature_invalid",
        "no verified identity",
    ] {
        assert!(!texte.contains(interdit), "fuite de motif : {texte}");
    }
    // Le motif précis est allé au journal, pas au réseau.
    let decisions = banc.decisions.lock().unwrap();
    assert!(
        decisions
            .iter()
            .any(|(_, _, motif)| motif == "signature_input_missing"),
        "{decisions:?}"
    );
}

#[test]
fn une_cle_inconnue_sous_un_keyid_connu_rend_le_meme_401() {
    let banc = banc();
    let corps = corps_appel("db.query", 3);
    let sans = echanger(
        banc.addr,
        "/mcp",
        &entetes_de_base(banc.addr, METHODE, Some("db.query")),
        &corps,
    );
    // Autre clé, même keyid : Ed25519 échoue, le corps ne doit pas changer.
    let autre = Signer::new(SigningKey::generate()).with_keyid("client-demo");
    let entetes = signer_requete(banc.addr, &autre, METHODE, Some("db.query"), &corps);
    let reponse = echanger(banc.addr, "/mcp", &entetes, &corps);
    assert_eq!(statut(&reponse), 401);
    assert_eq!(corps_de(&reponse), corps_de(&sans), "corps identique exigé");
    assert_eq!(
        en_tete(&reponse, "WWW-Authenticate").as_deref(),
        Some(WWW_AUTHENTICATE)
    );
}

#[test]
fn un_outil_sans_droit_sort_en_403_et_32023() {
    let banc = banc();
    let corps = corps_appel("db.drop", 4);
    let entetes = signer_requete(banc.addr, &banc.signer, METHODE, Some("db.drop"), &corps);
    let reponse = echanger(banc.addr, "/mcp", &entetes, &corps);
    assert_eq!(statut(&reponse), 403, "réponse : {reponse:?}");
    let valeur: serde_json::Value = serde_json::from_slice(corps_de(&reponse)).unwrap();
    assert_eq!(valeur["error"]["code"], -32023);
    assert_eq!(valeur["error"]["data"]["reason"], "right_missing");
    // Aucun en-tête d'authentification sur un 403 (l'identité est établie).
    assert_eq!(en_tete(&reponse, "WWW-Authenticate"), None);
}

#[test]
fn un_rejeu_rend_le_meme_401() {
    let banc = banc();
    let corps = corps_appel("db.query", 5);
    let entetes = signer_requete(banc.addr, &banc.signer, METHODE, Some("db.query"), &corps);
    let premier = echanger(banc.addr, "/mcp", &entetes, &corps);
    assert_eq!(statut(&premier), 200);
    let rejeu = echanger(banc.addr, "/mcp", &entetes, &corps);
    assert_eq!(statut(&rejeu), 401);
    assert_eq!(corps_de(&rejeu), unauthorized_body().as_slice());
}

#[test]
fn completion_est_filtre_comme_les_listes() {
    let banc = banc();
    let corps = corps_completion("secret.prompt", 6);
    let entetes = signer_requete(banc.addr, &banc.signer, "completion/complete", None, &corps);
    let reponse = echanger(banc.addr, "/mcp", &entetes, &corps);
    assert_eq!(statut(&reponse), 403, "réponse : {reponse:?}");
    let valeur: serde_json::Value = serde_json::from_slice(corps_de(&reponse)).unwrap();
    assert_eq!(valeur["error"]["code"], -32023);
    assert_eq!(valeur["error"]["data"]["reason"], "right_missing");
    // Le fournisseur n'a jamais été appelé : aucun nom d'argument ne fuit.
    assert_eq!(banc.completions.load(Ordering::Relaxed), 0);
}

#[test]
fn la_sonde_de_vie_reste_ouverte_et_identique() {
    // Serveur **sans** autorisation : référence du comportement historique.
    let ouvert = ServerBuilder::new("sans-auth", "1.0.0")
        .tool("db.query", "Lit", |_args| Ok(serde_json::json!({})))
        .bind("127.0.0.1:0")
        .unwrap();
    let addr_ouvert = ouvert.local_addr().unwrap();
    let handle = ouvert.handle();
    std::thread::spawn(move || {
        let _ = ouvert.serve();
    });
    let reference = echanger_get(addr_ouvert, "/health");
    handle.shutdown();
    assert_eq!(statut(&reference), 200);
    assert_eq!(corps_de(&reference), b"ok");
    assert_eq!(en_tete(&reference, "WWW-Authenticate"), None);

    // Serveur **avec** vérificateur : mêmes octets, hors `X-Instance-Id`
    // (identité d'instance, par nature unique à chaque bind).
    let banc = banc();
    let protege = echanger_get(banc.addr, "/health");
    assert_eq!(statut(&protege), 200);
    assert_eq!(corps_de(&protege), b"ok");
    assert_eq!(en_tete(&protege, "WWW-Authenticate"), None);
    let normaliser = |reponse: &[u8]| {
        let texte = String::from_utf8_lossy(reponse).to_string();
        let mut lignes: Vec<String> = texte
            .lines()
            .map(|ligne| {
                if ligne.to_ascii_lowercase().starts_with("x-instance-id:") {
                    "X-Instance-Id: <instance>".to_string()
                } else {
                    ligne.to_string()
                }
            })
            .collect();
        lignes.pop(); // dernier \r\n vide
        lignes.join("\n")
    };
    assert_eq!(
        normaliser(&protege),
        normaliser(&reference),
        "la sonde ne doit rien révéler de plus avec un vérificateur"
    );
}

#[test]
fn les_refus_de_transport_precedent_la_verification() {
    let banc = banc();
    // 415 : Content-Type non JSON, sans aucune signature.
    let corps = corps_appel("db.query", 7);
    let mut entetes = entetes_de_base(banc.addr, METHODE, Some("db.query"));
    entetes[1].1 = "text/plain".to_string();
    let reponse = echanger(banc.addr, "/mcp", &entetes, &corps);
    assert_eq!(statut(&reponse), 415, "Content-Type avant autorisation");
    // 405 : GET sur la route MCP, sans signature.
    let reponse = echanger_get(banc.addr, "/mcp");
    assert_eq!(statut(&reponse), 405);
    // 404 : route inconnue.
    let reponse = echanger_get(banc.addr, "/inconnue");
    assert_eq!(statut(&reponse), 404);
}

#[test]
fn dispatch_verified_verifie_toujours() {
    let signer = Signer::new(SigningKey::generate()).with_keyid("client-demo");
    let store: Arc<TestKeyStore> = Arc::new(TestKeyStore::new());
    store.insert(
        "client-demo",
        ClientKey::new("client-demo", signer.verifying_key().to_bytes()).with_right("db.read"),
    );
    let keys: Arc<dyn KeyStore> = store;
    let core = ServerBuilder::new("verifie", "1.0.0")
        .tool("db.query", "Lit", |_args| {
            Ok(serde_json::json!({ "rows": [] }))
        })
        .require_right("db.query", "db.read")
        .verifier(Arc::new(SignatureVerifier::new(
            keys,
            NonceCache::with_default_policy(),
        )))
        .core();
    assert!(core.authorization_active());
    let corps = corps_appel("db.query", 8);
    let mut entetes = entetes_de_base(
        "127.0.0.1:23080".parse().unwrap(),
        METHODE,
        Some("db.query"),
    );
    // Sur un socket applicatif, `MCP-Protocol-Version` fait partie de la
    // requête (le transport intégré l'ajoute lui-même dans ses tests).
    entetes.push(("MCP-Protocol-Version".to_string(), PROTO.to_string()));

    // Sans signature : 401 + en-tête exact + corps constant.
    let paires_base = paires(&entetes);
    let sans = VerifiedRequest::new("POST", "/mcp", "127.0.0.1:23080", &paires_base, &corps);
    let refus = core.dispatch_verified(&sans);
    assert_eq!(refus.status, 401);
    assert_eq!(
        refus.headers,
        vec![("WWW-Authenticate".to_string(), WWW_AUTHENTICATE.to_string())]
    );
    assert_eq!(refus.body, unauthorized_body());

    // Signée : 200, corps du handler.
    let requete = RequestToSign::new("POST", "/mcp", "127.0.0.1:23080", &paires_base, &corps);
    let signes = signer.headers(&requete);
    let mut recus = entetes.clone();
    for (nom, valeur) in signes.iter() {
        recus.push((nom.to_string(), valeur.to_string()));
    }
    let paires_recues = paires(&recus);
    let avec = VerifiedRequest::new("POST", "/mcp", "127.0.0.1:23080", &paires_recues, &corps);
    let accepte = core.dispatch_verified(&avec);
    assert_eq!(accepte.status, 200, "{:?}", accepte.body);

    // Aucun chemin « réseau » ne dispatch sans identité : `dispatch` et
    // `dispatch_request` rendent 403 / unauthenticated.
    let direct = core.dispatch_request(&corps, None).expect("réponse");
    assert_eq!(direct.status, HttpStatus::Forbidden);
    let valeur: serde_json::Value = serde_json::from_slice(&direct.body).unwrap();
    assert_eq!(valeur["error"]["data"]["reason"], "unauthenticated");
    assert_eq!(
        core.dispatch(&corps).expect("réponse"),
        direct.body,
        "même refus par `dispatch`"
    );
    assert_eq!(
        core.dispatch_unchecked(&corps).expect("réponse"),
        direct.body,
        "même refus par `dispatch_unchecked`"
    );
}

#[test]
fn un_verificateur_applicatif_sans_feature_rend_le_meme_401() {
    // Un `Verifier` du noyau (aucun `auth-sign`) suffit au transport : le
    // 401 est produit par le transport, pas par le profil de signature.
    let verificateur = TestVerifier::new(|request: &VerifiedRequest<'_>| {
        if request.header("x-client").is_some() {
            Ok(AuthContext::new("client-demo", ProofMethod::Application))
        } else {
            Err(litemcp::auth::AuthError::new("client_header_missing"))
        }
    });
    let bound = ServerBuilder::new("noyau", "1.0.0")
        .tool("ping", "Ping", |_args| {
            Ok(serde_json::json!({ "ok": true }))
        })
        .require_right("ping", "ping.call")
        .verifier(Arc::new(verificateur))
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = bound.local_addr().unwrap();
    let handle = bound.handle();
    std::thread::spawn(move || {
        let _ = bound.serve();
    });
    let corps = corps_appel("ping", 9);
    let reponse = echanger(
        addr,
        "/mcp",
        &entetes_de_base(addr, METHODE, Some("ping")),
        &corps,
    );
    assert_eq!(statut(&reponse), 401);
    assert_eq!(corps_de(&reponse), unauthorized_body().as_slice());
    handle.shutdown();
}

#[test]
fn le_document_rfc_9728_est_honnete_et_reserve_a_l_autorisation() {
    let banc = banc();
    let reponse = echanger_get(banc.addr, "/.well-known/oauth-protected-resource");
    assert_eq!(statut(&reponse), 200, "réponse : {reponse:?}");
    let document: serde_json::Value = serde_json::from_slice(corps_de(&reponse)).unwrap();
    assert!(
        document["resource"]
            .as_str()
            .is_some_and(|uri| uri.starts_with("http://") && uri.ends_with("/mcp")),
        "{document}"
    );
    assert_eq!(document["methods"], serde_json::json!(["POST"]));
    assert_eq!(document["www_authenticate"], WWW_AUTHENTICATE);
    assert_eq!(document["authorization_servers"], serde_json::json!([]));
    // Profil Signature : aucun `bearer_methods_supported` mensonger.
    assert!(
        document.get("bearer_methods_supported").is_none(),
        "{document}"
    );

    // Sans autorisation active, la route n'existe pas (404) : elle ne
    // documente pas un serveur qui n'authentifie personne.
    let ouvert = ServerBuilder::new("sans-auth", "1.0.0")
        .tool("ping", "Ping", |_args| Ok(serde_json::json!({})))
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = ouvert.local_addr().unwrap();
    let handle = ouvert.handle();
    std::thread::spawn(move || {
        let _ = ouvert.serve();
    });
    assert_eq!(
        statut(&echanger_get(addr, "/.well-known/oauth-protected-resource")),
        404
    );
    handle.shutdown();
}

#[test]
fn l_annonce_publie_l_authentification_sans_secret() {
    // Nom unique au processus : le registre est global, deux `cargo test`
    // simultanés écriraient le même fichier — l'un retirerait l'annonce de
    // l'autre.
    let nom = format!("litemcp-test-l3-annonce-{}", std::process::id());
    let empreinte = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    let adresse: SocketAddr = "127.0.0.1:23456".parse().unwrap();
    litemcp::registry::announce_at_with(
        &nom,
        "1.0.0",
        adresse,
        0x1234_5678_9ABC_DEF0,
        true,
        Some(empreinte),
    );
    let chemin = litemcp::registry::registry_dir().join(format!("{nom}.json"));
    let texte = std::fs::read_to_string(&chemin).expect("annonce écrite");
    let document: serde_json::Value = serde_json::from_str(&texte).unwrap();
    assert_eq!(document["authRequired"], true);
    assert_eq!(document["certificateFingerprint"], empreinte);
    assert_eq!(document["port"], 23456);
    // Aucun secret : ni clé privée, ni graine, ni nonce (clés JSON, entre
    // guillemets : le nom de l'annonce de test contient « annonce »).
    for interdit in ["\"private", "\"seed", "\"nonce", "\"secret"] {
        assert!(
            !texte.to_ascii_lowercase().contains(interdit),
            "l'annonce ne doit rien publier de secret ({interdit}) : {texte}"
        );
    }
    litemcp::registry::retract(&nom, 23456);
    assert!(!chemin.exists(), "retrait de l'annonce de test");
}

#[test]
fn l_epinglage_refuse_une_empreinte_changee() {
    let canonique = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
    let pin = PinnedServer::new(canonique).expect("empreinte canonique");
    assert_eq!(pin.fingerprint(), canonique);
    pin.verify(canonique).expect("identique");
    let changee = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    assert!(pin.verify(changee).is_err(), "refus explicite exigé");
    // Formes non canoniques : refusées (majuscules, longueur, schéma absent).
    assert!(PinnedServer::new("sha256:AAAA").is_err());
    assert!(PinnedServer::new("0000").is_err());
    // 64 « A » majuscules : la forme canonique exige des minuscules.
    let majuscules = format!("sha256:{}", "A".repeat(64));
    assert!(PinnedServer::new(&majuscules).is_err());
    assert_raison_epinglage();
}

/// Le refus d'épinglage ne recopie **aucune** empreinte.
fn assert_raison_epinglage() {
    let canonique = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
    let pin = PinnedServer::new(canonique).unwrap();
    let message = pin
        .verify("sha256:2222222222222222222222222222222222222222222222222222222222222222")
        .unwrap_err()
        .to_string();
    assert!(
        !message.contains("2222"),
        "empreinte dans le message : {message}"
    );
    assert!(message.contains("identity changed"));
}

/// P2 #2 — `resources/read` est utilisable par un client conforme : le
/// transport exige `Mcp-Name: <uri>` (SEP-2243) et le vérificateur compare
/// désormais `Mcp-Name` à `params.uri` (et non à `params.name`).
#[test]
fn resources_read_exige_mcp_name_egal_a_l_uri() {
    let banc = banc();
    let corps = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 41,
        "method": "resources/read",
        "params": {
            "uri": "file:///secret.txt",
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": PROTO,
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }
    })
    .to_string()
    .into_bytes();

    // Conforme : `Mcp-Name` = uri → 200 et contenu du handler.
    let entetes = signer_requete(
        banc.addr,
        &banc.signer,
        "resources/read",
        Some("file:///secret.txt"),
        &corps,
    );
    let reponse = echanger(banc.addr, "/mcp", &entetes, &corps);
    assert_eq!(
        statut(&reponse),
        200,
        "{}",
        String::from_utf8_lossy(&reponse)
    );
    assert!(
        String::from_utf8_lossy(corps_de(&reponse)).contains("SECRET-RESOURCE"),
        "contenu attendu : {}",
        String::from_utf8_lossy(corps_de(&reponse))
    );

    // `Mcp-Name` ≠ uri : refus du vérificateur, 401 de corps constant.
    let entetes = signer_requete(
        banc.addr,
        &banc.signer,
        "resources/read",
        Some("file:///autre.txt"),
        &corps,
    );
    let reponse = echanger(banc.addr, "/mcp", &entetes, &corps);
    assert_eq!(
        statut(&reponse),
        401,
        "{}",
        String::from_utf8_lossy(&reponse)
    );
    assert_eq!(corps_de(&reponse), unauthorized_body().as_slice());
}
