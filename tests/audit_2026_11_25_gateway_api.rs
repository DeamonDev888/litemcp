//! Audit 2026-11-25 — amélioration **A1** : compléter l'API de Gateway
//! (suppression, progression SSE, transport HTTP intégré).
//!
//! Ce fichier traverse les trois ajouts de l'audit sur les chemins réels :
//!
//! 1. **Retraits directs** : `Gateway::remove_tool` et
//!    `Gateway::remove_resource_template` suivent le motif exact de
//!    `remove_resource`/`remove_prompt` (retour `bool`, reconstruction du
//!    snapshot) et disparaissent des listes servies ensuite ;
//! 2. **Garde de récursion** : un handler qui rappelle
//!    `Gateway::dispatch_request_with_progress` sur le MÊME gateway ne
//!    contourne pas la garde — profondeur bornée, `-32603` au-delà de 32
//!    niveaux, process vivant ;
//! 3. **`Gateway::bind`** : le transport HTTP intégré sert `initialize` +
//!    `tools/list`, reflète une édition (ajout **puis** retrait) entre deux
//!    requêtes d'une MÊME connexion keep-alive, pousse la progression SSE
//!    d'un appel outillé (jeton → `notifications/progress` → réponse) et
//!    s'arrête proprement (`ServerHandle::shutdown`).

use litemcp::http::ServerHandle;
use litemcp::prelude::*;
use litemcp::protocol::{
    META_CLIENT_CAPABILITIES, META_PROTOCOL_VERSION, PROTOCOL_VERSION, ResourceContents,
};
use litemcp::{Gateway, progress::ProgressChannel};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::time::Duration;

static ID_SUIVANT: AtomicI64 = AtomicI64::new(1);

fn id_suivant() -> i64 {
    ID_SUIVANT.fetch_add(1, Ordering::Relaxed)
}

/// `_meta` conforme au profil strict par défaut (2026-07-28).
fn meta_valide() -> Value {
    json!({
        META_PROTOCOL_VERSION: PROTOCOL_VERSION,
        META_CLIENT_CAPABILITIES: {},
    })
}

/// Lie un gateway au transport intégré sur un port éphémère et le sert en
/// arrière-plan — rend `(adresse, poignée d'arrêt)`.
fn demarre(gateway: Arc<Gateway>) -> (SocketAddr, ServerHandle) {
    let serveur = gateway.bind("127.0.0.1:0").expect("bind du gateway");
    let addr = serveur.local_addr().expect("adresse locale");
    let handle = serveur.handle();
    std::thread::spawn(move || serveur.serve().expect("boucle de service"));
    (addr, handle)
}

/// Corps d'une requête POST `/mcp` — `Connection: close` quand `ferme` est
/// vrai, keep-alive HTTP/1.1 (défaut) sinon.
fn requete_post(accept: &str, corps: &str, ferme: bool) -> String {
    let value: Value = serde_json::from_str(corps).unwrap_or(Value::Null);
    let methode = value["method"].as_str().unwrap_or("").to_string();
    let nom = value["params"]["name"].as_str().map(str::to_owned);
    let mut entetes = format!("MCP-Protocol-Version: {PROTOCOL_VERSION}\r\n");
    if !methode.is_empty() {
        entetes.push_str(&format!("Mcp-Method: {methode}\r\n"));
    }
    if let Some(n) = &nom {
        entetes.push_str(&format!("Mcp-Name: {n}\r\n"));
    }
    let fermeture = if ferme { "Connection: close\r\n" } else { "" };
    format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: {accept}\r\n\
         {entetes}Content-Type: application/json\r\nContent-Length: {}\r\n\
         {fermeture}\r\n{corps}",
        corps.len()
    )
}

/// POST `/mcp` sur une **nouvelle** connexion (`Connection: close`) — rend
/// `(tête HTTP, corps)`.
fn post(addr: SocketAddr, accept: &str, corps: &Value) -> (String, String) {
    let corps = corps.to_string();
    let mut stream = TcpStream::connect(addr).expect("connexion");
    stream
        .write_all(requete_post(accept, &corps, true).as_bytes())
        .expect("écriture");
    let mut brut = String::new();
    stream.read_to_string(&mut brut).expect("lecture complète");
    let (head, recu) = brut.split_once("\r\n\r\n").expect("réponse HTTP complète");
    (head.to_string(), recu.to_string())
}

/// Une requête-réponse complète sur une connexion keep-alive déjà ouverte —
/// rend la réponse JSON-RPC décodée.
fn envoyer_et_lire(
    lecture: &mut BufReader<TcpStream>,
    ecriture: &mut TcpStream,
    message: &Value,
) -> Value {
    let corps = message.to_string();
    ecriture
        .write_all(requete_post("application/json", &corps, false).as_bytes())
        .expect("écriture de la requête");
    let mut statut = String::new();
    lecture.read_line(&mut statut).expect("ligne de statut");
    assert!(statut.starts_with("HTTP/1.1 200 OK"), "{statut}");
    let mut longueur = 0usize;
    loop {
        let mut ligne = String::new();
        lecture.read_line(&mut ligne).expect("ligne d'en-tête");
        let ligne = ligne.trim_end();
        if ligne.is_empty() {
            break;
        }
        if let Some((nom, valeur)) = ligne.split_once(':') {
            if nom.trim().eq_ignore_ascii_case("content-length") {
                longueur = valeur.trim().parse().expect("content-length numérique");
            }
        }
    }
    let mut tampon = vec![0u8; longueur];
    lecture
        .read_exact(&mut tampon)
        .expect("corps de la réponse");
    serde_json::from_slice(&tampon).expect("JSON-RPC")
}

/// `tools/list` servi sur la connexion keep-alive — rend les noms d'outils.
fn noms_outils_http(lecture: &mut BufReader<TcpStream>, ecriture: &mut TcpStream) -> Vec<String> {
    let requete = json!({
        "jsonrpc": "2.0", "id": id_suivant(), "method": "tools/list",
        "params": { "_meta": meta_valide() },
    });
    let reponse = envoyer_et_lire(lecture, ecriture, &requete);
    reponse["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list doit répondre : {reponse}"))
        .iter()
        .map(|t| t["name"].as_str().unwrap_or("").to_string())
        .collect()
}

/// Événements `data:` d'un flux SSE.
fn evenements(corps: &str) -> Vec<Value> {
    corps
        .lines()
        .filter_map(|ligne| ligne.strip_prefix("data: "))
        .map(|charge| serde_json::from_str(charge).expect("événement JSON"))
        .collect()
}

fn notifications_progression(events: &[Value]) -> Vec<&Value> {
    events
        .iter()
        .filter(|e| e["method"] == "notifications/progress")
        .collect()
}

// ------------------------------------------------------------ 1. retraits

fn noms_outils(gw: &Gateway) -> Vec<String> {
    let reponse = gw
        .dispatch_unchecked(br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
        .unwrap();
    let v: Value = serde_json::from_slice(&reponse).unwrap();
    v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap_or("").to_string())
        .collect()
}

fn gabarits(gw: &Gateway) -> Vec<String> {
    let reponse = gw
        .dispatch_unchecked(br#"{"jsonrpc":"2.0","id":1,"method":"resources/templates/list"}"#)
        .unwrap();
    let v: Value = serde_json::from_slice(&reponse).unwrap();
    v["result"]["resourceTemplates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["uriTemplate"].as_str().unwrap_or("").to_string())
        .collect()
}

#[test]
fn remove_tool_et_remove_resource_template_se_voient_dans_les_listes() {
    let gw = Gateway::new(ServerBuilder::new("gw-a1", "1.0.0").tool(
        "base",
        "outil de base",
        |_a| Ok(json!({ "ok": true })),
    ));
    gw.edit(|b| {
        b.resource("mcp://doc", "doc", "documentation vivante", || {
            Ok(ResourceContents {
                uri: "mcp://doc".to_string(),
                mime_type: Some("text/plain".to_string()),
                text: "v1".to_string(),
            })
        })
        .resource_template("mcp://items/{id}", "item", "gabarit d'item")
        .tool("srv__echo", "Echo du serveur détecté", |_a| {
            Ok(json!({ "echo": true }))
        })
    });

    // Avant retrait : les deux outils et le gabarit sont servis.
    assert!(noms_outils(&gw).contains(&"srv__echo".to_string()));
    assert_eq!(gabarits(&gw), vec!["mcp://items/{id}".to_string()]);

    // `remove_tool` : motif exact de `remove_resource`/`remove_prompt`
    // (retour `bool`, présent → true, absent ou déjà retiré → false).
    assert!(gw.remove_tool("srv__echo"), "outil présent → true");
    assert!(!gw.remove_tool("srv__echo"), "outil déjà retiré → false");
    assert!(!gw.remove_tool("inconnu"), "outil absent → false");
    assert!(
        !noms_outils(&gw).contains(&"srv__echo".to_string()),
        "l'outil retiré quitte tools/list : {:?}",
        noms_outils(&gw)
    );
    assert!(
        noms_outils(&gw).contains(&"base".to_string()),
        "l'outil restant survit"
    );
    // Un appel sur l'outil retiré devient -32602.
    let reponse = gw
        .dispatch_unchecked(
            br#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"srv__echo","arguments":{}}}"#,
        )
        .unwrap();
    let v: Value = serde_json::from_slice(&reponse).unwrap();
    assert_eq!(v["error"]["code"], -32602, "{v}");

    // `remove_resource_template` : même motif, sans toucher aux ressources
    // fixes.
    assert!(
        gw.remove_resource_template("mcp://items/{id}"),
        "gabarit présent → true"
    );
    assert!(
        !gw.remove_resource_template("mcp://items/{id}"),
        "gabarit déjà retiré → false"
    );
    assert!(gabarits(&gw).is_empty(), "le gabarit quitte la liste");
    let reponse = gw
        .dispatch_unchecked(br#"{"jsonrpc":"2.0","id":3,"method":"resources/list"}"#)
        .unwrap();
    let v: Value = serde_json::from_slice(&reponse).unwrap();
    let uris: Vec<&str> = v["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uri"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(uris, vec!["mcp://doc"], "la ressource fixe survit : {v}");
}

// ------------------------------------------------------------ 2. récursion

/// Canal de progression muet — ce qui est testé ici est la garde du
/// dispatch, pas la livraison des notifications.
struct Canal;

impl ProgressChannel for Canal {
    fn progress(&self, _progress: f64, _total: Option<f64>, _message: Option<&str>) -> bool {
        true
    }
}

#[test]
fn la_recursion_sur_dispatch_request_with_progress_ne_contourne_pas_la_garde() {
    // Corps conforme au profil strict : l'appel imbriqué doit passer toute
    // la vérification du noyau jusqu'à ce que la GARDE le refuse — tout
    // autre refus (-32600, -32602…) signifierait que le test ne mesure pas
    // la profondeur.
    let corps = json!({
        "jsonrpc": "2.0", "id": 7, "method": "tools/call",
        "params": {
            "name": "auto", "arguments": {},
            "_meta": {
                META_PROTOCOL_VERSION: PROTOCOL_VERSION,
                META_CLIENT_CAPABILITIES: {},
            },
        },
    })
    .to_string();

    let gw = Arc::new(Gateway::new(ServerBuilder::new("gw-rec-a1", "1.0.0")));
    let executions = Arc::new(AtomicUsize::new(0));
    let exterieur = Arc::clone(&gw);
    let compte = Arc::clone(&executions);
    let canal_ext: Arc<dyn ProgressChannel> = Arc::new(Canal);
    let corps_handler = corps.clone();
    let canal_handler: Arc<dyn ProgressChannel> = Arc::new(Canal);
    gw.edit(move |b| {
        let exterieur = Arc::clone(&exterieur);
        let compte = Arc::clone(&compte);
        let corps = corps_handler;
        let canal = canal_handler;
        b.tool(
            "auto",
            "s'appelle lui-même via dispatch_request_with_progress",
            move |_a| {
                compte.fetch_add(1, Ordering::SeqCst);
                let reponse = exterieur
                    .dispatch_request_with_progress(corps.as_bytes(), None, Arc::clone(&canal))
                    .and_then(|r| serde_json::from_slice::<Value>(&r.body).ok());
                match reponse {
                    // L'appel imbriqué a été REFUSÉ par la garde : remonter
                    // le drapeau et ce code.
                    Some(v) if v["error"]["code"] == json!(-32603) => Ok(json!({
                        "garde": true, "code": v["error"]["code"].clone(),
                    })),
                    // Le refus a été remonté par le niveau du dessous.
                    Some(v) if v["result"]["structuredContent"].get("garde").is_some() => {
                        Ok(json!({
                            "garde": true,
                            "code": v["result"]["structuredContent"]["code"].clone(),
                        }))
                    }
                    // Enveloppe d'erreur d'un AUTRE code : drapeau quand
                    // même, l'assertion finale échouera lisible.
                    Some(v) if v.get("error").is_some() => Ok(json!({
                        "garde": true, "code": v["error"]["code"].clone(),
                    })),
                    // La récursion continue d'un niveau.
                    _ => Ok(json!({ "suit": true })),
                }
            },
        )
    });

    let reponse = gw
        .dispatch_request_with_progress(corps.as_bytes(), None, Arc::clone(&canal_ext))
        .expect("le dispatch externe rend une réponse");
    let v: Value = serde_json::from_slice(&reponse.body).unwrap();
    assert!(
        v["error"].is_null(),
        "le dispatch externe rend une réponse, pas une erreur protocole : {v}"
    );
    assert_eq!(
        v["result"]["structuredContent"]["garde"], true,
        "la garde a coupé l'auto-appel et le drapeau remonte : {v}"
    );
    assert_eq!(
        v["result"]["structuredContent"]["code"],
        json!(-32603),
        "le refus porte -32603, et lui seul : {v}"
    );
    assert_eq!(
        executions.load(Ordering::SeqCst),
        32,
        "la garde coupe au-delà de 32 dispatchs imbriqués"
    );

    // Le process survit : le gateway sert encore.
    assert!(noms_outils(&gw).contains(&"auto".to_string()));
}

// ------------------------------------------------------------ 3. bind + SSE

#[test]
fn bind_pousse_la_progression_sse_d_un_appel_outille() {
    let gw = Gateway::shared(ServerBuilder::new("gw-sse-a1", "1.0.0").tool(
        "rapporte",
        "Rapporte sa progression",
        |_a| {
            for valeur in [0.0, 50.0, 100.0] {
                assert!(
                    litemcp::progress::report(valeur, Some(100.0), None),
                    "le canal doit être joignable depuis le handler"
                );
            }
            Ok(json!({ "fini": true }))
        },
    ));
    let (addr, _handle) = demarre(Arc::clone(&gw));

    let requete = json!({
        "jsonrpc": "2.0", "id": id_suivant(), "method": "tools/call",
        "params": {
            "name": "rapporte", "arguments": {},
            "_meta": {
                META_PROTOCOL_VERSION: PROTOCOL_VERSION,
                META_CLIENT_CAPABILITIES: {},
                "progressToken": "tok-a1",
            },
        },
    });
    let (head, flux) = post(addr, "text/event-stream", &requete);

    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("content-type: text/event-stream"),
        "flux SSE attendu : {head}"
    );

    let events = evenements(&flux);
    let progression = notifications_progression(&events);
    assert_eq!(
        progression.len(),
        3,
        "trois notifications attendues : {flux}"
    );
    for notification in &progression {
        assert_eq!(
            notification["params"]["progressToken"], "tok-a1",
            "jeton repris tel quel : {notification}"
        );
    }
    let valeurs: Vec<f64> = progression
        .iter()
        .filter_map(|e| e["params"]["progress"].as_f64())
        .collect();
    assert_eq!(valeurs, vec![0.0, 50.0, 100.0], "{valeurs:?}");

    // La réponse finale est le dernier événement, corrélée à l'id.
    let dernier = events.last().expect("événement de réponse");
    assert_eq!(dernier["id"], requete["id"], "{dernier}");
    assert_eq!(
        dernier["result"]["structuredContent"]["fini"], true,
        "{dernier}"
    );
    assert!(
        events
            .iter()
            .rposition(|e| e["method"] == "notifications/progress")
            < Some(events.len() - 1),
        "les notifications précèdent la réponse : {flux}"
    );
}

// --------------------------------------------- 4. bind : edits visibles

#[test]
fn bind_reflete_les_edits_entre_deux_requetes_d_une_meme_connexion() {
    let gw = Gateway::shared(ServerBuilder::new("gw-edit-a1", "9.9.9").legacy(true).tool(
        "base",
        "outil de base",
        |_a| Ok(json!({ "de": "base" })),
    ));
    let (addr, _handle) = demarre(Arc::clone(&gw));

    // UNE connexion keep-alive : toutes les requêtes passent sur le même
    // socket — la visibilité des edits doit être PAR REQUÊTE, pas par
    // connexion.
    let socket = TcpStream::connect(addr).expect("connexion");
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout de lecture");
    let mut lecture = BufReader::new(socket.try_clone().expect("clone du socket"));
    let mut ecriture = socket;

    // `initialize` : le gateway courant est servi, identité complète.
    let init = json!({
        "jsonrpc": "2.0", "id": id_suivant(), "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "audit-a1", "version": "0.0.0" },
            "_meta": {
                META_PROTOCOL_VERSION: PROTOCOL_VERSION,
                META_CLIENT_CAPABILITIES: {},
            },
        },
    });
    let reponse = envoyer_et_lire(&mut lecture, &mut ecriture, &init);
    assert_eq!(
        reponse["result"]["serverInfo"]["name"], "gw-edit-a1",
        "initialize sert le gateway courant : {reponse}"
    );
    assert_eq!(
        reponse["result"]["serverInfo"]["version"], "9.9.9",
        "{reponse}"
    );

    let avant = noms_outils_http(&mut lecture, &mut ecriture);
    assert_eq!(avant, vec!["base".to_string()], "{avant:?}");

    // Édition 1 : un outil apparaît — visible à la requête SUIVANTE sur la
    // même connexion.
    gw.edit(|b| {
        b.tool("srv__echo", "Echo du serveur détecté", |_a| {
            Ok(json!({ "echo": true }))
        })
    });
    let apres_ajout = noms_outils_http(&mut lecture, &mut ecriture);
    assert_eq!(
        apres_ajout,
        vec!["base".to_string(), "srv__echo".to_string()],
        "l'ajout entre deux requêtes est visible : {apres_ajout:?}"
    );

    // Édition 2 : l'outil disparaît — retrait direct via l'API A1.
    assert!(gw.remove_tool("srv__echo"));
    let apres_retrait = noms_outils_http(&mut lecture, &mut ecriture);
    assert_eq!(
        apres_retrait,
        vec!["base".to_string()],
        "le retrait entre deux requêtes est visible : {apres_retrait:?}"
    );

    // Et l'appel de l'outil retiré est refusé, sur la même connexion.
    let appel = json!({
        "jsonrpc": "2.0", "id": id_suivant(), "method": "tools/call",
        "params": {
            "name": "srv__echo", "arguments": {}, "_meta": meta_valide(),
        },
    });
    let reponse = envoyer_et_lire(&mut lecture, &mut ecriture, &appel);
    assert_eq!(reponse["error"]["code"], -32602, "{reponse}");
}

// --------------------------------------------- 5. bind : arrêt propre

#[test]
fn bind_s_arrete_proprement() {
    let gw = Gateway::shared(ServerBuilder::new("gw-stop-a1", "1.0.0").legacy(true).tool(
        "ping",
        "Sonner le gateway",
        |_a| Ok(json!({ "pong": true })),
    ));
    let serveur = gw.bind("127.0.0.1:0").expect("bind du gateway");
    let addr = serveur.local_addr().expect("adresse locale");
    let handle = serveur.handle();
    let (fini_tx, fini_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        fini_tx.send(serveur.serve()).expect("canal de fin");
    });

    // Le serveur répond avant l'arrêt.
    let ping = json!({
        "jsonrpc": "2.0", "id": id_suivant(), "method": "ping",
        "params": { "_meta": meta_valide() },
    });
    let (head, corps) = post(addr, "application/json", &ping);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    let reponse: Value = serde_json::from_str(corps.trim()).expect("JSON-RPC");
    assert_eq!(
        reponse["result"]["resultType"], "complete",
        "ping servi : {reponse}"
    );

    // Arrêt propre : `serve()` rend `Ok(())` promptement.
    handle.shutdown();
    let resultat = fini_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("serve() doit rendre après l'arrêt");
    assert!(
        resultat.is_ok(),
        "serve() rend Ok(()) après un arrêt propre : {resultat:?}"
    );
}
