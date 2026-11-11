//! Module anti-zombie — chaque test rejoue une promesse du module :
//! sweep du registre d'annonce (usurpations prouvées retirées, doute
//! conservé, vivantes vérifiées respectées, débris nettoyés, notre propre
//! annonce intouchée), drain borné à la fin de vie, bilan lisible via
//! `ServerHandle::zombie_report`, port rebinaible après l'arrêt. Les
//! serveurs tournent sur de vraies sockets (`127.0.0.1:0`). Les verdicts de
//! sonde d'identité (adresse publiée, usurpation, doute) sont couverts dans
//! les tests unitaires du module et `tests/registre_vie.rs`.

use litemcp::prelude::*;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Dossier de test unique — les sweeps unitaires n'y voient que nos
/// fichiers, jamais le registre partagé de la machine.
fn unique_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    let dir = std::env::temp_dir().join(format!("litemcp-az-{tag}-{}-{nanos}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Écrit une annonce au **format historique** du registre (contrat
/// `{ name, version, port, pid, exe, args, cwd, ts }` — sans adresse ni
/// identité publiées) — `pid` étranger sauf mention contraire.
fn write_announcement(dir: &Path, name: &str, port: u16, pid: u64) -> PathBuf {
    let doc = serde_json::json!({
        "name": name, "version": "1.0.0", "port": port, "pid": pid,
        "exe": "ztest", "args": [], "cwd": ".", "ts": 0,
    });
    let path = dir.join(format!("{name}.json"));
    fs::write(&path, doc.to_string()).unwrap();
    path
}

/// PID étranger plausible (jamais le nôtre, jamais un PID réel de service).
fn foreign_pid() -> u64 {
    u64::from(u32::MAX - 1)
}

/// Un port **libre et non réattribuable** — bind, port noté, listener lâché :
/// toute annonce qui le porte est prouvablement morte.
///
/// La tranche est prise **sous toutes les plages éphémères** (même logique
/// que la tranche LiteMCP `23080..=23099`) : l'OS ne la donnera jamais
/// spontanément à un `bind("127.0.0.1:0")` parallèle. Avec un port éphémère,
/// la course était réelle en CI (Windows réattribue en LIFO) : le port du
/// marqueur repartait à un listener voisin entre le `drop` et la sonde du
/// sweep → `alive: 1` → flaky.
fn dead_port() -> u16 {
    for port in 23100..=23119 {
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)) {
            let free = listener.local_addr().unwrap().port();
            drop(listener);
            return free;
        }
    }
    // Tranche entière occupée (improbable) : repli sur un port éphémère —
    // course possible, mais la fenêtre est microscopique.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// Un port **écouteur** de la même tranche non-éphémère — pour les tests qui
/// veulent une instance vivante (le listener reste lié) sans course avec un
/// bind parallèle.
fn live_listener() -> (TcpListener, u16) {
    for port in 23100..=23119 {
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)) {
            let bound = listener.local_addr().unwrap().port();
            return (listener, bound);
        }
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let bound = listener.local_addr().unwrap().port();
    (listener, bound)
}

/// Un `POST /mcp` réel, réponse lue jusqu'à la fermeture (`Connection: close`).
fn post(addr: SocketAddr, body: &str) -> String {
    let mut stream = TcpStream::connect(addr).unwrap();
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

#[test]
fn sweep_conserve_en_doute_une_annonce_sans_reponse() {
    let dir = unique_dir("doute");
    let path = write_announcement(&dir, "morte", dead_port(), foreign_pid());
    let report = sweep_dir(&dir);
    assert_eq!(
        report.undecided, 1,
        "un port sans réponse n'est pas une preuve de mort : l'annonce est conservée : {report:?}"
    );
    assert_eq!(
        report.reaped, 0,
        "aucun retrait sans preuve positive : {report:?}"
    );
    assert!(
        path.exists(),
        "l'annonce est conservée (doute) : {report:?}"
    );
    // Idempotent : le sweep suivant conserve aussi — jamais de retrait sans
    // preuve.
    let again = sweep_dir(&dir);
    assert_eq!(again.undecided, 1, "{again:?}");
    assert!(path.exists(), "{again:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn sweep_conserve_les_annonces_sans_identite_verifiable() {
    let dir = unique_dir("vivant");
    let (listener, port) = live_listener();
    let path = write_announcement(&dir, "vivante", port, foreign_pid());
    // Annonce au format historique (aucune identité publiée) : RIEN n'est
    // prouvable — ni vivante (le port répond, mais qui ?) ni morte : doute,
    // conservée, sans sonde.
    let report = sweep_dir(&dir);
    assert_eq!(
        report.undecided, 1,
        "sans identité publiée, aucun verdict n'est prouvable : {report:?}"
    );
    assert!(path.exists(), "{report:?}");
    drop(listener);
    // Port mort ensuite : TOUJOURS conservée — un refus n'est pas une
    // preuve de mort ; le retrait appartient au propriétaire ou à la preuve
    // d'usurpation (identité différente qui répond), impossible ici.
    let report = sweep_dir(&dir);
    assert_eq!(report.undecided, 1, "{report:?}");
    assert_eq!(report.reaped, 0, "{report:?}");
    assert!(
        path.exists(),
        "conservée : jamais de suppression sur un doute : {report:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn sweep_retire_les_fichiers_corrompus() {
    let dir = unique_dir("corrompu");
    let path = dir.join("casse.json");
    fs::write(&path, "{corrompu — pas du json").unwrap();
    let report = sweep_dir(&dir);
    assert_eq!(
        report.corrupt, 1,
        "l'écriture atomique garantit qu'un non-parsable n'est pas en cours"
    );
    assert!(!path.exists(), "le débris corrompu est retiré");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn sweep_laisse_les_fichiers_de_schema_inconnu() {
    let dir = unique_dir("etranger");
    let unknown = dir.join("etranger.json");
    fs::write(&unknown, r#"{"foo": 1}"#).unwrap();
    let zero = write_announcement(&dir, "port0", 0, foreign_pid());
    let report = sweep_dir(&dir);
    assert_eq!(
        report.foreign, 2,
        "schéma inconnu et port 0 : jamais une annonce prouvable — respectés"
    );
    assert!(unknown.exists());
    assert!(zero.exists());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn sweep_n_efface_jamais_sa_propre_annonce() {
    let dir = unique_dir("propre");
    let path = write_announcement(&dir, "moi", dead_port(), u64::from(std::process::id()));
    let report = sweep_dir(&dir);
    assert_eq!(
        report.own_skipped, 1,
        "notre annonce est gérée par le Drop du serveur lié"
    );
    assert!(path.exists(), "le sweep ne retire jamais sa propre annonce");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn sweep_sur_dossier_absent_rapport_vide() {
    let dir = std::env::temp_dir().join(format!(
        "litemcp-az-absent-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    assert!(
        sweep_dir(&dir).is_empty(),
        "dossier absent : rapport vide, pas d'erreur"
    );
    assert!(!dir.exists(), "le sweep ne crée rien");
}

#[test]
fn sweep_ne_touche_pas_aux_tmp_recents() {
    let dir = unique_dir("tmp");
    let tmp = dir.join("annonce.json.123.456.tmp");
    fs::write(&tmp, "{}").unwrap();
    let report = sweep_dir(&dir);
    assert_eq!(
        report.debris, 0,
        "un .tmp récent peut être l'écriture d'un écrivain vivant"
    );
    assert!(tmp.exists(), "les .tmp récents sont respectés");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn bind_avec_anti_zombie_reape_au_bind_et_rend_un_rapport() {
    // Marqueur placé dans le REGISTRE PARTAGÉ — nom unique à ce test, PID
    // étranger, port mort : le sweep au bind doit le retirer.
    let registry = litemcp::registry::registry_dir();
    fs::create_dir_all(&registry).unwrap();
    let marker_name = format!(
        "ztest-antizombie-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    );
    let dead = dead_port();
    let marker = write_announcement(&registry, &marker_name, dead, foreign_pid());

    let bound = ServerBuilder::new("ztest-az-bind", "1.0.0")
        .legacy(true)
        .tool("ping", "Sonner le serveur", |_args| {
            Ok(serde_json::json!({ "pong": true }))
        })
        .anti_zombie()
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = bound.local_addr().unwrap();
    let handle = bound.handle();
    // NB Windows : `Path::exists()` répond `false` sur une erreur de
    // metadata (verrou antivirus sur le fichier fraîchement écrit) — on
    // relaie l'existence réelle dans le diagnostic du bilan, plus fiable.
    // Le marqueur (port sans réponse, annonce historique sans identité) est
    // conservé en doute : le sweep ne détruit qu'en preuve positive.
    assert!(
        marker.exists(),
        "une annonce sur port sans réponse est conservée (doute — un refus TCP \
         n'est pas une preuve de mort) (marker = {marker:?})"
    );

    let worker = std::thread::spawn(move || bound.serve());
    let response = post(
        addr,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"ping","arguments":{}}}"#,
    );
    assert!(
        response.contains("pong"),
        "le serveur a servi la requête : {response}"
    );

    handle.shutdown();
    worker.join().unwrap().unwrap();

    let report = handle
        .zombie_report()
        .expect("le bilan est disponible après la fin de vie du serveur lié");
    assert!(
        report.drained,
        "aucune connexion ne doit rester au terme du drain"
    );
    assert_eq!(report.connections_pending, 0);
    assert_eq!(report.handlers_alive, 0, "aucun handler en vol ici");
    let registry_now: Vec<String> = fs::read_dir(&registry)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        report.sweep.undecided >= 1,
        "le sweep du bind a conservé en doute notre marqueur (port sans réponse) : \
         {:?} — port du marqueur {dead}, marker.exists() = {:?}, registre = {registry_now:?}",
        report.sweep,
        marker.exists(),
    );
}

#[test]
fn le_rapport_compte_les_handlers_encore_vivants() {
    let bound = ServerBuilder::new("ztest-az-hang", "1.0.0")
        .legacy(true)
        .tool("hang", "Ne finit jamais dans le délai", |_args| {
            std::thread::sleep(Duration::from_millis(1000));
            Ok(serde_json::json!({}))
        })
        .tool_timeout(Duration::from_millis(30))
        .anti_zombie()
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = bound.local_addr().unwrap();
    let handle = bound.handle();
    let worker = std::thread::spawn(move || bound.serve());

    // La réponse part au timeout (30 ms) — le handler dort encore (~1 s).
    let response = post(
        addr,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"hang","arguments":{}}}"#,
    );
    assert!(
        response.contains("\"id\":1"),
        "réponse reçue au timeout : {response}"
    );

    handle.shutdown();
    worker.join().unwrap().unwrap();

    let report = handle.zombie_report().unwrap();
    assert!(
        report.drained,
        "la connexion (close) est partie avant l'échéance"
    );
    assert_eq!(
        report.handlers_alive, 1,
        "le handler accroché termine en arrière-plan : le bilan le dit"
    );
}

#[test]
fn sans_anti_zombie_aucun_rapport_ni_sweep() {
    let bound = ServerBuilder::new("ztest-az-off", "1.0.0")
        .legacy(true)
        .tool("ping", "Sonner le serveur", |_args| {
            Ok(serde_json::json!({ "pong": true }))
        })
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = bound.local_addr().unwrap();
    let handle = bound.handle();
    assert!(handle.zombie_report().is_none(), "rien avant la fin de vie");
    let worker = std::thread::spawn(move || bound.serve());
    let response = post(addr, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    assert!(response.contains("\"tools\""));
    handle.shutdown();
    worker.join().unwrap().unwrap();
    assert!(
        handle.zombie_report().is_none(),
        "sans .anti_zombie(), aucun bilan, aucun coût"
    );
}

#[test]
fn le_port_est_rebinaible_apres_la_fin_de_vie() {
    let first = ServerBuilder::new("ztest-az-port", "1.0.0")
        .tool("ping", "Sonner le serveur", |_args| {
            Ok(serde_json::json!({ "pong": true }))
        })
        .anti_zombie()
        .bind("127.0.0.1:0")
        .unwrap();
    let addr = first.local_addr().unwrap();
    drop(first);

    // Tout se termine bien : le socket est fermé, le port rebindable.
    let second = ServerBuilder::new("ztest-az-port", "1.0.0")
        .tool("ping", "Sonner le serveur", |_args| {
            Ok(serde_json::json!({ "pong": true }))
        })
        .anti_zombie()
        .bind(&addr.to_string())
        .unwrap();
    assert_eq!(second.local_addr().unwrap(), addr);
}
