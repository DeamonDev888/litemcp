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
//! `{ "name", "version", "port", "pid", "exe", "args", "cwd", "ts" }`
//! Un process par nom : le fichier est écrasé à chaque démarrage, l'instance
//! la plus récente gagne. `retract` ne le supprime que si le couple
//! `(pid, port)` qu'il contient est celui de l'instance appelante : ni
//! l'arrêt d'une instance plus ancienne (autre process ou autre port) ne
//! peut effacer l'annonce d'une instance vivante.
//!
//! ⚠️ Le fichier publie la ligne de commande (`args`) : ne passez pas de
//! secrets par argv — préférez les variables d'environnement ou un fichier
//! de configuration.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

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

/// Écrit l'annonce `<nom>.json` après un bind réussi (`port` = port effectif
/// du listener, jamais 0). Silencieux en cas d'échec : l'annonce est une
/// commodité d'intégration, pas une condition de fonctionnement du serveur.
pub fn announce(name: &str, version: &str, port: u16) {
    if !is_safe_name(name) || port == 0 {
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
        "port": port,
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
        write_atomically(&dir.join(format!("{name}.json")), &body);
    }
}

/// Retire l'annonce `<nom>.json` — appelé par le `Drop` de
/// [`crate::http::BoundServer`] (arrêt gracieux **ou** serveur lié jamais
/// servi). Le fichier n'est supprimé que si son couple `(pid, port)`
/// correspond **exactement** à l'instance appelante : une autre instance du
/// même nom (autre process **ou** autre port — l'instance la plus récente a
/// écrasé le fichier) survit à notre arrêt. Un fichier absent, illisible ou
/// corrompu est laissé en place : le lecteur (MCP Control) a sa propre sonde
/// de vie (POST MCP) pour trancher. Un arrêt brutal (kill) ne passe pas par
/// ici : le fichier reste, traité par le lecteur comme l'annonce d'un
/// process peut-être mort.
pub fn retract(name: &str, port: u16) {
    if !is_safe_name(name) {
        return;
    }
    let path = registry_dir().join(format!("{name}.json"));
    let Ok(body) = fs::read_to_string(&path) else {
        return;
    };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(&body) else {
        return;
    };
    let same_pid =
        doc.get("pid").and_then(serde_json::Value::as_u64) == Some(u64::from(std::process::id()));
    let same_port = doc.get("port").and_then(serde_json::Value::as_u64) == Some(u64::from(port));
    if same_pid && same_port {
        let _ = fs::remove_file(&path);
    }
}

/// Crée le dossier d'annonce, récursivement — sous Unix avec le mode `0700`
/// : le dossier publie exe, argv et cwd de ses occupants, il ne doit pas
/// être traversable par les autres utilisateurs.
#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
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
}
