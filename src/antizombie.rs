//! **Anti-zombie** — la fin de vie d'un serveur LiteMCP est vérifiable.
//!
//! Trois promesses, opt-in via
//! [`anti_zombie`](crate::server::ServerBuilder::anti_zombie) :
//!
//! 1. **Aucune instance morte ne traîne.** Au bind, le registre d'annonce
//!    partagé ([`registry_dir`](crate::registry::registry_dir)) est passé au
//!    peigne fin : une annonce dont le port ne répond plus est le fantôme
//!    d'un process tué brutalement (son `Drop` n'a jamais couru, le retrait
//!    n'a pas eu lieu) — elle est retirée. Les débris d'écritures
//!    interrompues sont nettoyés : un `.json` présent mais non parsable ne
//!    peut pas être une écriture en cours (l'écriture d'annonce est
//!    **atomique** par rename — un lecteur ne voit jamais un JSON à moitié
//!    écrit), et un `.tmp` orphelin de plus de
//!    [`DEBRIS_AGE`](crate::antizombie::DEBRIS_AGE) non plus.
//!    Les annonces d'instances vivantes, les fichiers d'un schéma inconnu
//!    et l'annonce du process courant sont laissés intacts.
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
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Délai de la sonde de vie d'un port (`TcpStream::connect_timeout` sur
/// `127.0.0.1`). Sur le bouclage, un port sans auditeur refuse la connexion
/// en moins d'une milliseconde : la borne ne sert que si un pare-feu avale
/// les SYN — le sweep reste alors borné.
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// Durée maximale du **drain** de fin de vie (défaut du builder, voir
/// [`crate::server::ServerBuilder::drain_timeout`]). Les threads ne sont
/// jamais tués : au-delà, le rapport signale `drained: false`.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Budget **total** d'un sweep : au-delà, les fichiers restants sont laissés
/// sans être sondés (`skipped`). Un dossier d'annonce normal contient de
/// l'ordre de l'unité de fichiers — le budget ne mord que sur un dossier
/// pathologiquement peuplé.
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
    /// Annonces de process morts retirées : le port annoncé ne répond plus
    /// et le PID n'est pas celui du process courant.
    pub reaped: usize,
    /// Annonces d'instances **vivantes** (port répondant) — laissées en place.
    pub alive: usize,
    /// Fichiers `.json` illisibles ou non parsables — débris d'une écriture
    /// interrompue (impossible autrement : l'écriture est atomique) —
    /// retirés.
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
    /// Fichiers `.json` non visités : budget [`SWEEP_BUDGET`] épuisé, ou
    /// suppression impossible (droits, verrou) — laissés en place.
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
    /// Threads de handler encore vivants à la fin de vie — mode borné
    /// uniquement ([`tool_timeout`](crate::server::ServerBuilder::tool_timeout))
    /// : sans timeout, les handlers tournent dans le thread de connexion et
    /// ce compteur vaut zéro. Un handler accroché retient son slot jusqu'à
    /// sa terminaison effective ; le rapport le rend visible au lieu de le
    /// taire.
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

    /// Fin de vie : drain **borné** des threads de connexion puis bilan.
    ///
    /// Appelé par le `Drop` du [`BoundServer`] — donc sur **tous** les
    /// chemins de fin de vie (arrêt gracieux, serveur lié jamais servi,
    /// abandon après un bind partiel). L'attente est plafonnée par
    /// `drain_timeout` : un `Drop` ne peut pas pendre indéfiniment.
    pub(crate) fn finish(
        &self,
        live_connections: &AtomicUsize,
        handlers_alive: usize,
    ) -> ZombieReport {
        let deadline = Instant::now() + self.drain_timeout;
        while live_connections.load(Ordering::Relaxed) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        let connections_pending = live_connections.load(Ordering::Relaxed);
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

/// Passe `dir` au peigne fin : retire les annonces de process morts (port
/// sans réponse, PID étranger), les `.json` non parsables et les `.tmp`
/// orphelins ; respecte les instances vivantes, les fichiers d'un schéma
/// inconnu et l'annonce du process courant. Dossier absent ou vide :
/// rapport vide — jamais d'erreur.
///
/// Utilisable périodiquement sans serveur ; le budget total est
/// [`SWEEP_BUDGET`], chaque sonde de port est bornée par [`PROBE_TIMEOUT`].
#[must_use]
pub fn sweep_dir(dir: &Path) -> SweepReport {
    let mut report = SweepReport::default();
    let Ok(entries) = fs::read_dir(dir) else {
        return report; // dossier absent ou illisible : rien à faire
    };
    let deadline = Instant::now() + SWEEP_BUDGET;
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
        if Instant::now() >= deadline {
            report.skipped += 1;
            continue;
        }
        let body = match fs::read_to_string(&path) {
            Ok(body) => body,
            Err(_) => {
                // Illisible : inutilisable en l'état. Le retrait est
                // best-effort — s'il échoue (droits, verrou), le fichier
                // reste et sera rejugé au prochain sweep.
                report.corrupt += 1;
                let _ = fs::remove_file(&path);
                continue;
            }
        };
        let Ok(doc) = serde_json::from_str::<serde_json::Value>(&body) else {
            // Présent mais non parsable : l'écriture atomique par rename
            // garantit qu'aucune écriture en cours ne peut être observée à
            // moitié — c'est un débris corrompu, pas une annonce vivante.
            report.corrupt += 1;
            let _ = fs::remove_file(&path);
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
            // `announce` ne publie jamais un port 0 : schéma inconnu.
            report.foreign += 1;
            continue;
        }
        if port_alive(port) {
            report.alive += 1;
            continue;
        }
        // Port sans réponse : le process annoncé est mort (ou n'a jamais
        // existé). `NotFound` : un sweep concurrent est passé avant nous —
        // même verdict.
        match fs::remove_file(&path) {
            Ok(()) => report.reaped += 1,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => report.reaped += 1,
            Err(_) => report.skipped += 1,
        }
    }
    report
}

/// Le port répond-il encore sur le bouclage ? Un connect **réussi** prouve
/// qu'un auditeur vit (le sweep laisse alors la place) ; un refus ou un
/// dépassement de [`PROBE_TIMEOUT`] prouve la mort.
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
    fn un_port_ecoute_est_vivant_un_port_libre_est_mort() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(port_alive(port), "un auditeur réel doit répondre");
        drop(listener);
        assert!(!port_alive(port), "le port libéré doit paraître mort");
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
        let report = state.finish(&live, 0);
        assert!(report.drained);
        assert_eq!(report.sweep.reaped, 2);
        assert_eq!(state.report(), Some(report), "lisible après la fin de vie");
    }
}
