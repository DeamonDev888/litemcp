//! Progression **request-scoped** — `notifications/progress` (spec MCP).
//!
//! Un handler n'écrit jamais sur le socket : il appelle
//! [`report`](crate::progress::report), qui pousse la notification au canal du **transport de la
//! requête courante** (s'il en existe un). Le canal est lié au fil du
//! handler pour la durée du dispatch — c'est ce qui rend l'API utilisable
//! depuis les handlers synchrones existants, sans changer leur signature
//! ni introduire de runtime async.
//!
//! # Quand une notification part-elle ?
//!
//! La spec réserve `notifications/progress` aux requêtes qui ont fourni un
//! `params._meta.progressToken` ; le jeton est **toujours** repris du corps
//! de la requête par le transport, jamais choisi par le handler. Sans
//! jeton, sans flux SSE accepté par le client, ou une fois la réponse
//! partie, [`report`](crate::progress::report) est une **non-action** (`false`) : le handler
//! s'exécute normalement, rien n'est écrit.
//!
//! ```no_run
//! use litemcp::prelude::*;
//!
//! ServerBuilder::new("long-job", "1.0.0")
//!     .tool("index", "Indexe un corpus", |_args| {
//!         let _ = litemcp::progress::report(0.0, Some(100.0), Some("starting"));
//!         // … travail …
//!         let _ = litemcp::progress::report(100.0, Some(100.0), None);
//!         Ok(serde_json::json!({ "done": true }))
//!     })
//!     .bind("127.0.0.1:23080")?
//!     .serve()?;
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! # Portée et limites
//!
//! * Le canal est **par requête** : il ne survit pas à la réponse et une
//!   émission tardive (handler détaché après [`tool_timeout`]) est ignorée ;
//! * il est lié au **fil** du handler : une valeur `progress` reste locale
//!   au tour courant, aucun état de connexion n'est retenu ;
//! * `progress` et `total` doivent être finis (NaN/infini → ignoré) ;
//! * sans transport streaming (dispatch embarqué, client JSON seul), aucun
//!   canal n'est installé : `report` rend `false`.
//!
//! [`tool_timeout`]: crate::server::ServerBuilder::tool_timeout

use std::cell::RefCell;
use std::sync::Arc;

/// Canal de progression d'une requête, fourni par le transport.
///
/// Le transport qui sait livrer `notifications/progress` pendant
/// l'exécution (SSE request-scoped) en implémente un ; le cœur lui délègue
/// chaque [`report`]. Le canal reçoit des valeurs **déjà validées** (finies)
/// et reste maître du jeton `progressToken` : un handler ne peut pas le
/// choisir ni le falsifier.
pub trait ProgressChannel: Send + Sync {
    /// Émet une notification pour la requête courante.
    ///
    /// Rend `true` si la notification a été remise au client, `false` si le
    /// canal est fermé (réponse déjà partie, déconnexion) — le handler n'a
    /// rien à faire de ce verdict.
    fn progress(&self, progress: f64, total: Option<f64>, message: Option<&str>) -> bool;
}

thread_local! {
    /// Canal du dispatch en cours sur ce fil — `None` hors requête ou sur un
    /// transport sans flux. `RefCell` : le dispatch est synchrone, aucune
    /// réentrance concurrente sur un même fil.
    static CURRENT: RefCell<Option<Arc<dyn ProgressChannel>>> = const { RefCell::new(None) };
}

/// Émet une notification de progression pour la requête en cours.
///
/// Sans canal (client JSON, dispatch embarqué, réponse déjà envoyée), c'est
/// une non-action qui rend `false` — jamais une erreur : rapporter la
/// progression ne doit pas faire échouer un handler.
///
/// `progress` et `total` doivent être finis ; une valeur NaN/infinie est
/// ignorée (elle ne serait pas représentable en JSON).
pub fn report(progress: f64, total: Option<f64>, message: Option<&str>) -> bool {
    if !progress.is_finite() || total.is_some_and(|value| !value.is_finite()) {
        return false;
    }
    CURRENT.with(|slot| match slot.borrow().as_ref() {
        Some(channel) => channel.progress(progress, total, message),
        None => false,
    })
}

/// Canal actif sur ce fil — capturé avant de lancer un handler dans un
/// autre fil pour le réinstaller là-bas.
pub(crate) fn current() -> Option<Arc<dyn ProgressChannel>> {
    CURRENT.with(|slot| slot.borrow().clone())
}

/// Garde de liaison : restaure le canal précédent au `Drop`, y compris sur
/// un retour anticipé du dispatch.
pub(crate) struct Bound(Option<Arc<dyn ProgressChannel>>);

impl Drop for Bound {
    fn drop(&mut self) {
        let previous = self.0.take();
        CURRENT.with(|slot| *slot.borrow_mut() = previous);
    }
}

/// Lie (ou délie avec `None`) le canal de ce fil pour la durée du dispatch.
pub(crate) fn bind(channel: Option<Arc<dyn ProgressChannel>>) -> Bound {
    Bound(CURRENT.with(|slot| slot.replace(channel)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    type LastProgress = std::sync::Mutex<Option<(f64, Option<f64>, Option<String>)>>;

    struct Recorder {
        calls: AtomicUsize,
        last: LastProgress,
    }

    impl ProgressChannel for Recorder {
        fn progress(&self, progress: f64, total: Option<f64>, message: Option<&str>) -> bool {
            self.calls.fetch_add(1, Ordering::Relaxed);
            *self.last.lock().unwrap() = Some((
                progress,
                total,
                message.map(std::string::ToString::to_string),
            ));
            true
        }
    }

    #[test]
    fn report_without_channel_is_a_no_op() {
        assert!(!report(10.0, Some(100.0), None));
    }

    #[test]
    fn report_reaches_the_bound_channel_and_unbinds_on_drop() {
        let recorder = Arc::new(Recorder {
            calls: AtomicUsize::new(0),
            last: std::sync::Mutex::new(None),
        });
        {
            let _bound = bind(Some(recorder.clone()));
            assert!(report(50.0, Some(100.0), Some("half")));
            assert!(report(60.0, None, None));
        }
        assert_eq!(recorder.calls.load(Ordering::Relaxed), 2);
        assert_eq!(
            recorder.last.lock().unwrap().clone(),
            Some((60.0, None, None))
        );
        // Le garde a restauré l'absence de canal.
        assert!(!report(70.0, None, None));
    }

    #[test]
    fn non_finite_values_are_ignored() {
        let recorder = Arc::new(Recorder {
            calls: AtomicUsize::new(0),
            last: std::sync::Mutex::new(None),
        });
        let _bound = bind(Some(recorder.clone()));
        assert!(!report(f64::NAN, None, None));
        assert!(!report(1.0, Some(f64::INFINITY), None));
        assert_eq!(recorder.calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn nested_binding_restores_the_previous_channel() {
        let outer = Arc::new(Recorder {
            calls: AtomicUsize::new(0),
            last: std::sync::Mutex::new(None),
        });
        let inner = Arc::new(Recorder {
            calls: AtomicUsize::new(0),
            last: std::sync::Mutex::new(None),
        });
        let _outer = bind(Some(outer.clone()));
        {
            let _inner = bind(Some(inner.clone()));
            assert!(report(1.0, None, None));
        }
        assert!(report(2.0, None, None));
        assert_eq!(inner.calls.load(Ordering::Relaxed), 1);
        assert_eq!(outer.calls.load(Ordering::Relaxed), 1);
    }
}
