//! Annonce des serveurs au MCP Control (intégration native LiteMCP).
//!
//! Opt-in : `.announce()` sur le builder. Au bind réussi, un JSON est écrit
//! dans `%TEMP%/litemcp-registry/<nom>.json` (nom = serverInfo.name) avec tout
//! ce qu'il faut pour que MCP Control gère le process nativement :
//! port effectif du listener, PID, chemin de l'exe, arguments et répertoire
//! de travail — Start/Stop/autoboot sans scan de ports ni saisie manuelle.
//! L'annonce est retirée à la fin de vie du serveur lié — arrêt gracieux
//! **ou** simple `drop` après `bind` (voir [`crate::http::BoundServer`]).
//!
//! Contrat de fichier (stable, lu toléralement par MCP Control — un fichier absent
//! ou corrompu n'est jamais une erreur côté lecteur) :
//! `{ "name", "version", "addr", "port", "instance", "pid", "exe", "args", "cwd", "ts" }`
//! où `addr` est l'adresse effective **connectable** du listener (un bind
//! multi-interface est publié par son bouclage — `0.0.0.0` → `127.0.0.1`,
//! `::` → `::1` ; un bind spécifique est publié tel quel) et `instance` une
//! identité d'instance vérifiable : le serveur l'expose sur `/health`
//! (en-tête `X-Instance-Id`), ce qui permet à une sonde de prouver QUELLE
//! instance occupe le port — un simple refus TCP n'est pas une preuve de
//! mort, et un port réutilisé ne prouve pas l'identité de l'annonceur.
//! Un process par nom : le fichier est écrasé à chaque démarrage, l'instance
//! la plus récente gagne. `retract` ne le supprime que si le couple
//! `(pid, port)` qu'il contient est celui de l'instance appelante — et, pour
//! le [`BoundServer`](crate::http::BoundServer), si l'**identité d'instance**
//! publiée est aussi la sienne : un redémarrage dans le même process, sur le
//! même port, publie une autre identité, qu'un port réutilisé ne doit pas
//! faire passer pour la sienne. Écriture et retrait sont **sérialisés par un
//! verrou inter-processus par nom** (dossier créé exclusivement — la solution
//! la plus simple sans dépendance) : ni l'arrêt d'une instance plus ancienne
//! (autre process, autre port ou autre instance) ni une course
//! lecture→suppression ne peut effacer l'annonce d'une instance vivante plus
//! récente.
//! ⚠️ Le fichier publie la ligne de commande (`args`) : ne passez pas de
//! secrets par argv — préférez les variables d'environnement ou un fichier
//! de configuration.

use std::fs;
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Séquence process-locale — deux instances liées dans le même process
/// (même nanoseconde) ne partagent jamais d'identité.
static INSTANCE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Dossier d'annonce partagé : `%TEMP%/litemcp-registry/` (Windows), sinon
/// `$TMPDIR` ou `/tmp`. MCP Control lit ce dossier (même chemin) à chaque cycle.
#[must_use]
pub fn registry_dir() -> PathBuf {
    let mut base = std::env::temp_dir();
    if base.as_os_str().is_empty() {
        base = PathBuf::from("/tmp");
    }
    base.join("litemcp-registry")
}

/// Le nom peut-il servir de nom de fichier d'annonce en toute sécurité ?
///
/// Refuse le vide, la traversée (`..`), le point seul (`.` — le fichier
/// `..json` serait un résidu ambigu), les séparateurs (`/`, `\`) et `:` :
/// sous Windows, `PathBuf::join("C:nom.json")` **remplace la base** (chemin
/// drive-relatif) et `nom:flux.json` écrit un flux NTFS alternatif —
/// l'annonce sortirait du dossier de registre sans aucune erreur. Les noms
/// réservés MS-DOS (`CON`, `NUL`…) échouent silencieusement à l'écriture :
/// acceptable, l'annonce est une commodité et jamais une condition de
/// fonctionnement. La borne de longueur garde le nom final (nom + `.json`)
/// sous la limite de 255 octets des systèmes de fichiers usuels.
fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name.len() <= 100
        && !name.contains("..")
        && !name.contains(['/', '\\', ':'])
}

/// Écrit l'annonce `127.0.0.1:<port>` — **forme historique**, à ne réserver
/// qu'aux binds bouclage : préférez [`announce_at`], qui publie l'adresse
/// effective connectable et l'identité d'instance vérifiable par la sonde
/// anti-zombie (un serveur lié ailleurs qu'au bouclage y resterait sinon en
/// doute éternel). Retourne l'identité d'instance publiée — `0` si l'annonce
/// n'a pas pu être écrite (l'annonce est une commodité d'intégration, jamais
/// une condition de fonctionnement).
pub fn announce(name: &str, version: &str, port: u16) -> u64 {
    let instance = generate_instance_id(port);
    announce_at(
        name,
        version,
        SocketAddr::from(([127, 0, 0, 1], port)),
        instance,
    );
    instance
}

/// Écrit l'annonce `<nom>.json` après un bind réussi — `addr` = adresse
/// effective **connectable** du listener (jamais 0), `instance` = identité
/// d'instance non nulle exposée par `/health` (voir
/// `generate_instance_id`). Silencieux en cas d'échec : l'annonce est une
/// commodité d'intégration, pas une condition de fonctionnement du serveur.
/// L'écriture est sérialisée par le verrou du nom (voir `with_name_lock`)
/// : un retrait concurrent ne peut plus détruire l'annonce qui vient d'être
/// écrite.
pub fn announce_at(name: &str, version: &str, addr: SocketAddr, instance: u64) {
    if !is_safe_name(name) || addr.port() == 0 || instance == 0 {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    // `args_os` et non `args` : ce dernier **panique** sur un argument non
    // UTF-8 (argv exotique, wrapper système) — le contrat du module est de
    // ne jamais faire échouer le serveur pour une annonce.
    let args: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let doc = serde_json::json!({
        "name": name,
        "version": version,
        "addr": addr.to_string(),
        "port": addr.port(),
        "instance": instance,
        "pid": std::process::id(),
        "exe": exe.to_string_lossy(),
        "args": args,
        "cwd": cwd,
        "ts": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    });
    let dir = registry_dir();
    if create_private_dir(&dir).is_err() {
        return;
    }
    if let Ok(body) = serde_json::to_string_pretty(&doc) {
        with_name_lock(name, || {
            write_atomically(&dir.join(format!("{name}.json")), &body)
        });
    }
}

/// Identité d'instance — un `u64` non nul propre à CETTE instance liée :
/// publiée dans l'annonce (`instance`) et servie par `/health` (en-tête
/// `X-Instance-Id`) pour qu'une sonde puisse prouver quelle instance occupe
/// le port. Mélange (PID, port, nanos, séquence process) passé dans
/// SplitMix64 : deux instances — même process ou voisins démarrés à la même
/// nanoseconde — ne partagent jamais d'identité.
#[must_use]
pub(crate) fn generate_instance_id(port: u16) -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = INSTANCE_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut x = u64::from(std::process::id())
        ^ (u64::from(port) << 32)
        ^ nanos.rotate_left(17)
        ^ seq.rotate_left(47);
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31) | 1 // jamais 0 — 0 signifie « pas d'annonce »
}

/// Retire l'annonce `<nom>.json` — **forme publique** vérifiée par le seul
/// couple `(pid, port)` (le `Drop` de [`crate::http::BoundServer`] utilise
/// `retract_instance`, qui vérifie en plus l'identité). Le fichier n'est
/// supprimé que si son couple `(pid, port)` correspond **exactement** à
/// l'instance appelante : une autre instance du même nom (autre process
/// **ou** autre port — l'instance la plus récente a écrasé le fichier)
/// survit à notre arrêt. La décision et le retrait sont
/// **sérialisés par le verrou du nom** (voir `with_name_lock`) : entre la
/// lecture et la suppression, une instance plus récente ne peut plus écraser
/// le fichier — la course check-then-delete d'origine est fermée. Un
/// verrou non acquis (concurrent vivant) renonce : l'annonce reste, rejugée
/// plus tard — jamais de suppression sur une lecture périmée. Un fichier
/// absent, illisible ou corrompu est laissé en place : le lecteur (MCP
/// Control) a sa propre sonde de vie (POST MCP) pour trancher. Un arrêt
/// brutal (kill) ne passe pas par ici : le fichier reste, retiré par le
/// sweep dès qu'une autre instance prouve qu'elle occupe le port.
///
/// Cette forme ne dispose que de `(pid, port)` : deux instances successives
/// du **même process** sur le **même port** partagent ce couple (seule
/// l'identité d'instance les distingue). Le serveur lié utilise donc
/// `retract_instance` — un appelant manuel qui connaît l'identité devrait
/// faire de même ; `retract` reste inchangé (source-compatible).
pub fn retract(name: &str, port: u16) {
    retract_with(name, port, None);
}

/// Retrait vérifié par **identité d'instance** — utilisé par le `Drop` de
/// [`crate::http::BoundServer`] : le fichier n'est supprimé que si son
/// `instance` publié est celui de l'appelant (en plus du couple `(pid,
/// port)`). Un redémarrage dans le même process, sur le même port, a réécrit
/// l'annonce avec une autre identité : le retrait de l'ancienne instance ne
/// l'emporte pas — un port réutilisé ne prouve pas l'identité de
/// l'annonceur. Une annonce sans `instance` (format historique) reste jugée
/// sur `(pid, port)` : c'est la seule preuve disponible ; un champ `instance`
/// présent mais illisible (fichier altéré) est un doute et n'est **jamais**
/// supprimé.
pub(crate) fn retract_instance(name: &str, port: u16, instance: u64) {
    retract_with(name, port, Some(instance));
}

fn retract_with(name: &str, port: u16, instance: Option<u64>) {
    if !is_safe_name(name) {
        return;
    }
    with_name_lock(name, || {
        let path = registry_dir().join(format!("{name}.json"));
        let Ok(body) = fs::read_to_string(&path) else {
            return;
        };
        let Ok(doc) = serde_json::from_str::<serde_json::Value>(&body) else {
            return;
        };
        let same_pid = doc.get("pid").and_then(serde_json::Value::as_u64)
            == Some(u64::from(std::process::id()));
        let same_port =
            doc.get("port").and_then(serde_json::Value::as_u64) == Some(u64::from(port));
        // Une annonce sans identité (format historique) n'offre que
        // `(pid, port)` : tolérée telle quelle. Un champ `instance` présent
        // mais mal typé est un doute (fichier altéré) : pas de suppression.
        let same_instance = instance.is_none_or(|expected| match doc.get("instance") {
            None => true,
            Some(current) => current.as_u64() == Some(expected),
        });
        if same_pid && same_port && same_instance {
            let _ = fs::remove_file(&path);
        }
    });
}

/// Durée maximale d'attente du verrou d'un nom — les sections critiques
/// (écriture d'annonce, retrait vérifié, re-check + retrait de sweep) sont
/// de l'ordre de la milliseconde : au-delà, on renonce (best-effort).
const LOCK_ACQUIRE_TIMEOUT: Duration = Duration::from_millis(250);

/// Âge au-delà duquel un verrou tenu est **abandonné** (process tué pendant
/// sa section critique — ses fichiers ne sont jamais libérés par un kill) :
/// le premier contender le casse. La borne est très au-dessus de toute
/// section critique réelle (millisecondes) ; le contenu du registre n'est
/// jamais détruit par un verrou cassé (le contender reprend SA décision
/// depuis zéro).
const LOCK_STALE: Duration = Duration::from_secs(2);

/// Exécute `f` en maintenant le **verrou inter-processus du nom d'annonce**
/// — un dossier créé exclusivement (`create_dir` est atomique sur tous les
/// OS, sans aucune dépendance). Sérialise l'écriture d'annonce, le retrait
/// vérifié et le retrait du sweep pour un même nom : la course
/// check-then-delete d'origine (une annonce plus récente supprimée entre la
/// lecture et le retrait d'une instance plus ancienne) est fermée par
/// construction. `None` = verrou non acquis dans [`LOCK_ACQUIRE_TIMEOUT`]
/// (concurrent vivant) — l'appelant renonce, le fichier reste et sera rejugé.
/// Les verrous sont des **dossiers** `<nom>.lock` : ignorés par tous les
/// lecteurs (le sweep et MCP Control ne lisent que les `.json`).
pub(crate) fn with_name_lock<T>(name: &str, f: impl FnOnce() -> T) -> Option<T> {
    with_name_lock_for(name, LOCK_STALE, f)
}

/// Comme [`with_name_lock`], avec une borne de péremption explicite — pour
/// les tests (`stale_after` nul = verrou immédiatement cassable).
pub(crate) fn with_name_lock_for<T>(
    name: &str,
    stale_after: Duration,
    f: impl FnOnce() -> T,
) -> Option<T> {
    if !is_safe_name(name) {
        return None;
    }
    let lock = registry_dir().join(format!("{name}.lock"));
    let deadline = Instant::now() + LOCK_ACQUIRE_TIMEOUT;
    loop {
        match fs::create_dir(&lock) {
            Ok(()) => break, // verrou pris
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                // Tenu par un process vivant (section de l'ordre de la
                // milliseconde) ou abandonné (crash pendant la section) :
                // un verrou périmé est cassé, un verrou frais attendu. Une
                // cassé mais insupprimable (lecteur concurrent sur le
                // dossier) est traité comme occupé — la boucle reste bornée
                // par le délai d'acquisition dans tous les cas.
                if lock_is_stale(&lock, stale_after) && fs::remove_dir_all(&lock).is_ok() {
                    continue; // nouvelle tentative immédiate
                }
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(_) => return None, // dossier du registre absent ou illisible
        }
    }
    let result = f();
    // Libération best-effort : un résidu (fichier étranger planté dans le
    // dossier de verrou, crash au pire instant) sera cassé par âge.
    let _ = fs::remove_dir(&lock);
    Some(result)
}

/// Le verrou date-t-il d'au moins `stale_after` ? Une mtime dans le futur
/// (horloge recalée) compte comme fraîche : rien n'est cassé par erreur.
fn lock_is_stale(lock: &Path, stale_after: Duration) -> bool {
    fs::metadata(lock)
        .and_then(|meta| meta.modified())
        .map(|modified| {
            modified
                .elapsed()
                .map(|elapsed| elapsed >= stale_after)
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

/// Crée le dossier d'annonce, récursivement — sous Unix avec le mode `0700`
/// : le dossier publie exe, argv et cwd de ses occupants, il ne doit pas
/// être traversable par les autres utilisateurs.
#[cfg(unix)]
pub(crate) fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(dir)
}

#[cfg(not(unix))]
pub(crate) fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)
}

/// Ouvre `path` en création **exclusive** (`create_new` : échoue si le nom
/// existe déjà — un écrivain concurrent ne peut que nous faire échouer,
/// jamais détourner l'écriture vers un autre fichier). Sous Unix, le mode
/// `0600` est posé **à la création** : le contenu (exe, argv, cwd) n'est
/// lisible que par l'utilisateur du serveur, sans fenêtre de droits larges.
#[cfg(unix)]
fn open_exclusive(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_exclusive(path: &Path) -> std::io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// Écrit `body` dans `path` de façon **atomique**, sans suivre un éventuel
/// lien symbolique déjà planté à la destination.
///
/// Le dossier d'annonce est **partagé** (temp du système) : un process local
/// peut y créer des fichiers. Parades :
/// - le contenu part d'abord dans un fichier temporaire à nom
///   **imprévisible** (PID + nanos) créé exclusivement (voir
///   [`open_exclusive`]) — un reliquat de crash est retiré puis la création
///   repart (`remove_file` ne suit pas un symlink) ;
/// - juste avant le rename, le temporaire est re-vérifié : s'il a été
///   substitué par un lien symbolique entre-temps, il est jeté — le rename
///   ne déplacera jamais un lien ;
/// - `fs::rename` remplace la destination **sans la suivre** (il écrase le
///   lien lui-même, pas sa cible) et un lecteur concurrent ne voit jamais un
///   JSON partiel : l'ancien fichier ou le nouveau, jamais les deux mêlés.
fn write_atomically(path: &Path, body: &str) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(format!(".{}.{}.tmp", std::process::id(), nanos));
    let tmp = PathBuf::from(tmp_name);
    let mut file = match open_exclusive(&tmp) {
        Ok(file) => file,
        Err(_) => {
            // Reliquat (crash précédent) ou collision à la même nanoseconde :
            // `remove_file` retire un éventuel symlink sans suivre sa cible,
            // puis une seule nouvelle tentative exclusive part.
            let _ = fs::remove_file(&tmp);
            match open_exclusive(&tmp) {
                Ok(retry) => retry,
                Err(_) => return,
            }
        }
    };
    if file.write_all(body.as_bytes()).is_err() {
        let _ = fs::remove_file(&tmp);
        return;
    }
    drop(file);
    // Dernier garde TOCTOU : le temporaire ne doit pas avoir été substitué
    // par un lien symbolique entre l'écriture et le rename.
    match fs::symlink_metadata(&tmp) {
        Ok(meta) if meta.file_type().is_symlink() => {
            let _ = fs::remove_file(&tmp);
            return;
        }
        Ok(_) => {}
        Err(_) => return, // temporaire disparu : abandon silencieux
    }
    if fs::rename(&tmp, path).is_err() {
        let _ = fs::remove_file(&tmp);
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announce_et_retract_ecrivent_puis_suppriment_le_fichier() {
        let dir = registry_dir();
        create_private_dir(&dir).unwrap();
        let path = dir.join("litemcp-test-annonce.json");
        let _ = fs::remove_file(&path);
        announce("litemcp-test-annonce", "9.9.9", 8123);
        let body = fs::read_to_string(&path).expect("l'annonce doit exister");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["name"], "litemcp-test-annonce");
        assert_eq!(v["version"], "9.9.9");
        assert_eq!(v["port"], 8123);
        assert_eq!(v["pid"], std::process::id());
        assert!(v["exe"].as_str().unwrap().contains("litemcp"));
        assert!(!v["cwd"].as_str().unwrap().is_empty());
        // L'écriture atomique ne laisse pas de fichier temporaire derrière.
        let leftovers: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            !leftovers.iter().any(|f| f.contains(".tmp")),
            "aucun fichier temporaire ne doit subsister : {leftovers:?}"
        );
        retract("litemcp-test-annonce", 8123);
        assert!(!path.exists(), "retract doit supprimer sa propre annonce");
    }

    #[test]
    fn les_noms_dangereux_sont_refuses() {
        let dir = registry_dir();
        create_private_dir(&dir).unwrap();
        // Traversée de dossier, séparateurs, `:` (drive-relatif / flux NTFS
        // sous Windows : `PathBuf::join("C:x.json")` remplace la base) et le
        // point seul : aucun fichier ne doit être écrit, ni dans le registre
        // ni ailleurs.
        for name in ["../evasion", "a/b", "a\\b", "a:b", "C:evil", "", ".", ".."] {
            announce(name, "1", 8000);
            retract(name, 8000);
        }
        assert!(!dir.join("a").exists());
        assert!(!dir.join("C:evil.json").exists());
        assert!(!dir.join("..json").exists());
        // Windows : un chemin drive-relatif atterrirait dans le répertoire
        // courant — vérifier qu'il n'en reste rien.
        assert!(!PathBuf::from("evil.json").exists());
    }

    #[test]
    fn retract_ne_supprime_que_sa_propre_annonce() {
        let dir = registry_dir();
        create_private_dir(&dir).unwrap();
        let path = dir.join("litemcp-test-etranger.json");
        let _ = fs::remove_file(&path);
        // Annonce d'une AUTRE instance (pid fictif) du même nom : notre
        // retract ne doit pas l'effacer.
        let foreign = serde_json::json!({
            "name": "litemcp-test-etranger", "version": "1",
            "port": 1234, "pid": u64::from(u32::MAX - 1),
        });
        fs::write(&path, foreign.to_string()).unwrap();
        retract("litemcp-test-etranger", 1234);
        assert!(path.exists(), "l'annonce d'un autre process doit survivre");
        // Même process, port DIFFÉRENT (instance plus récente) : survit.
        let newer = serde_json::json!({
            "name": "litemcp-test-etranger", "version": "1",
            "port": 5678, "pid": std::process::id(),
        });
        fs::write(&path, newer.to_string()).unwrap();
        retract("litemcp-test-etranger", 1234);
        assert!(
            path.exists(),
            "l'annonce d'une autre instance doit survivre"
        );
        // Corrompu : laissé en place (le lecteur a sa sonde de vie).
        fs::write(&path, "{corrompu").unwrap();
        retract("litemcp-test-etranger", 1234);
        assert!(path.exists(), "un fichier corrompu n'est pas notre annonce");
        // Notre propre annonce (pid ET port du process courant) : supprimée.
        let ours = serde_json::json!({
            "name": "litemcp-test-etranger", "version": "1",
            "port": 1234, "pid": std::process::id(),
        });
        fs::write(&path, ours.to_string()).unwrap();
        retract("litemcp-test-etranger", 1234);
        assert!(!path.exists(), "notre annonce doit être supprimée");
    }

    #[test]
    fn port_zero_refuse() {
        let dir = registry_dir();
        create_private_dir(&dir).unwrap();
        let path = dir.join("litemcp-test-port0.json");
        let _ = fs::remove_file(&path);
        announce("litemcp-test-port0", "1", 0);
        assert!(!path.exists(), "port 0 = pas d'annonce");
    }

    #[test]
    fn announce_at_publie_l_adresse_et_l_identite() {
        let dir = registry_dir();
        create_private_dir(&dir).unwrap();
        let name = "litemcp-test-announce-at";
        let path = dir.join(format!("{name}.json"));
        let _ = fs::remove_file(&path);
        let addr: SocketAddr = "[::1]:23456".parse().unwrap();
        announce_at(name, "1.0.0", addr, 0x0123_4567_89AB_CDEF);
        let body = fs::read_to_string(&path).expect("l'annonce doit exister");
        let doc: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            doc["addr"], "[::1]:23456",
            "adresse effective publiée : {doc}"
        );
        assert_eq!(doc["port"], 23456);
        assert_eq!(doc["instance"], 0x0123_4567_89AB_CDEFu64);
        assert_eq!(doc["pid"], std::process::id());
        // instance nulle : jamais publiée (identité invérifiable = inutile).
        let _ = fs::remove_file(&path);
        announce_at(name, "1.0.0", addr, 0);
        assert!(!path.exists(), "instance 0 = pas d'annonce");
    }

    #[test]
    fn les_identites_d_instance_sont_uniques_et_non_nulles() {
        let ids: std::collections::HashSet<u64> =
            (0..1_000).map(|_| generate_instance_id(8080)).collect();
        assert_eq!(
            ids.len(),
            1_000,
            "deux instances ne partagent jamais d'identité"
        );
        assert!(
            ids.iter().all(|&id| id != 0),
            "0 = « pas d'annonce » : réservé"
        );
    }

    #[test]
    fn le_verrou_est_exclusif() {
        create_private_dir(&registry_dir()).unwrap();
        let acquired = with_name_lock_for("litemcp-test-verrou-exclusif", LOCK_STALE, || {
            // Verrou tenu et frais : un second acquire du même nom attend en
            // vain jusqu'au délai d'acquisition et renonce — jamais un accès
            // concurrent à la section critique.
            let nested = with_name_lock_for("litemcp-test-verrou-exclusif", LOCK_STALE, || 1);
            assert_eq!(nested, None, "verrou tenu : le second acquire renonce");
            2
        });
        assert_eq!(acquired, Some(2), "la section critique tourne sous verrou");
        assert_eq!(
            with_name_lock_for("litemcp-test-verrou-exclusif", LOCK_STALE, || 3),
            Some(3),
            "libéré : le suivant prend le verrou"
        );
    }

    #[test]
    fn retract_est_serialise_par_le_verrou_de_nom() {
        create_private_dir(&registry_dir()).unwrap();
        let name = "litemcp-test-retract-serie";
        let path = registry_dir().join(format!("{name}.json"));
        let _ = fs::remove_file(&path);
        let ours = serde_json::json!({
            "name": name, "version": "1", "port": 1234,
            "pid": std::process::id(),
        });
        fs::write(&path, ours.to_string()).unwrap();

        // Le verrou est tenu 300 ms (> LOCK_ACQUIRE_TIMEOUT) : le retrait
        // concurrent RENONCE — il ne détruit pas l'annonce sur une décision
        // prise pendant qu'un autre process occupait la section critique.
        let nom = name.to_string();
        let held = with_name_lock(name, || {
            let retire = std::thread::spawn(move || retract(&nom, 1234));
            std::thread::sleep(Duration::from_millis(300));
            retire.join().unwrap();
        });
        assert!(held.is_some());
        assert!(
            path.exists(),
            "verrou tenu : le retrait concurrent renonce sans détruire"
        );

        // Verrou libéré : le retrait exécute sa décision fraîche et détruit.
        retract(name, 1234);
        assert!(!path.exists(), "verrou libre : le retrait part normalement");
    }

    #[test]
    fn un_verrou_abandonne_est_casse_par_age() {
        create_private_dir(&registry_dir()).unwrap();
        let name = "litemcp-test-verrou-mort";
        let lock = registry_dir().join(format!("{name}.lock"));
        // Simule un process tué pendant sa section critique : le dossier de
        // verrou reste, personne ne le libérera jamais.
        fs::create_dir(&lock).unwrap();
        let result = with_name_lock_for(name, Duration::from_millis(0), || 42);
        assert_eq!(
            result,
            Some(42),
            "le verrou périmé est cassé, la section tourne"
        );
        assert!(!lock.exists(), "le verrou cassé repart propre");
    }
}
