//! Test **bout en bout** du module `gateway` (audit 2026-10-27).
//!
//! Le gateway est la brique des gateways MCP : un serveur dont la surface
//! d'outils change **pendant qu'il sert**. Ce test traverse le chemin réel
//! (socket TCP → HTTP → dispatch → outils) et vérifie le contrat complet :
//!
//! 1. **Cadrage E2E** : un gateway monté dans une app hôte (route HTTP)
//!    sert `tools/list` et `tools/call` sur de vraies sockets ;
//! 2. **Cycle de vie dynamique** : un outil apparaît, un outil disparaît,
//!    le catalogue suit — sans redémarrage ;
//! 3. **Cohérence du contrat** : ce que `tools/list` annonce est
//!    exactement ce que `tools/call` exécute (règle « publié = appliqué ») ;
//! 4. **Isolation des requêtes en vol** : une requête servie par un snapshot
//!    reste servie par CE snapshot (pas de bascule en cours de route) ;
//! 5. **Persistance** : `resources`/`prompts` survivent aux edits d'outils ;
//! 6. **Isolation multi-gateways** : deux gateways ne se contaminent pas ;
//! 7. **Résilience** : un handler qui panique ne casse pas le gateway, et le
//!    snapshot reste cohérent.

use litemcp::Gateway;
use litemcp::prelude::*;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

// ---------------------------------------------------------------- helpers

fn spawn_gateway(gateway: Arc<Gateway>) -> SocketAddr {
    // Bind **une seule fois, dans le thread appelant** : le socket écoute
    // dès le retour de cette fonction (les connects s'empilent dans le
    // backlog même avant que la boucle n'accepte). Re-binder la même
    // adresse dans le thread spawné ouvrait une fenêtre de course —
    // entre la fermeture du premier listener et le second bind, un
    // connect du test recevait `ConnectionRefused` (vu une fois en CI).
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    // Le gateway est partagé : chaque requête HTTP prend le snapshot
    // courant au moment du dispatch.
    let gw = Arc::clone(&gateway);

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let gw = Arc::clone(&gw);
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                // Route du gateway : dispatch via le snapshot courant.
                let body = request.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
                let reply = gw.dispatch(body.as_bytes()).unwrap_or_default();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    reply.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(&reply);
                let _ = stream.flush();
            });
        }
    });
    addr
}

/// Dispatch direct (sans HTTP) — le chemin embarqué.
fn dispatch(gw: &Gateway, body: &str) -> Option<Value> {
    let reply = gw.dispatch(body.as_bytes())?;
    Some(serde_json::from_slice(&reply).unwrap())
}

fn tool_names(gw: &Gateway) -> Vec<String> {
    let v = dispatch(gw, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).unwrap();
    v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

/// Construit des arguments **valides** d'après le `inputSchema` publié :
/// pour chaque champ `required`, une valeur du type déclaré.
fn arguments_valides(schema: &Value) -> Value {
    let mut arguments = serde_json::Map::new();
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    let properties = schema.get("properties").and_then(Value::as_object);
    for nom in required {
        let propriete = properties.and_then(|p| p.get(nom));
        let valeur = match propriete
            .and_then(|p| p.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("string")
        {
            "integer" | "number" => json!(1),
            "boolean" => json!(true),
            "array" => json!([]),
            "object" => json!({}),
            _ => json!("x"),
        };
        arguments.insert(nom.to_string(), valeur);
    }
    Value::Object(arguments)
}

fn call(gw: &Gateway, name: &str) -> Value {
    dispatch(
        gw,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": name, "arguments": {} }
        })
        .to_string(),
    )
    .unwrap()
}

// ---------------------------------------------------------------- 1. E2E

#[test]
fn e2e_gateway_sert_sur_de_vraies_sockets() {
    let gw = Arc::new(Gateway::new(ServerBuilder::new("gw-e2e", "1.0.0").tool(
        "base",
        "outil de base",
        |_a| Ok(json!({ "from": "base" })),
    )));
    let addr = spawn_gateway(Arc::clone(&gw));

    // Requête HTTP réelle vers la route du gateway.
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    let request = format!(
        "POST /mcp/gateway?token=essai HTTP/1.1\r\nHost: hote\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    s.write_all(request.as_bytes()).unwrap();
    let mut raw = String::new();
    let _ = s.read_to_string(&mut raw);
    assert!(raw.starts_with("HTTP/1.1 200 OK"), "{raw:?}");
    assert!(
        raw.contains("\"base\""),
        "le gateway doit servir son outil : {raw:?}"
    );
}

// ---------------------------------------------------------------- 2. cycle de vie

#[test]
fn cycle_de_vie_dynamique_sans_redemarrage() {
    let gw = Gateway::new(ServerBuilder::new("gw", "1.0.0"));
    assert_eq!(tool_names(&gw).len(), 0, "un gateway vide ne publie rien");

    // Apparition
    gw.edit(|b| {
        b.tool("serveur_a", "serveur A détecté", |_a| {
            Ok(json!({ "s": "A" }))
        })
    });
    assert_eq!(tool_names(&gw), vec!["serveur_a".to_string()]);
    assert_eq!(
        call(&gw, "serveur_a")["result"]["structuredContent"]["s"],
        "A"
    );

    // Disparition
    gw.edit(|mut b| {
        assert!(b.remove_tool("serveur_a"));
        b
    });
    assert!(tool_names(&gw).is_empty(), "l'outil doit disparaître");
    let reponse = call(&gw, "serveur_a");
    assert_eq!(
        reponse["error"]["code"], -32602,
        "un outil retiré doit répondre Unknown tool : {reponse}"
    );

    // Réapparition avec un NOUVEAU comportement
    gw.edit(|b| b.tool("serveur_b", "serveur B", |_a| Ok(json!({ "s": "B" }))));
    assert_eq!(tool_names(&gw), vec!["serveur_b".to_string()]);
    assert_eq!(
        call(&gw, "serveur_b")["result"]["structuredContent"]["s"],
        "B"
    );
}

// ---------------------------------------------------------------- 3. contrat

#[test]
fn le_catalogue_annonce_exactement_ce_qui_est_execute() {
    let gw = Gateway::new(ServerBuilder::new("gw", "1.0.0"));
    gw.edit(|b| {
        b.tool("avec_schema", "outil typé", |_a| Ok(json!({ "v": 1 })))
            .tool_typed(
                "typé",
                "outil à schéma",
                zod::object().field("n", zod::integer().min(0)),
                |a| Ok(json!({ "n": a["n"].clone() })),
            )
    });

    let liste = dispatch(&gw, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).unwrap();
    let outils = liste["result"]["tools"].as_array().unwrap();

    // Chaque outil publié doit être appelable avec des arguments qui
    // respectent SON schéma publié — un refus ici serait un bug de contrat
    // (« le schéma publié est exécuté »).
    for outil in outils {
        let nom = outil["name"].as_str().unwrap().to_string();
        let arguments = arguments_valides(outil.get("inputSchema").unwrap());
        let reponse = dispatch(
            &gw,
            &json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": nom, "arguments": arguments }
            })
            .to_string(),
        )
        .unwrap();
        assert!(
            reponse["error"].is_null(),
            "un outil publié doit être appelable : {nom} → {reponse}"
        );
    }

    // Le même outil typé, appelé AVEC les arguments valides, réussit.
    let reponse = dispatch(
        &gw,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "typé", "arguments": { "n": 7 } }
        })
        .to_string(),
    )
    .unwrap();
    assert_eq!(
        reponse["result"]["structuredContent"]["n"], 7,
        "l'appel valide doit réussir : {reponse}"
    );

    // Le schéma publié doit être exécuté : un appel hors schéma est refusé
    // (règle « publié = appliqué »).
    let reponse = dispatch(
        &gw,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "typé", "arguments": { "n": -5 } }
        })
        .to_string(),
    )
    .unwrap();
    assert_eq!(
        reponse["error"]["code"], -32602,
        "le schéma publié doit être exécuté : {reponse}"
    );
}

// ---------------------------------------------------------------- 4. isolation

#[test]
fn une_requete_en_vol_reste_servie_par_son_snapshot() {
    let gw = Arc::new(Gateway::new(ServerBuilder::new("gw", "1.0.0")));
    let compte = Arc::new(AtomicUsize::new(0));

    // **Synchronisation déterministe, aucune hypothèse temporelle** :
    // l'ancien `sleep(50 ms)` supposait que le thread de requête avait déjà
    // pris son snapshot avant l'edit — l'ordonnanceur ne le garantit pas
    // (test intermittent : l'edit gagne la course, l'outil est déjà retiré
    // au dispatch et l'appel rend `-32602`). Le handler signale son ENTRÉE
    // (il ne peut s'exécuter qu'après le `snapshot()` du dispatch), le
    // thread principal attend ce signal avant de retirer l'outil, puis
    // autorise le handler à terminer : l'invariant « requête en vol servie
    // par son ancien snapshot » est tenu par construction, pas par chance.
    let (entree_tx, entree_rx) = mpsc::channel::<()>();
    let (poursuite_tx, poursuite_rx) = mpsc::channel::<()>();
    // `Receiver` n'est pas `Sync` : le handler est `Send + Sync`, d'où le
    // `Mutex` (un seul thread de requête consomme l'autorisation).
    let poursuite_rx = Mutex::new(poursuite_rx);

    let c = Arc::clone(&compte);
    gw.edit(|b| {
        b.tool("lent", "handler lent", move |_a| {
            entree_tx
                .send(())
                .expect("le test doit être en attente de l'entrée");
            poursuite_rx
                .lock()
                .expect("verrou de poursuite non empoisonné")
                .recv()
                .expect("le test doit autoriser la poursuite");
            c.fetch_add(1, Ordering::SeqCst);
            Ok(json!({ "lent": true }))
        })
    });

    let gw2 = Arc::clone(&gw);
    let requete = std::thread::spawn(move || {
        dispatch(
            &gw2,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"lent","arguments":{}}}"#,
        )
        .unwrap()
    });

    // Le handler tourne ET retient sa requête : son snapshot est déjà pris.
    // L'attente est bornée pour qu'un harnais cassé échoue clairement au
    // lieu de figer la suite.
    entree_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("le handler doit signaler son entrée");

    // L'outil est retiré PENDANT l'exécution : le snapshot courant ne le
    // publie plus, mais la requête en vol doit rester servie par le sien.
    gw.edit(|mut b| {
        assert!(b.remove_tool("lent"), "l'outil en vol doit exister");
        b
    });
    assert!(
        tool_names(&gw).is_empty(),
        "le snapshot courant ne doit plus publier l'outil retiré"
    );

    // Autorisation de terminer : le retrait est déjà publié.
    poursuite_tx
        .send(())
        .expect("le handler doit attendre l'autorisation");

    let reponse = requete.join().unwrap();
    assert_eq!(
        reponse["result"]["structuredContent"]["lent"], true,
        "la requête en vol doit être servie par son snapshot : {reponse}"
    );
    assert_eq!(compte.load(Ordering::SeqCst), 1, "le handler a dû tourner");
}

// ---------------------------------------------------------------- 5. persistance

#[test]
fn resources_et_prompts_survivent_aux_edits_doutils() {
    let gw = Gateway::new(ServerBuilder::new("gw", "1.0.0"));
    gw.edit(|b| {
        b.resource("mcp://etat", "état", "état vivant", || {
            Ok(litemcp::protocol::ResourceContents {
                uri: "mcp://etat".to_string(),
                mime_type: Some("application/json".to_string()),
                text: "{}".to_string(),
            })
        })
        .prompt("salut", "prompt simple", |_args| {
            Ok(vec![litemcp::protocol::PromptMessage::user("bonjour")])
        })
    });

    for _ in 0..5 {
        gw.edit(|b| b.tool("outil", "temporaire", |_a| Ok(json!({}))));
        gw.edit(|mut b| {
            b.remove_tool("outil");
            b
        });
    }

    let ressources =
        dispatch(&gw, r#"{"jsonrpc":"2.0","id":1,"method":"resources/list"}"#).unwrap();
    assert_eq!(
        ressources["result"]["resources"][0]["uri"], "mcp://etat",
        "les ressources doivent survivre aux edits : {ressources}"
    );
    let prompts = dispatch(&gw, r#"{"jsonrpc":"2.0","id":1,"method":"prompts/list"}"#).unwrap();
    assert_eq!(
        prompts["result"]["prompts"][0]["name"], "salut",
        "les prompts doivent survivre aux edits : {prompts}"
    );
}

// ---------------------------------------------------------------- 6. isolation

#[test]
fn deux_gateways_ne_se_contaminent_pas() {
    let a = Gateway::new(ServerBuilder::new("gw-a", "1.0.0"));
    let b = Gateway::new(ServerBuilder::new("gw-b", "1.0.0"));
    a.edit(|x| x.tool("a_un", "a", |_a| Ok(json!({}))));
    b.edit(|x| x.tool("b_un", "b", |_a| Ok(json!({}))));
    assert_eq!(tool_names(&a), vec!["a_un".to_string()]);
    assert_eq!(tool_names(&b), vec!["b_un".to_string()]);
}

// ---------------------------------------------------------------- 7. résilience

#[test]
fn un_handler_qui_panique_ne_casse_pas_le_gateway() {
    let gw = Gateway::new(ServerBuilder::new("gw", "1.0.0"));
    gw.edit(|b| {
        b.tool("panique", "handler qui panic", |_a| panic!("boom"))
            .tool("sain", "handler sain", |_a| Ok(json!({ "ok": true })))
    });

    // La panique du handler est **capturée** par le noyau (`call_tool` rend
    // un `-32603`) — le dispatch rend une réponse propre, pas un plantage.
    let reponse = call(&gw, "panique");
    assert_eq!(
        reponse["error"]["code"], -32603,
        "la panique doit produire une erreur propre : {reponse}"
    );

    // Le gateway continue de servir.
    assert_eq!(tool_names(&gw).len(), 2);
    let reponse = call(&gw, "sain");
    assert_eq!(reponse["result"]["structuredContent"]["ok"], true);
}

// ---------------------------------------------------------------- 8. lot

#[test]
fn edit_batch_publie_le_catalogue_complet_d_un_coup() {
    let gw = Gateway::new(ServerBuilder::new("gw", "1.0.0"));
    gw.edit_batch(|b| {
        b.tool("srv_a", "A", |_a| Ok(json!({})))
            .tool("srv_b", "B", |_a| Ok(json!({})))
            .tool("srv_c", "C", |_a| Ok(json!({})))
    });
    assert_eq!(tool_names(&gw).len(), 3, "le lot doit publier les 3 outils");
}

// ---------------------------------------------------------------- 9. retrait de ressource / prompt

#[test]
fn retrait_de_ressource_et_de_prompt() {
    let gw = Gateway::new(ServerBuilder::new("gw", "1.0.0"));
    gw.edit(|b| {
        b.resource("mcp://a", "a", "doc", || {
            Ok(litemcp::protocol::ResourceContents {
                uri: "mcp://a".to_string(),
                mime_type: None,
                text: "{}".to_string(),
            })
        })
        .prompt("p1", "prompt", |_a| {
            Ok(vec![litemcp::protocol::PromptMessage::user("x")])
        })
    });

    assert!(gw.remove_resource("mcp://a"), "la ressource existe → true");
    assert!(!gw.remove_resource("mcp://inconnu"), "absente → false");
    assert!(gw.remove_prompt("p1"), "le prompt existe → true");
    assert!(!gw.remove_prompt("inconnu"), "absent → false");

    let ressources =
        dispatch(&gw, r#"{"jsonrpc":"2.0","id":1,"method":"resources/list"}"#).unwrap();
    assert!(
        ressources["result"]["resources"]
            .as_array()
            .unwrap()
            .is_empty(),
        "la ressource doit avoir disparu : {ressources}"
    );
    let prompts = dispatch(&gw, r#"{"jsonrpc":"2.0","id":1,"method":"prompts/list"}"#).unwrap();
    assert!(
        prompts["result"]["prompts"].as_array().unwrap().is_empty(),
        "le prompt doit avoir disparu : {prompts}"
    );
}

// ------------------------------------------------- 10. plafond inflight global

/// Régression : chaque noyau recréait son compteur d'inflight — des handlers
/// détachés d'un ANCIEN snapshot (timeout) ne comptaient plus dans le noyau
/// neuf publié par `edit` : `Gateway::edit` contournait
/// `MAX_INFLIGHT_HANDLERS`. Tous les snapshots d'un gateway partagent
/// désormais UN compteur : le plafond global tient après plusieurs edits,
/// les slots reviennent à la terminaison des détachés, et le repli conserve
/// le bon type d'erreur (`rate_limited`).
#[test]
fn edit_ne_contourne_pas_le_plafond_inflight_global() {
    // Les handlers « bloque » patientent sur cette variable jusqu'à la
    // libération explicite — chaque slot reste retenu jusque-là.
    let release = Arc::new(std::sync::Mutex::new(false));
    let cvar = Arc::new(std::sync::Condvar::new());
    let bloque_release = Arc::clone(&release);
    let bloque_cvar = Arc::clone(&cvar);
    let gw = Gateway::new(
        ServerBuilder::new("gw-plafond", "1.0.0")
            .tool_timeout(std::time::Duration::from_millis(60))
            .tool(
                "bloque",
                "retient son slot jusqu'à la libération",
                move |_a| {
                    let mut engage = bloque_release.lock().unwrap();
                    while !*engage {
                        engage = bloque_cvar.wait(engage).unwrap();
                    }
                    Ok(json!({}))
                },
            )
            .tool("ok", "répond vite", |_a| Ok(json!({ "vite": true }))),
    );

    let req_ok = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "ok", "arguments": {} }
    })
    .to_string();

    // Témoin : à compteur vide, l'outil passe — le plafond n'est pas la faute
    // de l'outil, c'est bien la saturation globale qui refuse.
    let temoin = dispatch(&gw, &req_ok).unwrap();
    assert_eq!(
        temoin["result"]["structuredContent"]["vite"], true,
        "l'outil doit passer à compteur vide : {temoin}"
    );

    // `MAX_INFLIGHT_HANDLERS` appels sur l'ancien snapshot : chaque handler
    // prend son slot puis est détaché par le timeout (60 ms) — il retient
    // son slot jusqu'à sa terminaison effective, ici bloqué sur la variable
    // de libération.
    let ancien = gw.snapshot();
    let req_bloque = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "bloque", "arguments": {} }
    })
    .to_string();
    let appels: Vec<_> = (0..litemcp::server::MAX_INFLIGHT_HANDLERS)
        .map(|_| {
            let ancien = Arc::clone(&ancien);
            let req = req_bloque.clone();
            std::thread::spawn(move || {
                let reply = ancien.dispatch(req.as_bytes()).unwrap();
                serde_json::from_slice::<Value>(&reply).unwrap()
            })
        })
        .collect();
    for (i, appel) in appels.into_iter().enumerate() {
        let v = appel.join().unwrap();
        assert_eq!(
            v["result"]["structuredContent"]["error"]["code"], "timeout",
            "appel {i} : le handler doit être détaché par le timeout : {v}"
        );
    }

    // Deux edits successifs : avant le correctif, chaque nouveau noyau
    // repartait de zéro — les nouveaux appels passaient alors que les 256
    // handlers détachés vivaient encore. Le plafond global doit les refuser
    // tous, avec le bon type d'erreur.
    gw.edit(|b| b);
    for i in 0..8 {
        let v = dispatch(&gw, &req_ok).unwrap();
        assert_eq!(
            v["result"]["structuredContent"]["error"]["code"], "rate_limited",
            "appel {i} : le plafond global doit refuser (256 détachés + 1) : {v}"
        );
        assert_eq!(
            v["result"]["isError"], true,
            "le refus doit être un échec d'exécution, pas une erreur protocole : {v}"
        );
    }
    gw.edit(|b| b.tool("marqueur", "publié après saturation", |_a| Ok(json!({}))));
    let v = dispatch(&gw, &req_ok).unwrap();
    assert_eq!(
        v["result"]["structuredContent"]["error"]["code"], "rate_limited",
        "après un second edit, le plafond global tient toujours : {v}"
    );

    // Libération : les 256 handlers détachés terminent et rendent leurs
    // slots — le plafond redevient disponible.
    *release.lock().unwrap() = true;
    cvar.notify_all();
    let mut rendu = false;
    for _ in 0..200 {
        std::thread::sleep(std::time::Duration::from_millis(25));
        let v = dispatch(&gw, &req_ok).unwrap();
        if v["result"]["structuredContent"]["vite"] == true {
            rendu = true;
            break;
        }
    }
    assert!(
        rendu,
        "les slots doivent revenir à la terminaison des handlers détachés"
    );
}
