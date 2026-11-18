//! Annulation **coopérative** — fermeture du flux SSE (spec Streamable HTTP
//! `2026-07-28`), à la discrétion du handler.
//!
//! La révision `2026-07-28` exige (« MUST ») que la fermeture du flux SSE
//! d'une requête soit traitée comme une **annulation** de cette requête.
//! Rust n'a pas d'annulation de thread : le transport ne peut pas interrompre
//! un handler. LiteMCP expose donc un **drapeau coopératif** — un
//! [`Arc<AtomicBool>`](std::sync::atomic::AtomicBool) par requête — que le
//! handler consulte aux points sûrs de son choix :
//!
//! ```no_run
//! use litemcp::prelude::*;
//!
//! ServerBuilder::new("long-job", "1.0.0")
//!     .tool("index", "Indexe un corpus", |_args| {
//!         for document in ["a", "b", "c"] {
//!             if litemcp::cancel::is_cancelled() {
//!                 // Le client a fermé le flux : sortie propre, sans effet de
//!                 // bord supplémentaire. Rendre un résultat reste licite —
//!                 // personne ne le lira.
//!                 return Err(ToolError::timeout("cancelled by client"));
//!             }
//!             // … traitement du document …
//!             let _ = litemcp::progress::report(1.0, Some(3.0), None);
//!         }
//!         Ok(serde_json::json!({ "done": true }))
//!     })
//!     .bind("127.0.0.1:23080")?
//!     .serve()?;
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! # Contrat — ce qui est garanti, et ce qui ne l'est pas
//!
//! * Le drapeau est **levé par le transport** dès qu'une écriture sur le flux
//!   SSE échoue (client déconnecté, connexion fermée pendant l'exécution) —
//!   c'est la seule détection possible : un pair qui a fermé son côté ne
//!   s'annonce pas, on l'apprend à la première écriture qui échoue. Le
//!   drapeau est monotone : une fois levé, il le reste jusqu'à la fin de la
//!   requête.
//! * **Aucune interruption n'est imposée** : le handler reste maître de sa
//!   sortie. Un handler qui ne consulte jamais le drapeau s'exécute jusqu'au
//!   bout, exactement comme avant — l'annulation est **coopérative**, pas
//!   préemptive. La borne temporelle, elle, vient de
//!   [`tool_timeout`](crate::server::ServerBuilder::tool_timeout).
//! * Le drapeau est **request-scoped** : lié au fil du handler pour la durée
//!   du dispatch, il ne survit pas à la réponse. Hors flux SSE (client JSON,
//!   dispatch embarqué), aucun jeton n'est installé :
//!   [`is_cancelled`](crate::cancel::is_cancelled) rend toujours `false` et
//!   rien n'est alloué.
//! * Un handler qui répartit son travail sur ses propres fils récupère un
//!   [`CancelToken`](crate::cancel::CancelToken) clonable via
//!   [`token`](crate::cancel::token) et le passe à ses fils : le jeton, lui,
//!   traverse les fils (l'état est partagé, `Arc`).
//!
//! # API
//!
//! * [`is_cancelled`](crate::cancel::is_cancelled) — verdict pour le fil du
//!   handler courant ;
//! * [`token`](crate::cancel::token) — jeton clonable, à passer explicitement
//!   à un fil dérivé ;
//! * [`CancelToken`](crate::cancel::CancelToken) — `cancel()` (réservé au
//!   transport et aux tests) et `is_cancelled()`.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Jeton d'annulation d'une requête — clonable, partagé entre fils.
///
/// Tous les clones observent le **même** drapeau : `cancel()` sur l'un rend
/// `is_cancelled()` vrai sur tous les autres. Le transport en détient un et
/// le lève quand le flux SSE échoue ; un handler en récupère un via
/// [`token`] pour le transmettre à ses propres fils.
///
/// Le jeton est volontairement **monotone** : il n'existe pas de
/// `reset()` — une annulation ne se « dé-annule » pas.
#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl CancelToken {
    /// Nouveau jeton, non annulé.
    ///
    /// Une allocation par jeton (`Arc<AtomicBool>`) : le transport n'en crée
    /// un que pour une requête servie en flux SSE avec `progressToken`, où
    /// une allocation a déjà lieu pour le canal. Le chemin nominal (JSON) n'en
    /// crée aucun.
    #[must_use]
    pub fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Lève le drapeau — idempotent.
    ///
    /// Réservé au transport (échec d'écriture SSE) et aux tests : un handler
    /// qui appellerait `cancel()` annulerait sa propre requête.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
    }

    /// Le drapeau est-il levé ?
    ///
    /// `true` dès qu'une écriture SSE a échoué pour cette requête — y compris
    /// depuis un clone détenu par un autre fil.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }
}

thread_local! {
    /// Jeton du dispatch en cours sur ce fil — `None` hors requête SSE (ou
    /// une fois le dispatch terminé). `RefCell` : le dispatch est synchrone,
    /// aucune réentrance concurrente sur un même fil.
    static CURRENT: RefCell<Option<CancelToken>> = const { RefCell::new(None) };
}

/// La requête en cours sur ce fil est-elle annulée ?
///
/// `false` sans flux SSE (client JSON, dispatch embarqué, réponse déjà
/// partie) : c'est une **non-action**, jamais une erreur. Un handler peut
/// l'appeler à chaque itération d'une boucle longue sans coût mesurable (une
/// charge atomique).
#[must_use]
pub fn is_cancelled() -> bool {
    CURRENT.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(CancelToken::is_cancelled)
    })
}

/// Jeton de la requête en cours sur ce fil, s'il en existe un.
///
/// À récupérer **avant** de lancer un fil dérivé : le jeton y est ensuite
/// consultable directement (`token.is_cancelled()`), l'état étant partagé.
/// `None` hors flux SSE — un handler qui n'obtient rien n'a rien à
/// surveiller.
#[must_use]
pub fn token() -> Option<CancelToken> {
    CURRENT.with(|slot| slot.borrow().clone())
}

/// Garde de liaison : restaure le jeton précédent au `Drop`, y compris sur
/// un retour anticipé du dispatch.
pub(crate) struct Bound(Option<CancelToken>);

impl Drop for Bound {
    fn drop(&mut self) {
        let previous = self.0.take();
        CURRENT.with(|slot| *slot.borrow_mut() = previous);
    }
}

/// Lie (ou délie avec `None`) le jeton de ce fil pour la durée du dispatch.
pub(crate) fn bind(token: Option<CancelToken>) -> Bound {
    Bound(CURRENT.with(|slot| slot.replace(token)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_token_nothing_is_cancelled() {
        assert!(!is_cancelled());
        assert!(token().is_none());
    }

    #[test]
    fn a_cancelled_token_is_seen_by_the_bound_thread_and_its_clones() {
        let token = CancelToken::new();
        let clone = token.clone();
        {
            let _bound = bind(Some(token.clone()));
            assert!(!is_cancelled(), "un jeton neuf n'annule rien");
            clone.cancel();
            assert!(is_cancelled(), "l'annulation traverse les clones");
            assert!(super::token().expect("jeton lié").is_cancelled());
        }
        // Le garde a restauré l'absence de jeton.
        assert!(!is_cancelled());
        assert!(token.is_cancelled(), "le jeton lui-même reste annulé");
    }

    #[test]
    fn cancel_is_monotonic_and_idempotent() {
        let token = CancelToken::new();
        token.cancel();
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[test]
    fn nested_binding_restores_the_previous_token() {
        let outer = CancelToken::new();
        let inner = CancelToken::new();
        let _outer = bind(Some(outer.clone()));
        {
            let _inner = bind(Some(inner.clone()));
            assert!(!is_cancelled());
            inner.cancel();
            assert!(is_cancelled());
        }
        assert!(!is_cancelled(), "le jeton extérieur n'a pas été annulé");
        outer.cancel();
        assert!(is_cancelled());
    }

    #[test]
    fn a_token_reaches_another_thread() {
        let token = CancelToken::new();
        let remote = token.clone();
        let handle = std::thread::spawn(move || {
            assert!(!remote.is_cancelled());
            remote.cancel();
        });
        handle.join().expect("fil de test");
        assert!(
            token.is_cancelled(),
            "l'annulation faite dans un autre fil est visible"
        );
    }
}
