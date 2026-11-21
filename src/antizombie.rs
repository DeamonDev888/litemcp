//! **Anti-zombie** — la fin de vie d'un serveur LiteMCP est vérifiable.
//!
//! Trois promesses, opt-in via
//! [`anti_zombie`](crate::server::ServerBuilder::anti_zombie) :
//!
//! 1. **Aucune annonce usurpée ne traîne.** Au bind, le registre d'annonce
//!    partagé ([`registry_dir`](crate::registry::registry_dir)) est passé au
//!    peigne fin — et le sweep **ne détruit qu'en preuve positive** : une
//!    annonce dont l'adresse effective publiée (`addr`) répond en exposant
//!    une identité d'instance **différente** de celle qu'elle publie
//!    (`instance`, servie par `/health` en en-tête `X-Instance-Id`) prouve
//!    que l'annonceur ne possède plus le port (réutilisé par une autre
//!    instance) — elle est retirée, décision re-vérifiée sous le verrou du
//!    nom. En revanche, **un port sans réponse n'est pas une preuve de
//!    mort** — un serveur vivant peut refuser (saturé, pare-feu, bind sur
//!    une autre interface que celle sondée) — et une identité invérifiable
//!    non plus : dans le doute, l'annonce est **conservée** (`undecided`)
//!    plutôt que de tuer une instance vivante ; les annonces sans adresse
//!    publiée ni identité (versions antérieures) aussi. Les débris
//!    d'écritures interrompues sont nettoyés : un `.json` **lu** mais non
//!    parsable ne peut pas être une écriture en cours (l'écriture d'annonce
//!    est **atomique** par rename — un lecteur ne voit jamais un JSON à
//!    moitié écrit), et un `.tmp` orphelin de plus de
//!    [`DEBRIS_AGE`](crate::antizombie::DEBRIS_AGE) non plus. En revanche,
//!    un fichier **illisible** (partage Windows, droits, verrou) reste une
//!    annonce possible : erreur d'accès = doute, conservé (`skipped`),
//!    jamais détruit.
//!    **La lecture est durcie** : le dossier partagé est validé avant tout
//!    parcours (voir [`sweep_registry`]), seules les entrées **fichiers
//!    réguliers** sont ouvertes (lien symbolique, dossier, FIFO — dont
//!    l'ouverture bloquerait le bind —, périphérique : ignorés), la lecture
//!    est plafonnée à 64 Kio, l'objet ouvert doit être celui inspecté, et
//!    sous Unix un fichier d'un autre uid que le dossier n'est ni lu ni
//!    sondé.
//!    Les annonces d'instances vivantes vérifiées, les fichiers d'un schéma
//!    inconnu et l'annonce du process courant sont laissés intacts.
//! 2. **Rien ne reste accroché.** À la fin de vie du serveur lié (arrêt
//!    gracieux **ou** simple `drop` après `bind`), les threads de connexion
//!    sont **drainés** : attente bornée
//!    ([`DRAIN_TIMEOUT`](crate::antizombie::DRAIN_TIMEOUT), configurable)
//!    jusqu'à zéro connexion vivante. Rust ne permet pas de tuer un thread —
//!    au-delà de la durée, le rapport le dit (`drained: false`) au lieu de
//!    mentir.
//! 3. **Tout est vérifiable.** Le bilan ([`ZombieReport`]) est lisible via
//!    [`ServerHandle::zombie_report`](crate::http::ServerHandle::zombie_report)
//!    après la fin de vie : connexions restantes, threads de handler encore
//!    vivants (un handler qui dépasse son
//!    [`tool_timeout`](crate::server::ServerBuilder::tool_timeout) termine en
//!    arrière-plan — Rust ne tue pas les threads : le bilan le compte),
//!    résultat du sweep.
//!
//! Le sweep est aussi utilisable **seul**, sans serveur : [`sweep_registry`]
//! (dossier partagé officiel) ou [`sweep_dir`] (n'importe quel dossier) — un
//! nettoyage périodique dans votre app, par exemple.
//!
//! Opt-in strict : sans `.anti_zombie()`, aucun sweep, aucun drain, aucun
//! code de plus sur les chemins chauds — les compteurs utilisés
//! (`live_connections` du [`BoundServer`](crate::http::BoundServer), threads
//! de handler bornés du [`ServerCore`]) existent
//! déjà ; le bilan se contente de les lire.
//!
//! ```no_run
//! use litemcp::antizombie::sweep_registry;
//!
//! let rapport = sweep_registry();
//! eprintln!("annonces mortes retirées : {}", rapport.reaped);
//! ```

use std::fs;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Délai de la sonde d'identité (connexion à l'adresse publiée **et**
/// lecture de la réponse `GET /health`). La borne ne sert qu'aux répondants
/// muets ou avalés par un pare-feu : le sweep reste borné. Un refus ou un
/// dépassement **n'est pas une preuve de mort** — voir `probe_identity`.
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// Durée maximale du **drain** de fin de vie (défaut du builder, voir
/// [`crate::server::ServerBuilder::drain_timeout`]). Les threads ne sont
/// jamais tués : au-delà, le rapport signale `drained: false`.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Budget d'un sweep — **temps réel passé dans les sondes de port** : au-delà,
/// les fichiers restants sont laissés sans être sondés (`skipped`). Mesurer le
/// travail du sweep et non une échéance d'horloge globale est délibéré : une
/// échéance murale se déclenche faussement quand le scheduler retire le thread
/// du sweep du CPU (process de test parallèle, machine chargée) alors qu'aucun
/// travail n'a été fait. Le budget ne mord que sur le seul coût réellement
/// bornable — les sondes (≤ [`PROBE_TIMEOUT`] chacune) — soit ~8 sondes avant
/// épuisement ; un dossier d'annonce normal contient de l'ordre de l'unité de
/// fichiers.
pub const SWEEP_BUDGET: Duration = Duration::from_secs(2);

/// Taille maximale lue dans un fichier d'annonce — une annonce réelle fait
/// moins de 2 Kio (nom, version, chemin d'exe, argv, cwd) ; le plafond borne
/// la lecture d'un fichier étranger volumineux, d'un fichier sparse ou d'un
/// lien déguisé en entrée régulière.
const ANNOUNCEMENT_MAX_BYTES: u64 = 64 * 1024;

/// Âge au-delà duquel un `.tmp` du registre est un débris (l'écriture d'une
/// annonce se termine en millisecondes) — un `.tmp` récent peut être le
/// fichier temporaire d'un écrivain concurrent **vivant** : il est respecté.
pub const DEBRIS_AGE: Duration = Duration::from_secs(60);

/// Bilan d'un **sweep** du registre d'annonce — les compteurs sont
/// disjoints : chaque fichier du dossier tombe dans exactement une case
/// (ou n'est pas visité, voir [`SweepReport::skipped`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct SweepReport {
    /// Annonces **usurpées** retirées : l'adresse publiée répond avec une
    /// identité d'instance différente de celle publiée — preuve positive que
    /// l'annonceur ne possède plus le port — et la décision a tenu sous le
    /// verrou du nom.
    pub reaped: usize,
    /// Annonces dont l'instance annoncée répond avec **exactement** son
    /// identité publiée — vivante, laissée en place.
    pub alive: usize,
    /// Annonces **conservées faute de preuve** : port sans réponse (refus ou
    /// timeout — jamais une preuve de mort : un serveur vivant peut refuser),
    /// identité absente, illisible ou invérifiable, annonce d'une version
    /// antérieure sans adresse publiée. Le conservateur prime : jamais la
    /// suppression d'une instance peut-être vivante.
    pub undecided: usize,
    /// Fichiers `.json` au contenu **non-UTF-8 ou non parsable** — débris
    /// d'une écriture interrompue (impossible autrement : l'écriture est
    /// atomique) — **effectivement retirés** (ou déjà disparus : un sweep
    /// concurrent a gagné, verdict identique). Un fichier illisible (erreur
    /// d'accès, partage Windows) n'est **pas** un débris prouvé : il est
    /// conservé et compté dans [`Self::skipped`]. Un retrait impossible
    /// n'est jamais compté ici non plus.
    pub corrupt: usize,
    /// Entrées **pas des annonces** — laissées en place, sans lecture ni
    /// sonde quand le type l'interdit : fichiers d'un schéma inconnu (JSON
    /// valide sans `port` utilisable), entrées qui ne peuvent pas être des
    /// annonces au vu du **nommage** (`.json` hors nommage d'annonce — nom
    /// vide, trop long, `..`, séparateur, `:` — et `.tmp` hors nommage de tmp
    /// d'annonce), entrées qui ne sont **pas des fichiers réguliers**
    /// (dossier, lien symbolique, FIFO, socket, périphérique) et, sous Unix,
    /// fichiers dont l'uid diffère de celui du dossier. Ils pourraient
    /// appartenir à une version future du format d'annonce, ou être les
    /// données d'un dossier arbitraire que [`sweep_dir`] n'a pas à détruire ;
    /// le resserrement en `0700` ne nettoyant pas les dépôts antérieurs d'un
    /// voisin local, c'est ce compteur qui les neutralise.
    pub foreign: usize,
    /// Annonces du process courant — ignorées : le serveur lié les gère
    /// lui-même (retrait dans le `Drop` de
    /// [`BoundServer`](crate::http::BoundServer)).
    pub own_skipped: usize,
    /// Fichiers `.tmp` orphelins (plus vieux que [`DEBRIS_AGE`]) retirés.
    pub debris: usize,
    /// Fichiers `.json` laissés **sans sonde** : budget
    /// [`SWEEP_BUDGET`] de sondes épuisé, fichier **au-delà du plafond de
    /// lecture** (64 Kio — jamais lu, jamais détruit), fichier illisible
    /// (partage, droits — l'annonce d'une instance vivante peut être
    /// momentanément inaccessible : jamais détruite sur un doute), fichier
    /// remplacé entre l'inspection et l'ouverture, suppression impossible
    /// (droits, verrou — y compris le retrait d'un débris), ou remplacé
    /// entre la sonde et la décision (une annonce plus récente a été
    /// écrite) — laissés en place, rejugés au prochain sweep.
    pub skipped: usize,
}

impl SweepReport {
    /// Le sweep n'a rien trouvé à faire — dossier vide, absent, ou tout est
    /// déjà propre.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Bilan de **fin de vie** d'un serveur lié — publié par le `Drop` de
/// [`BoundServer`](crate::http::BoundServer) quand
/// [`anti_zombie`](crate::server::ServerBuilder::anti_zombie) était actif,
/// lisible via
/// [`ServerHandle::zombie_report`](crate::http::ServerHandle::zombie_report).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ZombieReport {
    /// Toutes les connexions ont été fermées avant l'échéance du drain.
    /// `false` : des threads de connexion vivaient encore au terme de
    /// [`drain_timeout`](crate::server::ServerBuilder::drain_timeout) — une
    /// requête longue ou un client bloqué en lecture (il fermera à son
    /// `read_timeout`) ; les threads ne sont jamais tués.
    pub drained: bool,
    /// Threads de connexion encore vivants après le drain — `0` quand
    /// `drained` est vrai.
    pub connections_pending: usize,
    /// Threads de handler encore vivants **après le drain** — mode borné
    /// uniquement ([`tool_timeout`](crate::server::ServerBuilder::tool_timeout))
    /// : sans timeout, les handlers tournent dans le thread de connexion et
    /// ce compteur vaut zéro. La lecture a lieu après l'attente : un handler
    /// qui a terminé pendant le drain n'est plus compté vivant. Un handler
    /// accroché retient son slot jusqu'à sa terminaison effective ; le
    /// rapport le rend visible au lieu de le taire.
    pub handlers_alive: usize,
    /// Résultat du sweep exécuté **au bind** de ce serveur.
    pub sweep: SweepReport,
}

/// État anti-zombie d'un serveur lié — partagé entre le [`BoundServer`]
/// (qui le remplit à sa fin de vie) et le [`ServerHandle`]
/// ([`crate::http::ServerHandle::zombie_report`] le lit, même après que le
/// serveur a rendu l'âme dans un autre thread).
pub(crate) struct ZombieState {
    drain_timeout: Duration,
    sweep: SweepReport,
    report: Mutex<Option<ZombieReport>>,
}

impl ZombieState {
    pub(crate) fn new(sweep: SweepReport, drain_timeout: Duration) -> Self {
        Self {
            drain_timeout,
            sweep,
            report: Mutex::new(None),
        }
    }

    /// Fin de vie : drain **borné** des threads de connexion puis bilan —
    /// `handlers_alive` est lu **après** l'attente (une lecture au début du
    /// drain surcomptait les handlers qui y terminent). Le compteur est
    /// passé en closure pour être lu au bon moment.
    ///
    /// Appelé par le `Drop` du [`BoundServer`] — donc sur **tous** les chemins de fin de
    /// vie (arrêt gracieux, serveur lié jamais servi, abandon après un bind
    /// partiel). L'attente est plafonnée par `drain_timeout` : un `Drop` ne
    /// peut pas pendre indéfiniment.
    pub(crate) fn finish(
        &self,
        live_connections: &AtomicUsize,
        handlers_alive: impl FnOnce() -> usize,
    ) -> ZombieReport {
        // Échéance **bornée par construction** : `drain_timeout` arbitraire
        // (jusqu'à `Duration::MAX`) ne panique jamais — au-delà d'`Instant`,
        // l'attente est simplement sans échéance (elle reste bornée par la
        // réalité : rien ne vit plus). La lecture de l'échéance n'est jamais
        // réarmée : le drain ne peut pas dépasser son plafond.
        let deadline = Instant::now().checked_add(self.drain_timeout);
        while live_connections.load(Ordering::Relaxed) > 0
            && deadline.is_none_or(|end| Instant::now() < end)
        {
            std::thread::sleep(Duration::from_millis(2));
        }
        let connections_pending = live_connections.load(Ordering::Relaxed);
        let handlers_alive = handlers_alive();
        let report = ZombieReport {
            drained: connections_pending == 0,
            connections_pending,
            handlers_alive,
            sweep: self.sweep,
        };
        // Un consommateur aurait pu paniquer en lisant — le rapport reste
        // publiable (même tolérance que le reste du module : jamais une
        // condition de fonctionnement).
        let mut slot = self.report.lock().unwrap_or_else(PoisonError::into_inner);
        *slot = Some(report);
        report
    }

    /// Rapport publié par [`Self::finish`] — `None` avant la fin de vie du
    /// serveur lié.
    pub(crate) fn report(&self) -> Option<ZombieReport> {
        *self.report.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Passe le registre d'annonce partagé au peigne fin —
/// [`registry_dir`](crate::registry::registry_dir). Voir [`sweep_dir`].
///
/// Le dossier est **validé avant toute lecture**
/// (`validate_private_dir`, dans `crate::registry`) :
/// répertoire réel (Unix : à nous, mode `0700` ; Windows : hors point
/// d'analyse). Refusé (absent, lien symbolique ou jonction, dossier d'un
/// autre utilisateur, resserrement impossible) : rapport **vide**, rien n'est
/// lu ni sondé — un dossier 0777 pré-créé par un voisin local ne peut donc
/// pas déclencher de sonde `GET /health` ni bloquer le bind avec un FIFO.
#[must_use]
pub fn sweep_registry() -> SweepReport {
    sweep_registry_at(&crate::registry::registry_dir())
}

/// [`sweep_registry`] sur un dossier explicite — la validation est la même.
/// Privé : les tests exercent le refus sur un dossier scratch (le dossier
/// réel de la machine n'est jamais mis en scène).
#[must_use]
fn sweep_registry_at(dir: &Path) -> SweepReport {
    if crate::registry::validate_private_dir(dir).is_err() {
        return SweepReport::default();
    }
    sweep_dir(dir)
}

/// Le nom d'un **tmp d'annonce** — `<nom>.json.<pid>.<suffixe>.tmp`, le
/// nommage écrit par `registry::write_atomically` : nom d'annonce sûr, PID,
/// puis soit 8 octets du CSPRNG en hexadécimal (16 caractères — nommage
/// actuel), soit l'ancien suffixe en nanosecondes (tous chiffres, versions
/// antérieures). Les deux sont reconnus pour que les reliques d'une version
/// antérieure soient nettoyées. Un `.tmp` qui ne porte pas ce nommage n'est
/// jamais un débris d'annonce : fichier étranger, respecté par le sweep.
fn announcement_tmp_name(name: &str) -> bool {
    let Some(core) = name.strip_suffix(".tmp") else {
        return false;
    };
    let Some(pos) = core.rfind(".json.") else {
        return false;
    };
    let tail = &core[pos + ".json.".len()..];
    let mut segments = tail.split('.');
    match (segments.next(), segments.next(), segments.next()) {
        (Some(pid), Some(suffix), None) => {
            !pid.is_empty()
                && !suffix.is_empty()
                && pid.bytes().all(|b| b.is_ascii_digit())
                && ((suffix.len() == 16 && suffix.bytes().all(|b| b.is_ascii_hexdigit()))
                    || suffix.bytes().all(|b| b.is_ascii_digit()))
                && crate::registry::is_safe_name(&core[..pos])
        }
        _ => false,
    }
}

/// Le nom d'un **fichier d'annonce** — `<nom>.json` où `nom` est un nom
/// d'annonce sûr (`registry::is_safe_name`). Rend le nom d'annonce quand le
/// nommage correspond, `None` sinon : un `.json` étranger (données d'un
/// dossier arbitraire) n'est jamais détruit par le sweep.
fn announcement_stem(name: &str) -> Option<&str> {
    let stem = name.strip_suffix(".json")?;
    if crate::registry::is_safe_name(stem) {
        Some(stem)
    } else {
        None
    }
}

/// Pourquoi une entrée n'a pas pu être lue comme une annonce — chaque refus
/// a une conséquence différente dans le bilan (voir [`sweep_dir`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadRefusal {
    /// Entrée qui n'est pas un fichier régulier (dossier, lien symbolique,
    /// FIFO, socket, périphérique) : jamais ouverte.
    NotARegularFile,
    /// Plus de 64 Kio : ce n'est pas une annonce, et la lire serait une
    /// lecture non bornée.
    TooLarge,
    /// Illisible (droits, partage Windows, verrou), ou **remplacée** entre
    /// l'inspection et l'ouverture (l'objet ouvert n'est pas celui inspecté).
    Unreadable,
}

/// Lit une annonce **sans se laisser détourner** : `inspected` est le
/// `symlink_metadata` déjà pris sur l'entrée, et l'objet réellement ouvert
/// doit être **le même** (Unix : `dev` + `ino` lus par `fstat` sur le
/// descripteur) — un fichier remplacé par un lien ou un autre fichier après
/// l'inspection est refusé, pas suivi. La lecture est plafonnée à 64 Kio
/// (constante privée `ANNOUNCEMENT_MAX_BYTES`) : un fichier plus gros n'est
/// pas une annonce, et le lire épuiserait la mémoire sur un fichier sparse ou
/// un lien vers un périphérique.
fn read_announcement(path: &Path, inspected: &fs::Metadata) -> Result<Vec<u8>, ReadRefusal> {
    let file_type = inspected.file_type();
    if !file_type.is_file() || file_type.is_symlink() {
        return Err(ReadRefusal::NotARegularFile);
    }
    let file = fs::File::open(path).map_err(|_| ReadRefusal::Unreadable)?;
    // L'objet réellement ouvert doit être celui inspecté : `fstat` sur le
    // descripteur, comparé au `symlink_metadata` — un remplacement par un
    // lien (ou un autre fichier) entre les deux est refusé, pas suivi.
    #[cfg(unix)]
    {
        let opened = file.metadata().map_err(|_| ReadRefusal::Unreadable)?;
        if (opened.dev(), opened.ino()) != (inspected.dev(), inspected.ino()) {
            return Err(ReadRefusal::Unreadable);
        }
    }
    let mut body = Vec::new();
    if file
        .take(ANNOUNCEMENT_MAX_BYTES + 1)
        .read_to_end(&mut body)
        .is_err()
    {
        return Err(ReadRefusal::Unreadable);
    }
    if body.len() as u64 > ANNOUNCEMENT_MAX_BYTES {
        return Err(ReadRefusal::TooLarge);
    }
    Ok(body)
}

/// Passe `dir` au peigne fin : retire les annonces **usurpées** (l'adresse
/// publiée répond avec une identité d'instance différente — décision
/// re-vérifiée sous le verrou du nom), les `.json` au contenu non-UTF-8 ou
/// non parsable et les `.tmp` orphelins (quand le retrait aboutit — un
/// fichier insupprimable est compté `skipped`, jamais prétendu retiré) ;
/// conserve en doute les annonces sans réponse ou sans identité vérifiable
/// (un refus TCP n'est pas une preuve de mort) **et les fichiers illisibles**
/// (partage Windows, droits, verrou : l'annonce d'une instance vivante peut
/// être momentanément inaccessible — elle n'est jamais détruite sur un doute),
/// et respecte les instances vivantes vérifiées, les fichiers d'un schéma
/// inconnu et l'annonce du process courant. Dossier absent ou vide : rapport
/// vide — jamais d'erreur.
///
/// **Conçue pour les dossiers d'annonces** — le nommage est le contrat : ne
/// sont détruits que les fichiers qui portent LE nommage d'annonce écrit par
/// [`crate::registry`] — `<nom>.json` (nom sûr, voir
/// `registry::is_safe_name`) et son tmp d'écriture
/// `<nom>.json.<pid>.<aléa>.tmp` (ou l'ancien suffixe en nanosecondes, encore
/// reconnu). Tout autre fichier (`.tmp` utilisateur,
/// `.json` de données au nom hors nommage) est compté [`SweepReport::foreign`]
/// et respecté, où que soit le dossier : un nettoyage périodique sur un
/// dossier de données ne détruit jamais un fichier étranger.
///
/// **Lecture durcie** : seules les entrées **fichiers réguliers** sont
/// ouvertes — l'entrée est inspectée par `symlink_metadata` (jamais
/// `metadata` : aucun lien n'est suivi), et un dossier, un lien symbolique,
/// un FIFO (dont l'ouverture bloquerait le sweep), une socket ou un
/// périphérique est compté [`SweepReport::foreign`] sans être ouvert. La
/// lecture est plafonnée à 64 Kio (constante privée
/// `ANNOUNCEMENT_MAX_BYTES` — une annonce réelle fait moins de 2 Kio) ;
/// au-delà, le fichier est laissé en place (`skipped`), jamais détruit.
/// L'objet **ouvert** doit être celui inspecté (Unix : `dev` + `ino` lus par
/// `fstat`) : un fichier remplacé par un lien entre l'inspection et
/// l'ouverture est refusé, pas suivi. Sous Unix, un fichier dont l'uid
/// diffère de celui du dossier est ignoré — le resserrement en `0700` ne
/// nettoie pas les dépôts antérieurs d'un voisin local. **Reste hors de
/// portée** : un remplacement par un FIFO *entre* l'inspection et
/// l'ouverture bloquerait l'ouverture (pas d'`O_NONBLOCK` portable sans
/// constante système) — fenêtre de course sur un dossier déjà partagé, non
/// revendiquée.
///
/// **Limite assumée** : un `.json` au contenu non parsable dont le nom
/// ressemble exactement à un nom d'annonce (`<nom>.json`, nom sûr) reste
/// indistinguable d'un débris d'annonce par le seul nommage — il est retiré
/// même dans un dossier arbitraire. Le sweep ne peut pas faire mieux sans
/// lire le contenu (une écriture d'annonce interrompue n'est pas parsable
/// par construction), et le dossier d'annonces est un temp partagé où un
/// débris d'annonce est justement ce qu'il faut nettoyer.
///
/// Utilisable périodiquement sans serveur ; le budget est [`SWEEP_BUDGET`] de
/// **temps de sondes effectif** (chaque sonde est bornée par
/// [`PROBE_TIMEOUT`]) — une échéance d'horloge globale se déclencherait
/// faussement sous charge, quand le scheduler retire le CPU au thread.
#[must_use]
pub fn sweep_dir(dir: &Path) -> SweepReport {
    let mut report = SweepReport::default();
    let Ok(entries) = fs::read_dir(dir) else {
        return report; // dossier absent ou illisible : rien à faire
    };
    // Budget en **temps de sonde effective** (voir [`SWEEP_BUDGET`]) : seule
    // la durée réellement passée dans `probe_identity` compte — les
    // intervalles où le scheduler retire le CPU au thread n'en font pas
    // partie.
    let mut probe_spent = Duration::ZERO;
    // Propriétaire du dossier (Unix) : toutes les annonces sont écrites sous
    // notre identité. Un fichier d'un autre uid déposé dans un dossier
    // partagé qui nous appartient — le resserrement en 0700 ne nettoie pas
    // l'existant — n'est ni lu ni sondé. Dossier non inspectable : pas de
    // filtre (le parcours, lui, a déjà réussi).
    #[cfg(unix)]
    let dir_uid = fs::symlink_metadata(dir).ok().map(|meta| meta.uid());
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(std::ffi::OsStr::to_str) else {
            continue;
        };
        let is_tmp = name.ends_with(".tmp");
        if !is_tmp && !name.ends_with(".json") {
            continue; // jamais une annonce : respecté, pas compté
        }
        // Type décidé sur `symlink_metadata` — jamais `metadata` : le sweep
        // ne suit aucun lien. Tout ce qui n'est pas un **fichier régulier**
        // (dossier, lien symbolique, FIFO, socket, périphérique) est ignoré
        // sans être ouvert : un FIFO nommé `.json` bloquerait le bind à
        // l'ouverture, un lien ferait sortir du dossier.
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue; // disparue entre `read_dir` et l'inspection
        };
        let file_type = meta.file_type();
        if !file_type.is_file() || file_type.is_symlink() {
            report.foreign += 1;
            continue;
        }
        #[cfg(unix)]
        if dir_uid.is_some_and(|owner| meta.uid() != owner) {
            report.foreign += 1;
            continue;
        }
        // Débris d'écriture : les `.tmp` de `write_atomically` ne sont
        // jamais lus par les consommateurs. Un `.tmp` **récent** peut être
        // celui d'un écrivain vivant — seul un orphelin (vieux) part.
        if is_tmp {
            if announcement_tmp_name(name) {
                if older_than(&path, DEBRIS_AGE) && fs::remove_file(&path).is_ok() {
                    report.debris += 1;
                }
                continue;
            }
            // Un `.tmp` qui ne porte pas le nommage de tmp d'annonce n'est
            // jamais un débris : fichier étranger (dossier arbitraire),
            // respecté — sweep_dir ne détruit que le nommage d'annonce.
            report.foreign += 1;
            continue;
        }
        // Nommage : `<nom>.json` où `nom` est un nom d'annonce sûr. Un
        // `.json` dont le nom ne peut pas être celui d'une annonce est un
        // fichier étranger (données d'un dossier arbitraire) : respecté
        // SANS lecture ni sonde — le sweep ne détruit que le nommage
        // d'annonce (audit 2026-11-13, § 5.2).
        if announcement_stem(name).is_none() {
            report.foreign += 1;
            continue;
        }
        let body = match read_announcement(&path, &meta) {
            Ok(body) => body,
            Err(ReadRefusal::TooLarge | ReadRefusal::Unreadable) => {
                // Trop gros (une annonce réelle fait moins de 2 Kio) : le
                // fichier n'est pas lu — la lecture serait non bornée — et
                // n'est pas détruit : ce n'est pas un débris prouvé.
                // Illisible (partage Windows, droits, verrou) ou remplacé
                // entre l'inspection et l'ouverture : l'annonce d'une
                // instance VIVANTE peut être momentanément inaccessible —
                // doute, conservée sans sonde, rejugée au prochain sweep.
                report.skipped += 1;
                continue;
            }
            // Défensif : le type et le propriétaire sont déjà contrôlés.
            Err(ReadRefusal::NotARegularFile) => {
                report.foreign += 1;
                continue;
            }
        };
        let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&body) else {
            // Présent mais non parsable : l'écriture atomique par rename
            // garantit qu'aucune écriture en cours ne peut être observée à
            // moitié — c'est un débris corrompu, pas une annonce vivante.
            count_corrupt_removal(&mut report, remove_with_retry(&path));
            continue;
        };
        // Notre propre annonce (ou celle d'un autre serveur du même
        // process — vivant par définition) : jamais retirée par le sweep.
        if doc.get("pid").and_then(serde_json::Value::as_u64) == Some(u64::from(std::process::id()))
        {
            report.own_skipped += 1;
            continue;
        }
        // Lecteur tolérant (même contrat que MCP Control) : sans `port`
        // utilisable, le fichier n'est pas une annonce dont on peut prouver
        // la mort — schéma inconnu, laissé en place.
        let Some(port) = doc
            .get("port")
            .and_then(serde_json::Value::as_u64)
            .and_then(|p| u16::try_from(p).ok())
        else {
            report.foreign += 1;
            continue;
        };
        if port == 0 {
            // `announce_at` ne publie jamais un port 0 : schéma inconnu.
            report.foreign += 1;
            continue;
        }
        // Preuve d'identité : sans adresse effective publiée ou sans identité
        // (annonce d'une version antérieure, document incomplet), RIEN n'est
        // prouvable — conservée, sans sonde (aucun budget consommé).
        let (Some(probe_addr), Some(expected)) = (
            probe_addr_of(&doc),
            doc.get("instance").and_then(serde_json::Value::as_u64),
        ) else {
            report.undecided += 1;
            continue;
        };
        if probe_spent >= SWEEP_BUDGET {
            report.skipped += 1;
            continue;
        }
        let probe_started = Instant::now();
        let verdict = probe_identity(probe_addr, expected);
        probe_spent += probe_started.elapsed();
        match verdict {
            Identity::Verified => {
                // L'instance publiée répond avec exactement son identité :
                // vivante, respectée.
                report.alive += 1;
                continue;
            }
            Identity::Unproven => {
                // Refus, timeout, réponse illisible, identité absente : le
                // doute profite à l'annonce — un refus TCP n'est pas une
                // preuve de mort.
                report.undecided += 1;
                continue;
            }
            Identity::Usurped => {}
        }
        // Usurpation prouvée : le port publié est occupé par une identité
        // différente. La décision est re-vérifiée **sous le verrou du nom**
        // : entre la sonde et le retrait, une annonce plus récente (un
        // redémarrage) peut avoir été écrite — elle est respectée.
        let Some(name) = path.file_stem().and_then(std::ffi::OsStr::to_str) else {
            report.skipped += 1;
            continue;
        };
        let removed = crate::registry::with_name_lock(name, || {
            // Relecture **durcie** : même contrôle de type, de propriétaire,
            // de taille et d'identité d'objet que la lecture initiale. Un
            // fichier remplacé par un lien entre la sonde et le retrait
            // n'est pas suivi — et n'est jamais supprimé sur un doute.
            let relecture = match fs::symlink_metadata(&path) {
                Ok(meta) if meta.file_type().is_file() && !meta.file_type().is_symlink() => {
                    #[cfg(unix)]
                    let owned = dir_uid.is_none_or(|owner| meta.uid() == owner);
                    #[cfg(not(unix))]
                    let owned = true;
                    if owned {
                        read_announcement(&path, &meta).ok().and_then(|body| {
                            serde_json::from_slice::<serde_json::Value>(&body).ok()
                        })
                    } else {
                        None
                    }
                }
                Ok(_) => None,
                // Disparue : un sweep concurrent ou le propriétaire l'a
                // retirée — même verdict qu'un retrait effectué. Une autre
                // erreur n'est pas une preuve : conservée.
                Err(error) => return error.kind() == std::io::ErrorKind::NotFound,
            };
            match relecture {
                // Toujours la même annonce étrangère (même identité) : le
                // verdict tient — retrait.
                Some(current)
                    if current.get("pid").and_then(serde_json::Value::as_u64)
                        != Some(u64::from(std::process::id()))
                        && current.get("instance").and_then(serde_json::Value::as_u64)
                            == Some(expected) =>
                {
                    match remove_with_retry(&path) {
                        Ok(()) => true,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                        Err(_) => false,
                    }
                }
                // Remplacée entre-temps (redémarrage) : respectée — c'est
                // désormais l'annonce d'une autre (nouvelle) instance.
                Some(_) => false,
                // Illisible, remplacée par autre chose, ou devenue non
                // régulière : aucune preuve que ce soit encore l'annonce
                // sondée — conservée, rejugée au prochain sweep.
                None => false,
            }
        });
        match removed {
            Some(true) => report.reaped += 1,
            Some(false) => report.undecided += 1,
            // Verrou non acquis : rejugé au prochain sweep.
            None => report.skipped += 1,
        }
    }
    report
}

/// Verdict d'une sonde d'identité sur l'adresse publiée par une annonce.
enum Identity {
    /// L'instance annoncée répond et expose exactement l'identité publiée :
    /// vivante.
    Verified,
    /// Une **autre** instance (identité différente) occupe le port : preuve
    /// positive que l'annonceur n'y possède plus rien.
    Usurped,
    /// Rien de prouvable — port sans réponse (refus/timeout : un serveur
    /// vivant peut refuser), réponse illisible, identité absente : doute,
    /// l'annonce est conservée.
    Unproven,
}

/// Sonde d'identité : connexion à l'adresse **publiée** par l'annonce (plus
/// jamais un bouclage figé — un serveur lié à IPv6 ou à une IP spécifique
/// serait jugé mort alors qu'il vit), puis `GET /health` et lecture bornée
/// de l'en-tête `X-Instance-Id`. Un connect réussi prouve qu'un auditeur
/// vit ; seul un en-tête d'identité dans une réponse HTTP **2xx** prouve
/// **quelle** instance — une chaîne `x-instance-id:` dans le corps d'une
/// page d'erreur ne prouve rien. Un échec de connexion, un statut non 2xx,
/// des en-têtes incomplets ou un dépassement de [`PROBE_TIMEOUT`] rendent
/// `Unproven` : ce n'est pas une preuve de mort.
fn probe_identity(addr: SocketAddr, expected: u64) -> Identity {
    let mut stream = match TcpStream::connect_timeout(&addr, PROBE_TIMEOUT) {
        Ok(stream) => stream,
        Err(_) => return Identity::Unproven, // refus ou timeout : pas une preuve
    };
    let _ = stream.set_read_timeout(Some(PROBE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(PROBE_TIMEOUT));
    let request = format!("GET /health HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return Identity::Unproven;
    }
    // En-têtes d'une sonde de santé : quelques centaines d'octets — la
    // lecture est bornée (cap + timeout) pour qu'un répondant bavard ne
    // retienne pas le sweep.
    let mut head = [0u8; 4096];
    let mut read = 0usize;
    loop {
        match stream.read(&mut head[read..]) {
            Ok(0) => break,
            Ok(n) => {
                read += n;
                if read == head.len() {
                    break;
                }
            }
            Err(_) => break, // timeout : ce qui a été lu suffit (ou rien)
        }
    }
    let Ok(text) = std::str::from_utf8(&head[..read]) else {
        return Identity::Unproven;
    };
    // Une identité ne vaut que dans le **bloc d'en-têtes** d'une réponse HTTP
    // réussie : une chaîne `x-instance-id:` dans le corps (page d'erreur,
    // JSON d'un autre service sur un port réutilisé) ne prouve rien — le
    // doute conserve l'annonce. Un statut non 2xx n'est pas la sonde
    // `/health` de LiteMCP.
    let Some((status_line, headers_and_body)) = text.split_once('\n') else {
        return Identity::Unproven;
    };
    let mut status = status_line.trim_end_matches('\r').split_whitespace();
    let (Some(version), Some(code)) = (status.next(), status.next()) else {
        return Identity::Unproven;
    };
    let Ok(code) = code.parse::<u16>() else {
        return Identity::Unproven;
    };
    if !version.starts_with("HTTP/") || !(200..300).contains(&code) {
        return Identity::Unproven;
    }
    // En-têtes incomplets (pas de ligne vide) : rien de vérifiable.
    let Some(head_end) = headers_and_body
        .find("\r\n\r\n")
        .or_else(|| headers_and_body.find("\n\n"))
    else {
        return Identity::Unproven;
    };
    for line in headers_and_body[..head_end].lines() {
        if let Some(value) = line
            .split_once(':')
            .filter(|(champ, _)| champ.trim().eq_ignore_ascii_case("x-instance-id"))
            .map(|(_, valeur)| valeur.trim())
        {
            return match u64::from_str_radix(value, 16) {
                Ok(id) if id == expected => Identity::Verified,
                Ok(_) => Identity::Usurped,
                Err(_) => Identity::Unproven,
            };
        }
    }
    Identity::Unproven
}

/// Compte honnêtement le retrait d'un débris : `corrupt` seulement si le
/// fichier a **effectivement disparu** (ou avait déjà disparu — un sweep
/// concurrent a gagné, verdict identique) ; sinon `skipped` : le fichier
/// reste, sera rejugé au prochain sweep. Un compteur qui annonce un retrait
/// qui n'a pas eu lieu est un mensonge du bilan.
fn count_corrupt_removal(report: &mut SweepReport, outcome: std::io::Result<()>) {
    match outcome {
        Ok(()) => report.corrupt += 1,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => report.corrupt += 1,
        Err(_) => report.skipped += 1,
    }
}

/// Adresse de sonde d'une annonce — l'adresse **publiée** (`addr`), avec
/// substitution défensive du bouclage pour un bind multi-interface
/// (`0.0.0.0` → `127.0.0.1`, `::` → `::1`) : on sonde ce que l'instance a
/// elle-même publié, jamais un bouclage figé. `None` = adresse absente ou
/// illisible (versions antérieures, documents étrangers).
fn probe_addr_of(doc: &serde_json::Value) -> Option<SocketAddr> {
    let addr: SocketAddr = doc.get("addr")?.as_str()?.parse().ok()?;
    Some(match addr.ip() {
        std::net::IpAddr::V4(ip) if ip.is_unspecified() => SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            addr.port(),
        ),
        std::net::IpAddr::V6(ip) if ip.is_unspecified() => SocketAddr::new(
            std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
            addr.port(),
        ),
        _ => addr,
    })
}

/// Retrait best-effort avec **retries brefs** — le dossier d'annonce est
/// **partagé** : un lecteur concurrent (MCP Control, un autre sweep, un scan
/// antivirus) peut tenir le fichier quelques millisecondes, et sous Windows
/// une violation de partage est alors **transitoire**, pas un refus
/// définitif. Trois tentatives espacées ; `NotFound` court-circuite (un
/// sweep concurrent a gagné — verdict identique). Retourne la dernière
/// erreur sinon : l'appelant compte le fichier `skipped`, il sera rejugé au
/// prochain sweep.
fn remove_with_retry(path: &Path) -> std::io::Result<()> {
    let mut last = fs::remove_file(path);
    for attempt in 1..=3 {
        match &last {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(std::io::Error::from(std::io::ErrorKind::NotFound));
            }
            Err(_) => {
                std::thread::sleep(Duration::from_millis(25 * attempt));
                last = fs::remove_file(path);
            }
        }
    }
    last
}

/// Le port répond-il encore sur le bouclage ? Un connect **réussi** prouve
/// qu'un auditeur vit ; un échec (refus, timeout) ne prouve **rien** — un
/// serveur vivant peut refuser (saturé, pare-feu) et un serveur lié à IPv6
/// ou à une IP spécifique ne répond pas sur `127.0.0.1` : ce verdict
/// appartient à la sonde d'identité du sweep (`probe_identity` sur
/// l'adresse publiée). Primitive conservée pour compatibilité — le sweep ne
/// l'utilise plus.
#[must_use]
pub fn port_alive(port: u16) -> bool {
    TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), PROBE_TIMEOUT).is_ok()
}

/// Le fichier date-t-il d'au moins `age` ? Une mtime dans le futur (horloge
/// recalée) compte comme récente : rien n'est retiré par erreur.
fn older_than(path: &Path, age: Duration) -> bool {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .map(|modified| {
            modified
                .elapsed()
                .map(|elapsed| elapsed >= age)
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_zero_ne_repond_pas() {
        assert!(!port_alive(0), "le port 0 n'est pas un port écoutable");
    }

    #[test]
    fn un_port_ecoute_repond_et_un_port_libere_ne_repond_plus() {
        // Tranche non-éphémère (sous toutes les plages éphémères) : un bind
        // parallèle ne peut pas réattribuer le port entre le drop et la
        // seconde sonde — avec `:0`, la course rendait le test flaky.
        let listener = (23130..=23149)
            .find_map(|p| std::net::TcpListener::bind(("127.0.0.1", p)).ok())
            .expect("un port de la tranche de test doit être libre");
        let port = listener.local_addr().unwrap().port();
        assert!(port_alive(port), "un auditeur réel répond sur le bouclage");
        drop(listener);
        assert!(
            !port_alive(port),
            "sans auditeur, le bouclage ne répond pas — un non-réponse \
             n'est pas une preuve de mort (voir la sonde d'identité)"
        );
    }

    #[test]
    fn un_fichier_recent_n_est_pas_un_debris() {
        let dir = std::env::temp_dir().join(format!(
            "litemcp-az-age-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let tmp = dir.join("annonce.json.123.456.tmp");
        fs::write(&tmp, "{}").unwrap();
        assert!(!older_than(&tmp, DEBRIS_AGE));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn le_rapport_de_fin_de_vie_est_publie_une_fois() {
        let state = ZombieState::new(
            SweepReport {
                reaped: 2,
                ..SweepReport::default()
            },
            Duration::ZERO,
        );
        let live = AtomicUsize::new(0);
        assert!(state.report().is_none(), "rien avant la fin de vie");
        let report = state.finish(&live, || 7);
        assert!(report.drained);
        assert_eq!(report.sweep.reaped, 2);
        assert_eq!(
            report.handlers_alive, 7,
            "le compteur passé est lu tel quel"
        );
        assert_eq!(state.report(), Some(report), "lisible après la fin de vie");
    }

    #[test]
    fn le_compteur_de_handlers_est_lu_apres_le_drain() {
        // Le compteur passe de 1 à 0 PENDANT le drain : la lecture post-drain
        // doit voir 0 — la lecture pré-drain (ancien comportement) aurait
        // surcompté.
        let state = ZombieState::new(SweepReport::default(), Duration::from_millis(500));
        let live = std::sync::Arc::new(AtomicUsize::new(1));
        let release = std::sync::Arc::clone(&live);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            release.store(0, Ordering::Relaxed);
        });
        let report = state.finish(&live, || live.load(Ordering::Relaxed));
        assert!(report.drained, "la connexion est libérée pendant le drain");
        assert_eq!(
            report.handlers_alive, 0,
            "lu APRÈS le drain : le handler qui y a terminé n'est plus vivant"
        );
    }

    #[test]
    fn l_adresse_de_sonde_est_celle_publiee_jamais_le_bouclage_fige() {
        let doc = |addr: &str| serde_json::json!({ "addr": addr });
        let resolve = |addr: &str| probe_addr_of(&doc(addr));
        assert_eq!(
            resolve("10.1.2.3:8080"),
            "10.1.2.3:8080".parse().ok(),
            "IP spécifique : sondée telle quelle"
        );
        assert_eq!(
            resolve("[::1]:8080"),
            "[::1]:8080".parse().ok(),
            "IPv6 : sondée telle quelle"
        );
        assert_eq!(resolve("127.0.0.1:8080"), "127.0.0.1:8080".parse().ok());
        assert_eq!(
            resolve("0.0.0.0:8080"),
            "127.0.0.1:8080".parse().ok(),
            "bind multi-interface : sondé par le bouclage IPv4"
        );
        assert_eq!(
            resolve("[::]:8080"),
            "[::1]:8080".parse().ok(),
            "bind multi-interface : sondé par le bouclage IPv6"
        );
        assert_eq!(resolve("pas-une-adresse"), None);
        assert_eq!(resolve("999.1.1.1:80"), None);
        assert_eq!(
            probe_addr_of(&serde_json::json!({ "port": 8080 })),
            None,
            "sans addr publiée : rien à sonder"
        );
    }

    // Répondeur miniature : accepte plusieurs connexions (les tests sondent
    // plusieurs fois la même adresse), répond tant que le listener vit.
    fn health_responder(instance: u64) -> SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                let mut request = [0u8; 1024];
                let _ = stream.read(&mut request);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nX-Instance-Id: {instance:016x}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(b"ok");
            }
        });
        addr
    }

    #[test]
    fn la_sonde_verifie_l_identite_publiee() {
        let responder = health_responder(0xA1B2_C3D4_5E6F_7081);
        assert!(
            matches!(
                probe_identity(responder, 0xA1B2_C3D4_5E6F_7081),
                Identity::Verified
            ),
            "l'identité qui répond est celle attendue"
        );
        assert!(
            matches!(
                probe_identity(responder, 0x0102_0304_0506_0708),
                Identity::Usurped
            ),
            "une identité différente prouve l'usurpation du port"
        );
    }

    // Répondeur qui sert une réponse HTTP **fixe** à chaque connexion.
    fn raw_responder(response: &'static [u8]) -> SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                let mut request = [0u8; 1024];
                let _ = stream.read(&mut request);
                let _ = stream.write_all(response);
            }
        });
        addr
    }

    #[test]
    fn seuls_un_statut_2xx_et_un_en_tete_d_identite_prouvent_une_usurpation() {
        // Identité présente UNIQUEMENT dans le corps : une page d'erreur ou le
        // JSON d'un autre service ne prouve pas quelle instance occupe le
        // port — doute, l'annonce est conservée.
        let body_only = raw_responder(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nx-instance-id: 0000000000000002\n",
        );
        assert!(
            matches!(probe_identity(body_only, 1), Identity::Unproven),
            "une identité dans le corps n'est pas un en-tête : aucune preuve d'usurpation"
        );

        // En-tête présent mais statut d'erreur (404 d'un autre service sur un
        // port réutilisé) : ce n'est pas la sonde /health de l'instance.
        let error_status = raw_responder(
            b"HTTP/1.1 404 Not Found\r\nX-Instance-Id: 0000000000000002\r\nConnection: close\r\n\r\n",
        );
        assert!(
            matches!(probe_identity(error_status, 1), Identity::Unproven),
            "un statut non 2xx ne prouve rien — le doute conserve l'annonce"
        );

        // Contrôle : le même en-tête sur un 200 reste une preuve positive.
        let ok_other = raw_responder(
            b"HTTP/1.1 200 OK\r\nX-Instance-Id: 0000000000000002\r\nConnection: close\r\n\r\nok",
        );
        assert!(
            matches!(probe_identity(ok_other, 1), Identity::Usurped),
            "200 + identité différente = usurpation prouvée"
        );
    }

    #[test]
    fn un_port_sans_reponse_n_est_pas_une_preuve_de_mort() {
        // Auditeur muet (accepte sans rien répondre) : la sonde attend en
        // vain et rend Unproven — jamais un verdict de mort.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let _ = listener.accept(); // ne répond jamais
        });
        assert!(
            matches!(probe_identity(addr, 42), Identity::Unproven),
            "réponse absente = doute"
        );
        // Port sans aucun auditeur : refus = doute aussi (un vivant saturé
        // refuse aussi).
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let free_addr = free.local_addr().unwrap();
        drop(free);
        assert!(
            matches!(probe_identity(free_addr, 42), Identity::Unproven),
            "un refus TCP n'est pas une preuve de mort"
        );
    }

    #[test]
    fn une_entree_non_reguliere_est_ignoree_jamais_detruite() {
        let dir = std::env::temp_dir().join(format!(
            "litemcp-az-debris-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        // Un **dossier** nommé `*.json` : ce n'est pas un fichier, donc
        // jamais une annonce — ignoré (compté étranger), jamais ouvert.
        let blocked = dir.join("bloque.json");
        fs::create_dir(&blocked).unwrap();
        // Un vrai débris non parsable (contenu lu), lui, part.
        let corrupt = dir.join("casse.json");
        fs::write(&corrupt, "{pas du json").unwrap();

        let report = sweep_dir(&dir);
        assert!(!corrupt.exists(), "le vrai débris est retiré : {report:?}");
        assert_eq!(
            report.corrupt, 1,
            "seul le débris effectivement retiré est compté corrupt : {report:?}"
        );
        assert_eq!(
            report.foreign, 1,
            "un dossier nommé .json n'est pas un fichier d'annonce : {report:?}"
        );
        assert_eq!(
            report.skipped, 0,
            "une entrée non régulière n'est ni lue ni « illisible » : {report:?}"
        );
        assert!(blocked.exists(), "un doute ne détruit jamais le fichier");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn le_compteur_de_retrait_ne_ment_jamais() {
        use std::io::{Error, ErrorKind};
        let mut report = SweepReport::default();
        count_corrupt_removal(&mut report, Ok(()));
        count_corrupt_removal(&mut report, Err(Error::from(ErrorKind::NotFound)));
        assert_eq!(
            report.corrupt, 2,
            "un retrait réussi et un fichier déjà disparu sont des débris traités"
        );
        assert_eq!(report.skipped, 0);
        count_corrupt_removal(&mut report, Err(Error::from(ErrorKind::PermissionDenied)));
        assert_eq!(
            report.corrupt, 2,
            "un retrait échoué n'est jamais compté comme un retrait"
        );
        assert_eq!(report.skipped, 1, "il est rejugé au prochain sweep");
    }

    #[test]
    fn sweep_usurpation_prouvee_retrait_doute_et_vie_conservees() {
        let dir = std::env::temp_dir().join(format!(
            "litemcp-az-sonde-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        // Prérequis du verrou de nom : `with_name_lock` verrouille dans le
        // dossier de registre global (`registry_dir()`), que `announce` crée
        // en production. Sans lui, `create_dir` échoue en `NotFound` et le
        // sweep renonce (« verrou occupé ») : le balayage ne retirerait rien.
        // Le test ne peut donc pas dépendre de l'ordre d'exécution des autres
        // tests (sur Windows il passait par chance, un autre test créant le
        // dossier) — il pose lui-même son prérequis.
        crate::registry::create_private_dir(&crate::registry::registry_dir()).unwrap();
        let write = |name: &str, addr: &SocketAddr, instance: u64| {
            let doc = serde_json::json!({
                "name": name, "version": "1.0.0", "addr": addr.to_string(),
                "port": addr.port(), "instance": instance, "pid": u64::from(u32::MAX - 1),
            });
            let path = dir.join(format!("{name}.json"));
            fs::write(&path, doc.to_string()).unwrap();
            path
        };

        // Vivante vérifiée : conservée.
        let vivante_addr = health_responder(0x1111_1111_1111_1111);
        let vivante = write("vivante", &vivante_addr, 0x1111_1111_1111_1111);
        // Usurpée : l'adresse répond avec une AUTRE identité.
        let usurpee_addr = health_responder(0x2222_2222_2222_2222);
        let usurpee = write("usurpee", &usurpee_addr, 0xDEAD_DEAD_DEAD_DEAD);
        // Sans réponse : conservée en doute (l'auditeur accepte, ne répond
        // jamais — comme un serveur saturé ou derrière un pare-feu muet).
        let auditeur_muet = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mutee_addr = auditeur_muet.local_addr().unwrap();
        std::thread::spawn(move || {
            let _ = auditeur_muet.accept(); // ne répond jamais
        });
        let mutee = write("mutee", &mutee_addr, 0x4444_4444_4444_4444);
        // Sans identité publiée (version antérieure) : doute sans sonde.
        let ancienne = dir.join("ancienne.json");
        fs::write(
            &ancienne,
            serde_json::json!({ "name": "ancienne", "port": 1, "pid": u64::from(u32::MAX - 1) })
                .to_string(),
        )
        .unwrap();

        let report = sweep_dir(&dir);
        assert_eq!(report.alive, 1, "identité vérifiée = vivante : {report:?}");
        assert_eq!(
            report.reaped, 1,
            "usurpation prouvée = retrait : {report:?}"
        );
        assert_eq!(
            report.undecided, 2,
            "sans réponse + sans identité = doute : {report:?}"
        );
        assert!(
            vivante.exists(),
            "une instance vivante vérifiée est respectée"
        );
        assert!(!usurpee.exists(), "l'annonce usurpée est retirée");
        assert!(
            mutee.exists(),
            "l'annonce sans réponse est conservée (doute)"
        );
        assert!(
            ancienne.exists(),
            "l'annonce sans identité est conservée (doute)"
        );
        // Compteurs disjoints : chaque fichier visité tombe dans une case.
        assert_eq!(
            report.alive
                + report.reaped
                + report.undecided
                + report.corrupt
                + report.foreign
                + report.own_skipped
                + report.skipped
                + report.debris,
            4
        );
        let _ = fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------------------------
    // Durcissement de la lecture (revue 2026-11-20, § 2.1 et 3.1)
    // ------------------------------------------------------------------

    /// Chemin scratch unique, **non créé** — pour les cas où le test doit
    /// poser lui-même le type d'entrée (lien, FIFO).
    fn chemin_scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "litemcp-az-lecture-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// Dossier scratch vide, créé pour le test.
    fn dossier_scratch(tag: &str) -> std::path::PathBuf {
        let dir = chemin_scratch(tag);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Annonce **complète** (addr + instance) pointant vers `addr` : si le
    /// sweep la lisait, la sonde ouvrirait une connexion vers l'auditeur.
    fn annonce_complete(addr: SocketAddr, instance: u64) -> String {
        serde_json::json!({
            "name": "piegee", "version": "1.0.0", "addr": addr.to_string(),
            "port": addr.port(), "instance": instance,
            "pid": u64::from(u32::MAX - 1),
        })
        .to_string()
    }

    /// Auditeur muet qui **compte** les connexions acceptées (sans jamais
    /// répondre) : le compteur dit si le sweep a sondé.
    fn auditeur_compteur() -> (SocketAddr, std::sync::Arc<AtomicUsize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let compte = std::sync::Arc::new(AtomicUsize::new(0));
        let compteur = std::sync::Arc::clone(&compte);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stream.is_err() {
                    break;
                }
                compteur.fetch_add(1, Ordering::Relaxed);
            }
        });
        (addr, compte)
    }

    /// Connexions comptées après une connexion de **contrôle** (la nôtre) :
    /// 1 = le sweep n'a pas sondé ; plus de 1 = il a ouvert une connexion.
    fn connexions_apres_controle(addr: SocketAddr, compte: &AtomicUsize) -> usize {
        let _controle = TcpStream::connect(addr).expect("auditeur de contrôle joignable");
        let deadline = Instant::now() + Duration::from_secs(2);
        while compte.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        // L'acceptation est asynchrone : laisse à une éventuelle connexion de
        // sonde le temps d'être comptée avant de conclure.
        std::thread::sleep(Duration::from_millis(50));
        compte.load(Ordering::Relaxed)
    }

    #[cfg(unix)]
    #[test]
    fn un_lien_symbolique_vers_une_annonce_hors_dossier_est_ignore() {
        let dir = dossier_scratch("lien");
        let dehors = dossier_scratch("lien-cible");
        let (addr, compte) = auditeur_compteur();
        // La cible est une annonce **valide** hors du dossier : suivre le lien
        // la lirait et la sonde partirait vers l'auditeur.
        let cible = dehors.join("piegee.json");
        fs::write(&cible, annonce_complete(addr, 0xAAAA_AAAA_AAAA_AAAA)).unwrap();
        let lien = dir.join("piegee.json");
        std::os::unix::fs::symlink(&cible, &lien).unwrap();

        let report = sweep_dir(&dir);
        assert!(
            fs::symlink_metadata(&lien).is_ok(),
            "le lien n'est pas détruit : {report:?}"
        );
        assert_eq!(
            report.foreign, 1,
            "un lien n'est pas un fichier d'annonce : {report:?}"
        );
        assert_eq!(
            report.skipped, 0,
            "un lien n'est même pas ouvert : {report:?}"
        );
        assert!(cible.exists(), "la cible hors dossier est intacte");
        assert_eq!(
            connexions_apres_controle(addr, &compte),
            1,
            "aucune sonde ne doit partir d'un lien : {report:?}"
        );
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&dehors);
    }

    #[test]
    fn un_fichier_au_dela_du_plafond_n_est_ni_lu_ni_sonde() {
        let dir = dossier_scratch("volumineux");
        let (addr, compte) = auditeur_compteur();
        // JSON **valide** complété d'espaces : sans plafond, le parseur
        // l'accepterait et la sonde partirait. Le plafond seul décide.
        let mut body = annonce_complete(addr, 0xBBBB_BBBB_BBBB_BBBB);
        body.push_str(&" ".repeat(ANNOUNCEMENT_MAX_BYTES as usize + 1));
        let path = dir.join("volumineuse.json");
        fs::write(&path, body).unwrap();

        let report = sweep_dir(&dir);
        assert_eq!(
            report.skipped, 1,
            "au-delà du plafond : laissé sans lecture : {report:?}"
        );
        assert_eq!(
            report.corrupt, 0,
            "un fichier trop gros n'est jamais détruit : {report:?}"
        );
        assert!(path.exists(), "le fichier reste en place");
        assert_eq!(
            connexions_apres_controle(addr, &compte),
            1,
            "aucune sonde ne part d'un fichier non lu"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Le nommage des `.tmp` d'annonce couvre le suffixe aléatoire actuel
    /// **et** l'ancien suffixe en nanosecondes (reliques à nettoyer).
    #[test]
    fn le_nommage_des_tmp_couvre_l_alea_et_l_ancien_suffixe() {
        assert!(announcement_tmp_name(
            "annonce.json.123.0123456789abcdef.tmp"
        ));
        assert!(announcement_tmp_name("annonce.json.123.456.tmp"));
        assert!(!announcement_tmp_name(
            "annonce.json.123.0123456789abcde.tmp"
        ));
        assert!(!announcement_tmp_name("annonce.json.123..tmp"));
        assert!(!announcement_tmp_name("sauvegarde.tmp"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn un_fifo_nomme_json_est_ignore_sans_bloquer_le_sweep() {
        let dir = dossier_scratch("fifo");
        let fifo = dir.join("x.json");
        let created = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !created {
            // Pas de `mkfifo` (ou système de fichiers qui le refuse) : le cas
            // n'est pas couvrable ici — sans `unsafe`, pas de `mkfifo(3)`
            // direct. Le test s'auto-ignore plutôt que d'échouer.
            let _ = fs::remove_dir_all(&dir);
            return;
        }
        // Le sweep tourne dans un thread : s'il ouvrait le FIFO, il
        // bloquerait indéfiniment (aucun écrivain) — le délai le prouve.
        let (tx, rx) = std::sync::mpsc::channel();
        let dossier = dir.clone();
        std::thread::spawn(move || {
            let _ = tx.send(sweep_dir(&dossier));
        });
        let report = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("un FIFO nommé .json ne doit pas bloquer le sweep");
        assert_eq!(
            report.foreign, 1,
            "un FIFO n'est pas un fichier d'annonce : {report:?}"
        );
        assert!(
            fs::symlink_metadata(&fifo).is_ok(),
            "le FIFO n'est pas détruit"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn un_dossier_de_registre_non_valide_n_est_ni_lu_ni_sonde() {
        let cible = dossier_scratch("registre-cible");
        let (addr, compte) = auditeur_compteur();
        fs::write(
            cible.join("piegee.json"),
            annonce_complete(addr, 0xCCCC_CCCC_CCCC_CCCC),
        )
        .unwrap();
        // Un lien symbolique au chemin du registre : `validate_private_dir`
        // le refuse (jamais suivi) — le sweep doit rendre un rapport vide
        // **avant** toute lecture, donc aucune sonde ne part.
        let lien = chemin_scratch("registre-lien");
        std::os::unix::fs::symlink(&cible, &lien).unwrap();

        let report = sweep_registry_at(&lien);
        assert!(
            report.is_empty(),
            "dossier non validé : rapport vide, rien n'est lu : {report:?}"
        );
        assert_eq!(
            connexions_apres_controle(addr, &compte),
            1,
            "aucune sonde ne part d'un dossier non validé"
        );
        assert!(
            cible.join("piegee.json").exists(),
            "aucun fichier n'est touché"
        );
        let _ = fs::remove_file(&lien);
        let _ = fs::remove_dir_all(&cible);
    }

    #[cfg(unix)]
    #[test]
    fn un_fichier_d_un_autre_uid_est_ignore_sans_lecture() {
        let dir = dossier_scratch("uid");
        let (addr, compte) = auditeur_compteur();
        let path = dir.join("etranger.json");
        fs::write(&path, annonce_complete(addr, 0xDDDD_DDDD_DDDD_DDDD)).unwrap();
        // Changer le propriétaire exige root (conteneur Linux) : sans ce
        // droit, le cas n'est pas couvrable — test auto-ignoré.
        let changed = std::process::Command::new("chown")
            .args(["12345:12345"])
            .arg(&path)
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !changed {
            let _ = fs::remove_dir_all(&dir);
            return;
        }
        let report = sweep_dir(&dir);
        assert_eq!(
            report.foreign, 1,
            "uid différent du dossier : ignoré : {report:?}"
        );
        assert_eq!(
            connexions_apres_controle(addr, &compte),
            1,
            "un fichier d'un autre uid n'est jamais sondé"
        );
        assert!(path.exists(), "le fichier d'un autre uid n'est pas détruit");
        let _ = fs::remove_dir_all(&dir);
    }
}
