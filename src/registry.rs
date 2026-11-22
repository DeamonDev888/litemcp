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
//!
//! Le dossier partagé est **validé à chaque usage** (répertoire réel — un lien
//! symbolique, ou une jonction Windows, est refusé —, mode exactement `0700`,
//! écriture prouvée par une sonde d'appartenance) : un dossier pré-créé
//! `0777` par un autre utilisateur local, ou remplacé par un lien symbolique,
//! n'est jamais utilisé — l'annonce est alors abandonnée en silence, comme
//! tout échec d'écriture ; un dossier à nous mais trop ouvert est resserré en
//! `0700`. La validation s'applique aussi **à la lecture** : le sweep
//! anti-zombie valide le dossier avant de le parcourir (voir
//! [`crate::antizombie::sweep_registry`]).
//! ⚠️ Le fichier publie la ligne de commande (`args`) : ne passez pas de
//! secrets par argv — préférez les variables d'environnement ou un fichier
//! de configuration.

use std::fs;
use std::io::{self, Write as _};
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
///
/// `pub(crate)` : le sweep anti-zombie l'utilise pour reconnaître LE nommage
/// d'annonce — `sweep_dir` ne détruit que les fichiers qui y correspondent
/// (voir [`crate::antizombie::sweep_dir`]).
pub(crate) fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name.len() <= 100
        && !name.contains("..")
        && !name.contains(['/', '\\', ':'])
}

/// Suffixe **aléatoire** de 16 hexadécimaux (8 octets du CSPRNG du système,
/// `getrandom`) — nommage de la sonde d'appartenance et du fichier temporaire
/// d'écriture. Ce n'est **pas** un secret (rien de confidentiel ne porte ce
/// nom) : l'aléa empêche seulement qu'un voisin local devine le nom et le
/// pré-crée pour faire échouer la validation ou détourner l'écriture. Repli
/// sur `None` quand la source d'entropie est indisponible : l'appelant garde
/// alors son nommage historique (PID + séquence ou horloge), moins résistant
/// mais fonctionnel — une annonce ne doit pas dépendre de l'entropie.
fn random_suffix() -> Option<String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).ok()?;
    Some(format!("{:016x}", u64::from_be_bytes(bytes)))
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
///
/// Forme **historique** : l'annonce ne dit rien de l'authentification
/// (équivalent à [`announce_at_with`] avec `auth_required = false` et aucune
/// empreinte). Un serveur qui active l'autorisation publie la variante
/// complète.
pub fn announce_at(name: &str, version: &str, addr: SocketAddr, instance: u64) {
    announce_at_with(name, version, addr, instance, false, None);
}

/// Écrit l'annonce avec l'état d'authentification — voir [`announce_at`].
///
/// `auth_required` devient `"authRequired"` (booléen) et
/// `certificate_fingerprint` — l'empreinte TLS du certificat servi
/// (`sha256:` + hex) — devient `"certificateFingerprint"` quand elle existe.
/// **Aucun secret** n'est publié : une empreinte de certificat public est une
/// identité, pas une clé ; le serveur n'a du reste aucune clé privée
/// d'authentification (le modèle est « le client signe », `docs/auth.md` §1).
pub fn announce_at_with(
    name: &str,
    version: &str,
    addr: SocketAddr,
    instance: u64,
    auth_required: bool,
    certificate_fingerprint: Option<&str>,
) {
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
    let mut doc = serde_json::json!({
        "name": name,
        "version": version,
        "addr": addr.to_string(),
        "port": addr.port(),
        "instance": instance,
        "authRequired": auth_required,
        "pid": std::process::id(),
        "exe": exe.to_string_lossy(),
        "args": args,
        "cwd": cwd,
        "ts": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    });
    if let (Some(fingerprint), Some(object)) = (certificate_fingerprint, doc.as_object_mut()) {
        object.insert(
            "certificateFingerprint".to_string(),
            serde_json::Value::String(fingerprint.to_string()),
        );
    }
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
/// (concurrent vivant) **ou** dossier de registre non validé (absent, trop
/// largement partagé, d'un autre utilisateur — voir
/// [`validate_private_dir`]) — l'appelant renonce, le fichier reste et sera
/// rejugé.
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
    // Le dossier partagé est la **seule porte d'entrée** du registre : il
    // est validé avant tout verrou (répertoire réel, privé, à nous — voir
    // [`validate_private_dir`]). Absent, hostile ou détourné : `None`, comme
    // un verrou non acquis — aucune section critique ne tourne sous un
    // verrou qu'un autre utilisateur local contrôle.
    let dir = registry_dir();
    if validate_private_dir(&dir).is_err() {
        return None;
    }
    let lock = dir.join(format!("{name}.lock"));
    // `checked_add` : une constante de péremption démesurée ne peut pas
    // paniquer ; `None` = aucune échéance (l'attente reste bornée par les
    // verrous frais, jamais par un `+` qui déborde).
    let deadline = Instant::now().checked_add(LOCK_ACQUIRE_TIMEOUT);
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
                if deadline.is_some_and(|echeance| Instant::now() >= echeance) {
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
///
/// **La création ne prouve pas la privauté** : `DirBuilder::recursive(true)`
/// rend `Ok(())` sans rien faire quand le chemin existe déjà, et le dossier
/// vit dans le temp **partagé**, où un autre utilisateur local peut l'avoir
/// pré-créé (`0777`) ou avoir planté un lien symbolique à sa place. Le mode
/// `0700` ne protège que la création ; le dossier est donc validé après
/// coup par [`validate_private_dir`] — refus d'un dossier qu'un autre
/// contrôle, resserrement d'un dossier à nous qui s'écarte de `0700` (trop
/// ouvert aux autres, ou `0300` : sans bit de lecture, l'annonce serait
/// invisible à son lecteur).
#[cfg(unix)]
pub(crate) fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(dir)?;
    validate_private_dir(dir)
}

/// Variante non-Unix : `create_dir_all` (comportement inchangé), puis
/// vérification que le chemin est bien un **répertoire réel**.
#[cfg(not(unix))]
pub(crate) fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    validate_private_dir(dir)
}

/// Le dossier d'annonce est-il utilisable **en privé** ? Répertoire réel,
/// écriture effectivement possible pour nous sous **notre** identité, et mode
/// exactement `0700` (Unix : bits propriétaire complets, aucun bit de
/// groupe/autres).
///
/// `symlink_metadata` et non `metadata` : un lien symbolique planté au
/// chemin du registre est refusé, jamais suivi — sinon un autre utilisateur
/// local redirigerait lectures et écritures vers un dossier qu'il contrôle,
/// en laissant croire au mode du répertoire d'arrivée.
///
/// **Contrôle de propriété sans `libc`** : le crate n'a pas `libc` et
/// n'ajoutera aucun `unsafe`. `MetadataExt::uid()` donne le propriétaire du
/// dossier mais pas l'euid du process ; un dossier `0700` appartenant à un
/// autre utilisateur refuse l'écriture, donc une sonde créée en
/// `create_new` **dans** le dossier échoue — c'est le contrôle effectif.
/// Quand elle réussit, l'uid de la sonde doit être celui du dossier (un
/// dossier où l'on écrit sous une autre identité n'est pas privé). La
/// sonde (`.probe-<pid>-<aléa>`) est supprimée **au mieux** : un échec de
/// suppression n'invalide pas le verdict et ne laisse au pire qu'un fichier
/// inoffensif, qu'aucun lecteur ne prend pour une annonce.
///
/// **Mode** : un dossier à nous qui s'écarte de `0700` est **resserré**, pas
/// refusé — on répare ce qui nous appartient. Trop ouvert (`0755`, `0777`),
/// il publie exe, argv et cwd de ses occupants. Trop fermé pour ses propres
/// lecteurs (`0300` : la sonde et l'écriture réussissent, mais MCP Control,
/// même utilisateur, ne peut pas `read_dir` — l'annonce serait invisible),
/// il priverait l'intégration de l'annonce sans aucune erreur. Un dossier
/// d'un autre utilisateur ne peut pas emprunter ce chemin : la sonde
/// échouerait ou son uid différerait.
///
/// Erreurs : `AlreadyExists` si le chemin existe sans être un répertoire
/// réel (lien symbolique, jonction Windows, fichier) ; `PermissionDenied` si
/// la sonde est refusée faute de droits, si son uid n'est pas celui du
/// dossier, ou si le mode reste différent de `0700` après resserrement ; les
/// autres échecs d'E/S sont rendus tels quels (kind conservé).
#[cfg(unix)]
pub(crate) fn validate_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let meta = fs::symlink_metadata(dir)?;
    let file_type = meta.file_type();
    if file_type.is_symlink() || !file_type.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "{} n'est pas un répertoire réel (lien symbolique ou fichier)",
                dir.display()
            ),
        ));
    }
    // La propriété se prouve **avant** le mode : c'est elle qui distingue un
    // dossier que nous avons créé (mode à resserrer) d'un dossier pré-créé
    // par un autre utilisateur local (à refuser, quel que soit son mode).
    probe_ownership(dir, meta.uid())?;
    if meta.permissions().mode() & 0o777 == 0o700 {
        return Ok(());
    }
    // Dossier à nous, mais pas exactement `0700` : trop ouvert (`0755`,
    // `0777`), il publie exe, argv et cwd des occupants ; trop fermé (`0300`),
    // il rend l'annonce invisible à MCP Control (même utilisateur, mais sans
    // bit de lecture). Dans les deux cas, on resserre en `0700` : le refus
    // silencieux priverait l'intégration de l'annonce sans que rien ne le
    // signale. Un dossier **étranger** ne peut pas arriver ici : la sonde
    // aurait échoué ou son uid différerait.
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "{} n'a pas pu être resserré en 0700 : {error}",
                dir.display()
            ),
        )
    })?;
    let mode = fs::symlink_metadata(dir)?.permissions().mode() & 0o777;
    if mode != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} n'est pas en 0700 après resserrement (mode {:04o}) : registre non privé",
                dir.display(),
                mode
            ),
        ));
    }
    Ok(())
}

/// Variante non-Unix : le chemin doit être un **répertoire réel** (jamais un
/// lien symbolique, une **jonction** ou un fichier).
///
/// Sous Windows, un **point d'analyse** (`FILE_ATTRIBUTE_REPARSE_POINT` :
/// jonction de répertoire, lien symbolique, point de montage) est refusé
/// explicitement. Mesure sur la toolchain 1.85 (`symlink_metadata` d'une
/// jonction créée par `mklink /J`) : `is_symlink()` vaut déjà `true` et
/// `is_dir()` `false` — le contrôle `is_symlink()/!is_dir()` la refusait donc
/// déjà. Le test explicite du bit rend le refus **indépendant de la
/// classification de `FileType`** (autres tags de reparse point, évolution
/// de la std) ; `metadata`, qui suit la redirection, la verrait comme un
/// répertoire — d'où `symlink_metadata`. Le bit est lu par
/// `MetadataExt::file_attributes`, sans `unsafe` ni dépendance.
///
/// **Limite restante** : le contrôle d'ACL et de propriétaire n'est pas fait
/// (une ACL Windows ne se résume pas à un bit de mode, et `%TEMP%` est un
/// dossier par utilisateur). Un `TEMP` pointé sur un dossier partagé reste
/// donc hors du modèle de menace : le registre y accepte un dossier réel
/// créé par un autre utilisateur.
#[cfg(not(unix))]
pub(crate) fn validate_private_dir(dir: &Path) -> std::io::Result<()> {
    let meta = fs::symlink_metadata(dir)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        // `FILE_ATTRIBUTE_REPARSE_POINT` — tout point d'analyse (jonction de
        // répertoire, lien symbolique, point de montage de volume) est
        // refusé : le répertoire du registre doit être l'objet lui-même.
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "{} n'est pas un répertoire réel (point d'analyse ou jonction)",
                    dir.display()
                ),
            ));
        }
    }
    let file_type = meta.file_type();
    if file_type.is_symlink() || !file_type.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "{} n'est pas un répertoire réel (lien symbolique ou fichier)",
                dir.display()
            ),
        ));
    }
    Ok(())
}

/// Sonde d'appartenance — voir [`validate_private_dir`] : `create_new` d'un
/// fichier `.probe-<pid>-<aléa>` dans `dir`, uid comparé à celui du dossier,
/// fichier retiré au mieux. Le suffixe vient du CSPRNG de l'OS (voir
/// [`random_suffix`]) : un voisin local ne peut pas deviner le nom pour le
/// pré-créer (faire échouer la validation) ni le remplacer pendant la
/// mesure. Ce n'est pas un secret, seulement un nom non devinable.
#[cfg(unix)]
fn probe_ownership(dir: &Path, owner: u32) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    static PROBE_SEQ: AtomicU64 = AtomicU64::new(0);
    let mut probe = None;
    let mut last_error = None;
    // Un nom déjà pris (résidu d'une validation tuée, collision improbable
    // des 8 octets aléatoires) est contourné par un nouveau tirage. Sans
    // entropie disponible, le repli reprend PID + séquence (nommage
    // historique) : la validation reste fonctionnelle.
    for _ in 0..4 {
        let suffix = random_suffix()
            .unwrap_or_else(|| PROBE_SEQ.fetch_add(1, Ordering::Relaxed).to_string());
        let path = dir.join(format!(".probe-{}-{suffix}", std::process::id()));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => {
                drop(file);
                probe = Some(path);
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => last_error = Some(error),
            Err(error) => {
                // Dossier appartenant à un autre utilisateur (0700 : écriture
                // refusée) ou système de fichiers en lecture seule : refus,
                // jamais d'usage d'un dossier qu'on ne prouve pas posséder.
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "sonde d'écriture refusée dans {} (dossier d'un autre utilisateur ?) : {error}",
                        dir.display()
                    ),
                ));
            }
        }
    }
    let Some(probe) = probe else {
        let error = last_error.unwrap_or_else(|| io::Error::other("sonde impossible"));
        return Err(io::Error::new(
            error.kind(),
            format!(
                "sonde d'écriture impossible dans {} : {error}",
                dir.display()
            ),
        ));
    };
    // `symlink_metadata` : une substitution de la sonde par un lien ne fait
    // pas lire la cible ; l'uid lu est celui de l'objet réellement créé.
    let probe_uid = fs::symlink_metadata(&probe).map(|meta| meta.uid());
    // Nettoyage **au mieux** : un échec (droits, verrou antivirus) n'invalide
    // pas le verdict — il ne reste qu'un fichier vide au nom hors nommage
    // d'annonce, qu'aucun lecteur ne prend pour une annonce et que le sweep
    // ignore.
    let _ = fs::remove_file(&probe);
    match probe_uid {
        Ok(uid) if uid == owner => Ok(()),
        Ok(uid) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} : la sonde appartient à l'uid {uid}, pas au propriétaire du dossier ({owner})",
                dir.display()
            ),
        )),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!(
                "sonde d'écriture illisible dans {} : {error}",
                dir.display()
            ),
        )),
    }
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

/// Écrit `body` dans `path` de façon **atomique** pour les lecteurs.
///
/// Le dossier d'annonce est **partagé** (temp du système) : un process local
/// peut y créer des fichiers. Parades, et ce qu'elles garantissent vraiment :
/// - le contenu part d'abord dans un fichier temporaire au nom **aléatoire**
///   (`<nom>.json.<pid>.<aléa>.tmp`, 8 octets du CSPRNG de l'OS) créé
///   exclusivement (voir [`open_exclusive`]) : un nom devinable ne peut pas
///   être pré-créé pour faire échouer l'écriture ; ce n'est pas un secret ;
/// - le temporaire est re-vérifié juste avant le rename : un lien substitué
///   est jeté. Ce contrôle **n'est pas atomique** — une substitution entre
///   la vérification et `fs::rename` reste possible (TOCTOU sur le dossier
///   parent), et un lien planté au nom du temporaire serait alors renommé
///   tel quel. Fermer cette fenêtre demande `openat`/`renameat2`, hors de
///   portée sans `unsafe` ni dépendance ; elle n'est pas revendiquée ici ;
/// - `create_new` empêche d'**écrire dans** un fichier existant (`O_EXCL`),
///   et `fs::rename` remplace l'entrée de destination **sans la suivre** :
///   un lien déjà planté à `<nom>.json` est écrasé, pas sa cible. Un lecteur
///   concurrent ne voit jamais un JSON partiel : l'ancien fichier ou le
///   nouveau, jamais les deux mêlés.
fn write_atomically(path: &Path, body: &str) {
    // Nom aléatoire (8 octets du CSPRNG de l'OS) : un voisin local ne peut ni
    // pré-créer le temporaire pour faire échouer l'écriture ni le deviner
    // pour le remplacer. Sans entropie disponible, repli sur l'horloge (nom
    // historique) — l'atomicité ne dépend pas du nom.
    let suffix = random_suffix().unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos().to_string())
            .unwrap_or_else(|_| "0".to_string())
    });
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(format!(".{}.{suffix}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp_name);
    let mut file = match open_exclusive(&tmp) {
        Ok(file) => file,
        Err(_) => {
            // Reliquat (crash précédent) ou collision improbable du suffixe :
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
    // Garde **au mieux** avant le rename : réduit la fenêtre, ne la ferme pas
    // (voir la doc : sans `openat`, la vérification et le rename ne sont pas
    // atomiques). Un lien détecté ici est jeté.
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

    /// Nom d'annonce propre à CE processus et à CE test. Le registre est
    /// **global** (`%TEMP%/litemcp-registry`) : deux `cargo test` simultanés
    /// écriraient le même `<nom>.json` (et le même `<nom>.lock`) — l'un
    /// écraserait ou verrouillerait l'annonce de l'autre. `PID` sépare les
    /// processus, le compteur les tests d'un même processus (ils tournent en
    /// parallèle). Calculé une seule fois par test, puis réutilisé tel quel.
    fn nom_unique(base: &str) -> String {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        format!("{base}-{}-{seq}", std::process::id())
    }

    #[test]
    fn announce_et_retract_ecrivent_puis_suppriment_le_fichier() {
        let dir = registry_dir();
        create_private_dir(&dir).unwrap();
        let nom = nom_unique("litemcp-test-annonce");
        let path = dir.join(format!("{nom}.json"));
        let _ = fs::remove_file(&path);
        announce(&nom, "9.9.9", 8123);
        let body = fs::read_to_string(&path).expect("l'annonce doit exister");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["name"], nom.as_str());
        assert_eq!(v["version"], "9.9.9");
        assert_eq!(v["port"], 8123);
        assert_eq!(v["pid"], std::process::id());
        assert!(v["exe"].as_str().unwrap().contains("litemcp"));
        assert!(!v["cwd"].as_str().unwrap().is_empty());
        // L'écriture atomique ne laisse pas de fichier temporaire derrière —
        // pour CETTE annonce : le dossier de registre est partagé et un test
        // parallèle peut y avoir un tmp en vol au moment de la lecture (le
        // scan global rendait le test instable sans rien prouver de plus).
        let prefixe = format!("{nom}.json.");
        let leftovers: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|fichier| fichier.starts_with(&prefixe) && fichier.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "aucun fichier temporaire ne doit subsister : {leftovers:?}"
        );
        retract(&nom, 8123);
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
        let nom = nom_unique("litemcp-test-etranger");
        let path = dir.join(format!("{nom}.json"));
        let _ = fs::remove_file(&path);
        // Annonce d'une AUTRE instance (pid fictif) du même nom : notre
        // retract ne doit pas l'effacer.
        let foreign = serde_json::json!({
            "name": nom.as_str(), "version": "1",
            "port": 1234, "pid": u64::from(u32::MAX - 1),
        });
        fs::write(&path, foreign.to_string()).unwrap();
        retract(&nom, 1234);
        assert!(path.exists(), "l'annonce d'un autre process doit survivre");
        // Même process, port DIFFÉRENT (instance plus récente) : survit.
        let newer = serde_json::json!({
            "name": nom.as_str(), "version": "1",
            "port": 5678, "pid": std::process::id(),
        });
        fs::write(&path, newer.to_string()).unwrap();
        retract(&nom, 1234);
        assert!(
            path.exists(),
            "l'annonce d'une autre instance doit survivre"
        );
        // Corrompu : laissé en place (le lecteur a sa sonde de vie).
        fs::write(&path, "{corrompu").unwrap();
        retract(&nom, 1234);
        assert!(path.exists(), "un fichier corrompu n'est pas notre annonce");
        // Notre propre annonce (pid ET port du process courant) : supprimée.
        let ours = serde_json::json!({
            "name": nom.as_str(), "version": "1",
            "port": 1234, "pid": std::process::id(),
        });
        fs::write(&path, ours.to_string()).unwrap();
        retract(&nom, 1234);
        assert!(!path.exists(), "notre annonce doit être supprimée");
    }

    #[test]
    fn port_zero_refuse() {
        let dir = registry_dir();
        create_private_dir(&dir).unwrap();
        let nom = nom_unique("litemcp-test-port0");
        let path = dir.join(format!("{nom}.json"));
        let _ = fs::remove_file(&path);
        announce(&nom, "1", 0);
        assert!(!path.exists(), "port 0 = pas d'annonce");
    }

    #[test]
    fn announce_at_publie_l_adresse_et_l_identite() {
        let dir = registry_dir();
        create_private_dir(&dir).unwrap();
        let name = nom_unique("litemcp-test-announce-at");
        let path = dir.join(format!("{name}.json"));
        let _ = fs::remove_file(&path);
        let addr: SocketAddr = "[::1]:23456".parse().unwrap();
        announce_at(&name, "1.0.0", addr, 0x0123_4567_89AB_CDEF);
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
        announce_at(&name, "1.0.0", addr, 0);
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
        let nom = nom_unique("litemcp-test-verrou-exclusif");
        let acquired = with_name_lock_for(&nom, LOCK_STALE, || {
            // Verrou tenu et frais : un second acquire du même nom attend en
            // vain jusqu'au délai d'acquisition et renonce — jamais un accès
            // concurrent à la section critique.
            let nested = with_name_lock_for(&nom, LOCK_STALE, || 1);
            assert_eq!(nested, None, "verrou tenu : le second acquire renonce");
            2
        });
        assert_eq!(acquired, Some(2), "la section critique tourne sous verrou");
        assert_eq!(
            with_name_lock_for(&nom, LOCK_STALE, || 3),
            Some(3),
            "libéré : le suivant prend le verrou"
        );
    }

    #[test]
    fn retract_est_serialise_par_le_verrou_de_nom() {
        create_private_dir(&registry_dir()).unwrap();
        let name = nom_unique("litemcp-test-retract-serie");
        let path = registry_dir().join(format!("{name}.json"));
        let _ = fs::remove_file(&path);
        let ours = serde_json::json!({
            "name": name.as_str(), "version": "1", "port": 1234,
            "pid": std::process::id(),
        });
        fs::write(&path, ours.to_string()).unwrap();

        // Le verrou est tenu 300 ms (> LOCK_ACQUIRE_TIMEOUT) : le retrait
        // concurrent RENONCE — il ne détruit pas l'annonce sur une décision
        // prise pendant qu'un autre process occupait la section critique.
        let nom = name.to_string();
        let held = with_name_lock(&name, || {
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
        retract(&name, 1234);
        assert!(!path.exists(), "verrou libre : le retrait part normalement");
    }

    #[test]
    fn un_verrou_abandonne_est_casse_par_age() {
        create_private_dir(&registry_dir()).unwrap();
        let name = nom_unique("litemcp-test-verrou-mort");
        let lock = registry_dir().join(format!("{name}.lock"));
        // Simule un process tué pendant sa section critique : le dossier de
        // verrou reste, personne ne le libérera jamais.
        fs::create_dir(&lock).unwrap();
        let result = with_name_lock_for(&name, Duration::from_millis(0), || 42);
        assert_eq!(
            result,
            Some(42),
            "le verrou périmé est cassé, la section tourne"
        );
        assert!(!lock.exists(), "le verrou cassé repart propre");
    }

    /// Le registre vit dans le temp **partagé** : la validation du dossier est
    /// testée sur des dossiers scratch, jamais sur le dossier réel ni via
    /// `TMPDIR` (aucune mutation de l'environnement — les tests d'un même
    /// processus tournent en parallèle).
    #[cfg(unix)]
    mod dossier_prive {
        use super::*;
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

        /// Chemin scratch propre à un test : supprimé avant et après usage.
        fn scratch(tag: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "litemcp-test-dirprive-{tag}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            let _ = fs::remove_file(&dir);
            dir
        }

        fn poser_mode(dir: &Path, mode: u32) {
            fs::set_permissions(dir, fs::Permissions::from_mode(mode)).unwrap();
        }

        /// La sonde d'appartenance ne doit pas subsister dans le chemin
        /// nominal : le retrait est fait **au mieux** (best effort, aucune
        /// panique s'il échoue), et ces cas-là ne doivent laisser aucun
        /// résidu.
        fn sans_residu_sonde(dir: &Path) {
            let residus: Vec<String> = fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|nom| nom.starts_with(".probe-"))
                .collect();
            assert!(residus.is_empty(), "sondes résiduelles : {residus:?}");
        }

        /// Un dossier à nous mais trop ouvert (`0777`, créé par un appelant
        /// antérieur ou par `create_dir_all` qui applique l'umask) est
        /// **resserré** en `0700` plutôt que refusé : le registre publie exe,
        /// argv et cwd, et un refus silencieux priverait MCP Control de
        /// l'annonce sans que rien ne le signale. Un dossier d'un **autre**
        /// utilisateur, lui, ne peut pas emprunter ce chemin : la sonde
        /// échouerait (0700) ou son uid différerait (0777).
        #[test]
        fn un_dossier_preexistant_ouvert_aux_autres_est_resserre() {
            let dir = scratch("0777");
            fs::DirBuilder::new().create(&dir).unwrap();
            poser_mode(&dir, 0o777);
            create_private_dir(&dir).expect("dossier à nous : resserré, pas refusé");
            assert_eq!(
                fs::symlink_metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700,
                "le mode trop large est corrigé en 0700"
            );
            sans_residu_sonde(&dir);
            let _ = fs::remove_dir_all(&dir);
        }

        /// Un dossier à nous en `0300` (écriture + traversée, **sans
        /// lecture**) passait la validation (aucun bit de groupe/autres) : la
        /// sonde et l'écriture réussissaient, mais MCP Control — même
        /// utilisateur — ne pouvait pas `read_dir` : l'annonce était invisible,
        /// sans erreur. Le dossier est désormais resserré en `0700`, comme un
        /// dossier trop ouvert : on répare ce qui nous appartient.
        #[test]
        fn un_dossier_0300_a_nous_est_resserre_en_0700() {
            let dir = scratch("0300");
            fs::DirBuilder::new().create(&dir).unwrap();
            poser_mode(&dir, 0o300);
            create_private_dir(&dir).expect("dossier à nous : resserré, pas refusé");
            let mode = fs::symlink_metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o700,
                "les bits de lecture du propriétaire sont rétablis (mode {mode:04o})"
            );
            sans_residu_sonde(&dir);
            let _ = fs::remove_dir_all(&dir);
        }

        /// Un dossier dont la sonde ne peut pas prouver la propriété est
        /// refusé : mode déjà privé d'un autre utilisateur (`0700` étranger),
        /// l'écriture de la sonde est refusée par le système.
        ///
        /// Ignoré sous `root` : l'euid 0 traverse les bits de mode, aucune
        /// écriture ne peut être refusée de cette façon (le contrôle reste
        /// vrai pour un serveur non privilégié, le cas visé).
        #[test]
        fn un_dossier_non_inscriptible_est_refuse() {
            let dir = scratch("etroit");
            fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
            // Le dossier vient d'être créé par nous : son uid **est** notre
            // euid. Pas de `libc` pour `geteuid`, et aucun `unsafe`.
            if fs::metadata(&dir).unwrap().uid() == 0 {
                let _ = fs::remove_dir_all(&dir);
                return;
            }
            poser_mode(&dir, 0o500);
            let error = create_private_dir(&dir).expect_err("dossier non inscriptible : refus");
            assert!(
                matches!(
                    error.kind(),
                    io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem
                ),
                "{error}"
            );
            poser_mode(&dir, 0o700);
            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn un_lien_symbolique_vers_un_dossier_est_refuse() {
            let cible = scratch("cible");
            fs::DirBuilder::new().mode(0o700).create(&cible).unwrap();
            let lien = scratch("lien");
            std::os::unix::fs::symlink(&cible, &lien).unwrap();
            // `create_dir_all` réussit en silence sur un lien vers un
            // dossier : c'est exactement l'angle mort que la validation
            // ferme.
            let error = create_private_dir(&lien).expect_err("lien : refus, jamais suivi");
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error}");
            assert!(
                fs::symlink_metadata(&lien)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "le lien n'est pas remplacé"
            );
            let _ = fs::remove_file(&lien);
            let _ = fs::remove_dir_all(&cible);
        }

        #[test]
        fn un_fichier_regulier_au_chemin_est_refuse() {
            let dir = scratch("fichier");
            fs::write(&dir, b"pas un dossier").unwrap();
            let error = create_private_dir(&dir).expect_err("un fichier n'est pas un dossier");
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error}");
            assert!(fs::symlink_metadata(&dir).unwrap().file_type().is_file());
            let _ = fs::remove_file(&dir);
        }

        #[test]
        fn un_dossier_absent_est_cree_en_0700() {
            let dir = scratch("absent");
            create_private_dir(&dir).expect("création 0700");
            let meta = fs::symlink_metadata(&dir).unwrap();
            assert!(meta.file_type().is_dir(), "un répertoire réel est créé");
            assert_eq!(
                meta.permissions().mode() & 0o077,
                0,
                "aucun bit de groupe/autres (mode {:04o})",
                meta.permissions().mode() & 0o777
            );
            assert_eq!(meta.permissions().mode() & 0o700, 0o700);
            sans_residu_sonde(&dir);
            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn un_dossier_0700_a_nous_est_accepte() {
            let dir = scratch("0700");
            fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
            poser_mode(&dir, 0o700);
            create_private_dir(&dir).expect("dossier privé et à nous : accepté");
            sans_residu_sonde(&dir);
            let _ = fs::remove_dir_all(&dir);
        }
    }

    /// Windows : `create_dir_all` inchangé, mais le chemin doit être un
    /// répertoire réel — un fichier n'est jamais pris pour le registre.
    #[cfg(not(unix))]
    #[test]
    fn le_chemin_du_registre_doit_etre_un_repertoire() {
        let dir = std::env::temp_dir().join(format!(
            "litemcp-test-dirprive-fichier-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_file(&dir);
        fs::write(&dir, b"pas un dossier").unwrap();
        assert!(
            create_private_dir(&dir).is_err(),
            "un fichier au chemin du registre est refusé"
        );
        let _ = fs::remove_file(&dir);
        create_private_dir(&dir).expect("dossier absent : créé");
        assert!(fs::symlink_metadata(&dir).unwrap().file_type().is_dir());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Sous Windows, une **jonction** est un point d'analyse : `read_dir` et
    /// les écritures la suivent. Elle est refusée par le contrôle explicite
    /// `FILE_ATTRIBUTE_REPARSE_POINT` (mesure 1.85 : `symlink_metadata` la
    /// rend déjà `is_symlink() == true`, mais le refus ne doit pas dépendre
    /// de cette classification).
    ///
    /// `mklink /J` ne demande pas de privilège particulier (contrairement aux
    /// liens symboliques) ; si la création échoue (stratégie, volume), le cas
    /// n'est pas couvrable ici : test auto-ignoré.
    #[cfg(windows)]
    #[test]
    fn une_jonction_est_refusee() {
        let cible = std::env::temp_dir().join(format!(
            "litemcp-test-jonction-cible-{}",
            std::process::id()
        ));
        let lien =
            std::env::temp_dir().join(format!("litemcp-test-jonction-lien-{}", std::process::id()));
        let _ = fs::remove_dir_all(&cible);
        let _ = fs::remove_dir(&lien);
        fs::create_dir_all(&cible).unwrap();
        let created = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&lien)
            .arg(&cible)
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !created {
            let _ = fs::remove_dir_all(&cible);
            return;
        }
        let error = create_private_dir(&lien).expect_err("jonction : refus, jamais suivie");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error}");
        // Le refus vient bien du contrôle explicite des points d'analyse, pas
        // seulement de `!is_dir()` : `symlink_metadata` d'une jonction rend
        // déjà `is_dir() == false` sur la toolchain 1.85 (mesuré), mais le
        // message distingue la branche.
        assert!(
            error.to_string().contains("point d'analyse"),
            "le refus doit venir du contrôle de reparse point : {error}"
        );
        {
            use std::os::windows::fs::MetadataExt as _;
            const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
            assert_eq!(
                fs::symlink_metadata(&lien).unwrap().file_attributes()
                    & FILE_ATTRIBUTE_REPARSE_POINT,
                FILE_ATTRIBUTE_REPARSE_POINT,
                "la jonction n'est pas remplacée"
            );
        }
        // `remove_dir` retire la jonction elle-même, jamais la cible.
        let _ = fs::remove_dir(&lien);
        let _ = fs::remove_dir_all(&cible);
    }
}
