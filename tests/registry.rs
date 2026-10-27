//! Intégration de l'annonce MCP Control (`announce()`) sur un serveur réel :
//! le fichier d'annonce suit exactement le cycle de vie du serveur lié —
//! écrit au bind, retiré à la fin de vie, **quelle qu'elle soit** (arrêt
//! gracieux via `serve()`/`shutdown`, ou simple `drop` sans `serve()`).

use litemcp::ServerBuilder;
use std::path::PathBuf;

/// Le registre est un dossier **partagé** : suffixer les noms de test par le
/// PID évite que deux exécutions concurrentes (deux checkouts, CI shardée)
/// se marchent dessus.
fn test_name(base: &str) -> String {
    format!("{base}-{}", std::process::id())
}

fn announce_path(name: &str) -> PathBuf {
    litemcp::registry::registry_dir().join(format!("{name}.json"))
}

#[test]
fn announce_suit_exactement_le_cycle_de_vie_du_serveur_lie() {
    let name = test_name("litemcp-test-annonce-integration");
    let path = announce_path(&name);
    let _ = std::fs::remove_file(&path);

    // 1. bind écrit l'annonce (le port effectif est résolu, jamais 0)…
    let bound = ServerBuilder::new(&name, "1.0.0")
        .tool("t", "t", |_a| Ok(serde_json::json!({})))
        .announce()
        .bind("127.0.0.1:0")
        .expect("bind");
    assert!(path.exists(), "l'annonce doit être écrite au bind");

    // 2. … et un drop sans serve() la retire : le Drop de BoundServer couvre
    //    cette fin de vie aussi (sinon le fichier affirmerait un serveur
    //    vivant qui n'existe plus).
    drop(bound);
    assert!(!path.exists(), "drop sans serve() doit retirer l'annonce");

    // 3. Cycle complet : bind → serve → shutdown → retrait.
    let bound = ServerBuilder::new(&name, "1.0.0")
        .tool("t", "t", |_a| Ok(serde_json::json!({})))
        .announce()
        .bind("127.0.0.1:0")
        .expect("bind 2");
    let handle = bound.handle();
    let worker = std::thread::spawn(move || bound.serve());
    // Le serveur peut mettre quelques ms à démarrer : attente bornée de
    // l'annonce (déjà validée au bind, mais l'écriture est idempotente).
    assert!(
        path.exists(),
        "l'annonce doit être présente pendant le service"
    );
    handle.shutdown();
    // serve() rend Ok(()) à l'arrêt ; le Drop s'exécute ensuite dans le
    // thread — join() garantit que le retrait a eu lieu avant l'assertion.
    worker.join().expect("serve").expect("serve Ok");
    assert!(!path.exists(), "arrêt gracieux doit retirer l'annonce");
}

#[test]
fn sans_announce_aucun_fichier_n_est_ecrit() {
    let name = test_name("litemcp-test-sans-announce");
    let path = announce_path(&name);
    let _ = std::fs::remove_file(&path);
    let bound = ServerBuilder::new(&name, "1.0.0")
        .tool("t", "t", |_a| Ok(serde_json::json!({})))
        .bind("127.0.0.1:0")
        .expect("bind");
    drop(bound);
    assert!(
        !path.exists(),
        "opt-out : aucune annonce ne doit être écrite"
    );
}

#[test]
fn bind_en_echec_n_ecrit_aucune_annonce() {
    // Le nom est sûr et le port libre, mais la route est invalide : bind()
    // échoue AVANT la liaison TCP — aucune annonce ne doit exister, et rien
    // ne doit paniquer (l'annonce est une commodité, jamais une condition).
    let name = test_name("litemcp-test-bind-echec");
    let path = announce_path(&name);
    let _ = std::fs::remove_file(&path);
    let result = ServerBuilder::new(&name, "1.0.0")
        .tool("t", "t", |_a| Ok(serde_json::json!({})))
        .announce()
        .route("pas-une-route")
        .bind("127.0.0.1:0");
    assert!(result.is_err(), "route invalide : bind doit échouer");
    assert!(
        !path.exists(),
        "un bind en échec ne doit pas publier d'annonce"
    );
}
