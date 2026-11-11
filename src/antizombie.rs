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
//! de handler bornés du [`ServerCore`](crate::server::ServerCore)) existent
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

/// Âge au-delà duquel un `.tmp` du registre est un débris (l'écriture d'une
/// annonce se termine en millisecondes) — un `.tmp` récent peut être le
/// fichier temporaire d'un écrivain concurrent **vivant** : il est respecté.
pub const DEBRIS_AGE: Duration = Duration::from_secs(60);

/// Bilan d'un **sweep** du registre d'annonce — les compteurs sont
/// disjoints : chaque fichier du dossier tombe dans exactement une case
/// (ou n'est pas visité, voir [`SweepReport::skipped`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
    /// Fichiers d'un schéma inconnu (JSON valide sans `port` utilisable) —
    /// laissés en place : ils pourraient appartenir à une version future du
    /// format d'annonce.
    pub foreign: usize,
    /// Annonces du process courant — ignorées : le serveur lié les gère
    /// lui-même (retrait dans le `Drop` de
    /// [`BoundServer`](crate::http::BoundServer)).
    pub own_skipped: usize,
    /// Fichiers `.tmp` orphelins (plus vieux que [`DEBRIS_AGE`]) retirés.
    pub debris: usize,
    /// Fichiers `.json` laissés **sans sonde** : budget
    /// [`SWEEP_BUDGET`] de sondes épuisé, fichier illisible (partage,
    /// droits — l'annonce d'une instance vivante peut être momentanément
    /// inaccessible : jamais détruite sur un doute), suppression impossible
    /// (droits, verrou — y compris le retrait d'un débris), ou remplacés
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
#[must_use]
pub fn sweep_registry() -> SweepReport {
    sweep_dir(&crate::registry::registry_dir())
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
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(std::ffi::OsStr::to_str) else {
            continue;
        };
        // Débris d'écriture : les `.tmp` de `write_atomically` ne sont
        // jamais lus par les consommateurs. Un `.tmp` **récent** peut être
        // celui d'un écrivain vivant — seul un orphelin (vieux) part.
        if name.ends_with(".tmp") {
            if older_than(&path, DEBRIS_AGE) && fs::remove_file(&path).is_ok() {
                report.debris += 1;
            }
            continue;
        }
        if !name.ends_with(".json") {
            continue; // jamais une annonce : respecté
        }
        let body = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(_) => {
                // Illisible (partage Windows, droits, verrou) : l'annonce
                // d'une instance VIVANTE peut être momentanément
                // inaccessible — ce n'est pas une preuve de débris. Doute :
                // conservée sans sonde, rejugée au prochain sweep.
                report.skipped += 1;
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
            let relecture = fs::read_to_string(&path)
                .ok()
                .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok());
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
                // Disparue : un sweep concurrent ou le propriétaire l'a
                // retirée — même verdict.
                None => true,
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
    fn un_fichier_illisible_est_conserve_en_doute_jamais_detruit() {
        let dir = std::env::temp_dir().join(format!(
            "litemcp-az-debris-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        // Un **dossier** nommé `*.json` : lecture impossible. Ce n'est pas
        // une preuve de débris (un fichier peut être momentanément
        // inaccessible : partage Windows, droits, verrou) — conservé.
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
            report.skipped, 1,
            "un fichier illisible est conservé et compté skipped : {report:?}"
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
}
