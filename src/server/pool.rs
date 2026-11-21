//! Fils de handlers : cache de fils résidents, gardes de slot et de
//! profondeur, exécution bornée par `tool_timeout`.

use super::*;

/// Garde RAII d'un slot de thread de handler détaché : décrémente le compteur
/// uniquement lorsque le thread système se termine réellement (même après un
/// timeout ou une panique).
struct HandlerSlotGuard(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for HandlerSlotGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// Issue d'une exécution bornée par [`ServerBuilder::tool_timeout`].
pub(super) enum Outcome<T> {
    /// Terminé dans le délai.
    Done(T),
    /// Le handler a paniqué (rattrapé dans son thread) ou le thread n'a pas
    /// pu être créé.
    Panicked,
    /// Délai dépassé — le thread du handler **continue** en arrière-plan
    /// (Rust ne peut pas tuer un thread), son résultat sera ignoré.
    TimedOut,
    /// Plafond de threads de handlers simultanés atteint — refuse de créer
    /// un nouveau thread noyau.
    Overloaded,
}

thread_local! {
    /// Profondeur d'exécution de handlers sur ce fil (`0` = hors handler).
    ///
    /// Un appel d'outil **imbriqué** (un handler qui re-dispatche sur son
    /// propre [`Gateway`](crate::Gateway), ou qui appelle un outil frère)
    /// s'exécute alors sur le fil de son appelant, sans empiler un thread de
    /// plus : il hérite de l'échéance du handler extérieur, et la garde
    /// anti-récursion de `Gateway` (thread-locale) reste effective. Sans ce
    /// marqueur, le délai par défaut ferait tourner chaque niveau de
    /// récursion dans un fil neuf — la garde ne verrait jamais qu'un seul
    /// niveau et 256 threads seraient consommés avant le refus `overloaded`.
    static HANDLER_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Garde de profondeur : rend le niveau au `Drop` (panique incluse).
pub(super) struct HandlerDepthGuard;

impl HandlerDepthGuard {
    pub(super) fn enter() -> Self {
        HANDLER_DEPTH.set(HANDLER_DEPTH.get() + 1);
        Self
    }
}

impl Drop for HandlerDepthGuard {
    fn drop(&mut self) {
        HANDLER_DEPTH.set(HANDLER_DEPTH.get().saturating_sub(1));
    }
}

/// Un handler s'exécute-t-il déjà sur ce fil ?
pub(super) fn in_handler() -> bool {
    HANDLER_DEPTH.get() > 0
}

/// Plafond de fils de handlers **résidents** (cache de fils réutilisés).
///
/// Un handler borné par [`ServerBuilder::tool_timeout`] ne s'exécute plus sur
/// un fil système neuf à chaque appel : un fil rendu au repos par un appel
/// précédent est réutilisé (créer un fil coûtait ~150 µs par `tools/call` sur
/// Windows, soit 40× le coût du dispatch complet). Le cache est **borné** :
/// au-delà de ce plafond, un appel repart sur un fil éphémère — exactement le
/// comportement antérieur — donc un handler bloqué ne peut pas assécher le
/// cache, et un pic de charge ne fait qu'ajouter des fils éphémères.
pub(super) const MAX_RESIDENT_HANDLERS: usize = 32;

/// Travail confié à un fil de handler : la closure complète (liaison des
/// variables de fil, garde de slot, envoi du résultat) est déjà dedans.
type HandlerJob = Box<dyn FnOnce() + Send + 'static>;

/// État partagé de la file de handlers.
struct FileHandlers {
    jobs: std::collections::VecDeque<HandlerJob>,
    /// Fils **au repos** : nombre de fils qui consommeront un job déposé
    /// dans `jobs`. Un worker s'y compte **en tenant le verrou**, avant de se
    /// mettre en attente, et s'en retire (toujours sous le verrou, avant de
    /// le relâcher) dès qu'il en sort — pour prendre un job ou pour se
    /// rendormir. Un fil occupé n'y figure donc jamais.
    ///
    /// Invariant exploité par `run` : `jobs.len() < attente` ⇒ le job déposé
    /// trouve preneur immédiatement, sans attendre la fin d'un autre handler.
    attente: usize,
    /// Le cache a été détruit : les fils au repos sortent à leur réveil.
    ferme: bool,
}

/// Cache de fils de handlers réutilisables — voir [`MAX_RESIDENT_HANDLERS`].
///
/// Le `Drop` referme la file : les fils au repos se réveillent et sortent (les
/// fils retenus par un handler bloqué terminent à leur rythme, comme avant).
/// Les fils ne retiennent que `queue` (allocation distincte), jamais le cache
/// lui-même : sans cela le `Drop` ne pourrait jamais s'exécuter.
pub(super) struct HandlerPool {
    queue: Arc<(std::sync::Mutex<FileHandlers>, std::sync::Condvar)>,
    /// Fils résidents vivants (au repos ou occupés).
    resident: Arc<std::sync::atomic::AtomicUsize>,
    max_resident: usize,
}

impl HandlerPool {
    pub(super) fn new(max_resident: usize) -> Self {
        Self {
            queue: Arc::new((
                std::sync::Mutex::new(FileHandlers {
                    jobs: std::collections::VecDeque::new(),
                    attente: 0,
                    ferme: false,
                }),
                std::sync::Condvar::new(),
            )),
            resident: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            max_resident,
        }
    }

    fn etat(&self) -> std::sync::MutexGuard<'_, FileHandlers> {
        self.queue
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Confie `job` à un fil au repos **libre**, sinon à un fil neuf.
    ///
    /// Le job n'attend **jamais** un fil occupé : sans fil au repos libre, un
    /// fil est créé (résident tant que le plafond le permet, éphémère
    /// ensuite). Cette règle est ce qui garde `tool_timeout` honnête — une
    /// attente en file entrerait dans le délai et couperait un handler
    /// innocent.
    fn run(&self, job: HandlerJob) {
        let mut etat = self.etat();
        if !etat.ferme {
            // « Libre » et non « existant » : `attente` ne comptabilise un
            // fil qu'il ne consommera pas encore, un fil déjà promis au job
            // précédent reste compté jusqu'à son réveil. Tester `attente > 0`
            // seul enfilait alors un second job derrière ce fil, qui le
            // servait après le premier — le délai du second courait sans que
            // son handler ait jamais démarré. `jobs.len() < attente` garantit
            // au contraire un preneur immédiat (invariant de `FileHandlers`).
            if etat.jobs.len() < etat.attente {
                // Un fil est **au repos** et libre : il se réveille et se
                // retire du compte lui-même en sortant de l'attente (voir
                // `worker`).
                etat.jobs.push_back(job);
                drop(etat);
                self.queue.1.notify_one();
                return;
            }
            // Aucun fil au repos **libre** : en créer un avant d'enfiler,
            // pour qu'un échec de création retombe sur le fil éphémère
            // (jamais sur un job enfilé que personne ne servirait).
            if self.resident.load(std::sync::atomic::Ordering::Acquire) < self.max_resident {
                let queue = Arc::clone(&self.queue);
                let resident = Arc::clone(&self.resident);
                self.resident
                    .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                let lance = std::thread::Builder::new()
                    .name("litemcp-handler".into())
                    .spawn(move || worker(queue, resident))
                    .is_ok();
                if lance {
                    etat.jobs.push_back(job);
                    drop(etat);
                    self.queue.1.notify_one();
                    return;
                }
                self.resident
                    .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            }
        }
        drop(etat);
        // Fil éphémère : il rend son slot et meurt, comme avant le cache.
        let _ = std::thread::Builder::new()
            .name("litemcp-handler".into())
            .spawn(move || {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
            });
    }
}

impl Drop for HandlerPool {
    fn drop(&mut self) {
        let mut etat = self.etat();
        etat.ferme = true;
        drop(etat);
        self.queue.1.notify_all();
    }
}

/// Boucle d'un fil résident : attend un job, l'exécute, se remet au repos.
fn worker(
    queue: Arc<(std::sync::Mutex<FileHandlers>, std::sync::Condvar)>,
    resident: Arc<std::sync::atomic::AtomicUsize>,
) {
    loop {
        let job = {
            let mut etat = queue
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Le fil se compte au repos **sous le verrou**, avant d'attendre :
            // dès cet instant `run` peut lui destiner un job, un job déposé
            // dans `jobs` sera consommé par lui au réveil (invariant de
            // `FileHandlers::attente`, seul fondement du test
            // `jobs.len() < attente` dans `run`).
            etat.attente += 1;
            while etat.jobs.is_empty() && !etat.ferme {
                etat = queue
                    .1
                    .wait(etat)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            // Sortie de l'attente : le fil n'est plus au repos, qu'il prenne
            // un job ou se rendorme. Le `continue` ci-dessous **relâche** le
            // `MutexGuard` (il quitte la portée du bloc) : `attente` peut donc
            // valoir 0 un instant avant que le fil ne se recompte au tour
            // suivant — la décrémentation et le recomptage ne sont pas
            // atomiques. L'invariant tient quand même : `attente` ne désigne
            // jamais un fil **occupé** (décrémenté sous le verrou), et un
            // `run` qui voit 0 crée un fil au lieu d'enfiler derrière un fil
            // occupé.
            etat.attente -= 1;
            if etat.jobs.is_empty() {
                if etat.ferme {
                    break;
                }
                continue;
            }
            etat.jobs.pop_front()
        };
        if let Some(job) = job {
            // Un handler qui panique ne doit pas emporter le fil résident.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
        }
    }
    resident.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
}

/// Exécute `f` borné par `timeout` — sur un fil de handler (résident
/// réutilisé quand il y en a un au repos, éphémère sinon), sous réserve du
/// plafond `max_inflight` de threads de handlers simultanés.
///
/// En cas de timeout, le thread de `f` n'est **pas** interrompu (impossible
/// en Rust) : il termine à son rythme et son résultat est ignoré, tout en
/// retenant son slot `inflight` jusqu'à sa terminaison effective afin
/// d'empêcher l'épuisement de la table de threads du noyau. La panique de `f`
/// est rattrapée dans son thread et rendue comme [`Outcome::Panicked`].
pub(super) fn run_with_timeout<T: Send + 'static>(
    pool: &HandlerPool,
    timeout: Duration,
    inflight: &Arc<std::sync::atomic::AtomicUsize>,
    max_inflight: usize,
    f: impl FnOnce() -> T + Send + 'static,
) -> Outcome<T> {
    let prev = inflight.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    if prev >= max_inflight {
        inflight.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        return Outcome::Overloaded;
    }
    let guard = HandlerSlotGuard(Arc::clone(inflight));
    let (tx, rx) = std::sync::mpsc::channel();
    // Le slot est rendu **avant** la remise du résultat : quand le fil de
    // connexion observe `Done`, le compteur est déjà décrémenté. Sans cet
    // ordre, le bilan anti-zombie lu juste après le drain pouvait compter
    // vivant un handler qui avait terminé (la remise du résultat précède la
    // fin du thread).
    pool.run(Box::new(move || {
        let result = std::panic::catch_unwind(AssertUnwindSafe(f));
        drop(guard);
        let _ = tx.send(result);
    }));
    match rx.recv_timeout(timeout) {
        Ok(Ok(value)) => Outcome::Done(value),
        Ok(Err(_)) => Outcome::Panicked,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Outcome::TimedOut,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Outcome::Panicked,
    }
}

#[cfg(test)]
mod tests_limites_2026_10_25 {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    /// Le plafond de threads de handlers refuse le surplus (`Overloaded`) et
    /// rend le slot dès que le handler détaché termine (garde `Drop`).
    #[test]
    fn inflight_cap_refuses_extra_handler_threads_and_releases_slots() {
        let pool = HandlerPool::new(MAX_RESIDENT_HANDLERS);
        let inflight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let barrier = Arc::new(std::sync::Barrier::new(2));

        let (b, i) = (Arc::clone(&barrier), Arc::clone(&inflight));
        let premier_pool = Arc::new(HandlerPool::new(MAX_RESIDENT_HANDLERS));
        let first = std::thread::spawn(move || {
            run_with_timeout(&premier_pool, Duration::from_secs(5), &i, 1, move || {
                b.wait();
                1u8
            })
        });

        let mut tours = 0;
        while inflight.load(Ordering::Acquire) == 0 {
            tours += 1;
            assert!(
                tours < 1_000_000,
                "le premier handler n'a jamais pris son slot"
            );
            std::thread::yield_now();
        }

        let second = run_with_timeout(&pool, Duration::from_secs(5), &inflight, 1, || 2u8);
        assert!(
            matches!(second, Outcome::Overloaded),
            "plafond d'inflight non appliqué"
        );

        barrier.wait();
        assert!(matches!(first.join().unwrap(), Outcome::Done(1)));

        let mut tours = 0;
        while inflight.load(Ordering::Acquire) != 0 {
            tours += 1;
            assert!(tours < 1_000_000, "slot inflight jamais rendu");
            std::thread::yield_now();
        }
    }

    /// Un handler qui a terminé ne retient plus son slot quand
    /// `run_with_timeout` rend `Done` : le bilan anti-zombie lu juste après
    /// le drain ne peut plus compter vivant un handler qui y a terminé.
    #[test]
    fn un_handler_termine_ne_reste_pas_compte_apres_la_remise_du_resultat() {
        let pool = HandlerPool::new(MAX_RESIDENT_HANDLERS);
        let inflight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for _ in 0..512 {
            let outcome = run_with_timeout(&pool, Duration::from_secs(5), &inflight, 4, || 7u8);
            assert!(matches!(outcome, Outcome::Done(7)));
            assert_eq!(
                inflight.load(Ordering::Acquire),
                0,
                "`Done` doit impliquer un slot déjà rendu (lecture post-drain fiable)"
            );
        }
    }

    /// Un fil résident **au repos** est réutilisé : le job suivant s'exécute
    /// sur le même fil système (aucune création), et un handler qui panique
    /// ne l'emporte pas.
    #[test]
    fn le_cache_de_fils_reutilise_un_fil_au_repos_et_survit_a_une_panique() {
        let pool = HandlerPool::new(MAX_RESIDENT_HANDLERS);
        let identifiant = |pool: &HandlerPool| {
            let inflight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let fils = Arc::new(std::sync::Mutex::new(None));
            let fils_job = Arc::clone(&fils);
            let outcome = run_with_timeout(pool, Duration::from_secs(5), &inflight, 4, move || {
                *fils_job
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some(std::thread::current().id());
                7u8
            });
            assert!(matches!(outcome, Outcome::Done(7)));
            let id = fils
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .expect("le job doit rendre son fil");
            (id, inflight)
        };
        let (premier, _) = identifiant(&pool);
        // Fenêtre de repos : le fil est garanti au repos (il s'est déclaré
        // avant l'attente), donc le job suivant DOIT le réutiliser.
        std::thread::sleep(Duration::from_millis(20));
        let (second, inflight) = identifiant(&pool);
        assert_eq!(premier, second, "un fil au repos doit être réutilisé");
        // Panique : rattrapée dans le fil résident, qui reste servi.
        let outcome = run_with_timeout(&pool, Duration::from_secs(5), &inflight, 4, || -> u8 {
            panic!("handler fautif")
        });
        assert!(matches!(outcome, Outcome::Panicked));
        let outcome = run_with_timeout(&pool, Duration::from_secs(5), &inflight, 4, || 9u8);
        assert!(matches!(outcome, Outcome::Done(9)));
        assert!(
            pool.resident.load(Ordering::Acquire) <= MAX_RESIDENT_HANDLERS,
            "le cache ne doit jamais dépasser son plafond"
        );
    }

    /// Plafond du cache : au-delà de `max_resident` fils occupés, le job part
    /// sur un fil éphémère — il s'exécute quand même (jamais d'attente en
    /// file, qui fausserait `tool_timeout`).
    #[test]
    fn plafond_du_cache_de_fils_fait_repartir_sur_un_fil_ephemere() {
        let pool = Arc::new(HandlerPool::new(1));
        let inflight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let barriere = Arc::new(std::sync::Barrier::new(2));
        let (b, i) = (Arc::clone(&barriere), Arc::clone(&inflight));
        let pool_occupe = Arc::clone(&pool);
        let occupe = std::thread::spawn(move || {
            run_with_timeout(&pool_occupe, Duration::from_secs(5), &i, 4, move || {
                b.wait();
                1u8
            })
        });
        let mut tours = 0;
        while inflight.load(Ordering::Acquire) == 0 {
            tours += 1;
            assert!(tours < 1_000_000, "le premier handler n'a pas démarré");
            std::thread::yield_now();
        }
        // Le fil résident est occupé : le second job doit s'exécuter sans
        // attendre (fil éphémère), et non patienter derrière le premier.
        let second = run_with_timeout(&pool, Duration::from_secs(5), &inflight, 4, || 2u8);
        assert!(matches!(second, Outcome::Done(2)));
        barriere.wait();
        assert!(matches!(occupe.join().unwrap(), Outcome::Done(1)));
    }

    /// Rafale de deux jobs sur un seul fil au repos : le second ne doit pas
    /// attendre la fin du premier. Sans le correctif, `run` enfilait les deux
    /// (le worker ne décrémente `attente` qu'à son réveil) : le second voyait
    /// son délai courir alors que son handler n'avait jamais démarré, et
    /// rendait `TimedOut`.
    ///
    /// Reproduction de la fenêtre : le fil de test garde le verrou de la
    /// file, les deux `run_with_timeout` s'y bloquent, puis il est relâché —
    /// les deux `run` passent alors avant que le worker (encore dans `wait`,
    /// non notifié) ne se réveille. L'ordonnancement ne laisse la fenêtre
    /// ouverte qu'irrégulièrement : si la course ne se produit pas (le worker
    /// se réveille avant le second `run`, qui crée alors un fil), le test
    /// reste vert — il ne prouve simplement rien de plus, et ne doit pas
    /// échouer pour autant.
    ///
    /// `un_job_deja_en_file_ne_rend_pas_le_fil_au_repos_disponible` épingle
    /// la même règle sans dépendre de l'ordonnancement.
    #[test]
    fn une_rafale_de_deux_jobs_ne_serialise_pas_le_second_derriere_le_premier() {
        let pool = Arc::new(HandlerPool::new(MAX_RESIDENT_HANDLERS));
        let inflight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        // Le premier handler dort bien au-delà du budget du second : s'il est
        // mis en file derrière lui, le second rend `TimedOut` à coup sûr,
        // tandis que le budget laisse de la marge à la création d'un fil.
        const DORT: Duration = Duration::from_millis(400);
        const BUDGET: Duration = Duration::from_millis(300);

        // Amorce : un job trivial fait naître le fil résident, qui revient au
        // repos. `attente > 0` sous le verrou = un fil prêt à consommer.
        assert!(matches!(
            run_with_timeout(&pool, Duration::from_secs(5), &inflight, 8, || ()),
            Outcome::Done(())
        ));
        let mut tours = 0;
        loop {
            let au_repos = pool.etat().attente;
            if au_repos > 0 {
                break;
            }
            tours += 1;
            assert!(tours < 1_000_000, "aucun fil au repos après l'amorçage");
            std::thread::yield_now();
        }

        // Verrou de la file pris par le fil de test : les deux `run` suivants
        // s'y bloquent et n'entrent qu'une fois qu'il est relâché.
        let verrou = pool.etat();
        let barriere = Arc::new(std::sync::Barrier::new(3));
        let lancer = |dort: Option<Duration>, budget: Duration| {
            let pool = Arc::clone(&pool);
            let inflight = Arc::clone(&inflight);
            let barriere = Arc::clone(&barriere);
            std::thread::spawn(move || {
                barriere.wait();
                run_with_timeout(&pool, budget, &inflight, 8, move || {
                    if let Some(dort) = dort {
                        std::thread::sleep(dort);
                    }
                    1u8
                })
            })
        };
        let premier = lancer(Some(DORT), Duration::from_secs(5));
        let second = lancer(None, BUDGET);
        barriere.wait();
        // Les deux appels sont désormais en attente du verrou (le sleep les
        // laisse l'atteindre) : on le relâche, ils passent tous les deux
        // avant le réveil du worker.
        std::thread::sleep(Duration::from_millis(50));
        drop(verrou);

        assert!(
            matches!(premier.join().unwrap(), Outcome::Done(1)),
            "le premier handler doit aboutir"
        );
        assert!(
            matches!(second.join().unwrap(), Outcome::Done(1)),
            "job enfilé derrière un fil occupé : son délai a couru sans qu'il démarre"
        );
    }

    /// Épinglage **déterministe** de la même règle, sans dépendre de
    /// l'ordonnancement : un job déjà déposé dans la file du fil au repos
    /// (état exact de la fenêtre de course) ne rend pas ce fil disponible
    /// pour autant. Le job suivant doit partir sur un fil créé pour lui.
    ///
    /// L'état est construit sous le verrou — un job lent en file, `attente`
    /// encore à 1 — là où la rafale concurrente ci-dessus ne retombe dans la
    /// fenêtre que par intermittence : sans le correctif, le job de test est
    /// enfilé derrière le job lent et son délai court avant tout démarrage.
    #[test]
    fn un_job_deja_en_file_ne_rend_pas_le_fil_au_repos_disponible() {
        let pool = HandlerPool::new(MAX_RESIDENT_HANDLERS);
        let inflight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        // Amorce : un job trivial fait naître le fil résident, qui revient au
        // repos (`attente > 0` sous le verrou = un fil qui consommera un job).
        assert!(matches!(
            run_with_timeout(&pool, Duration::from_secs(5), &inflight, 8, || ()),
            Outcome::Done(())
        ));
        let mut tours = 0;
        loop {
            let au_repos = pool.etat().attente;
            if au_repos > 0 {
                break;
            }
            tours += 1;
            assert!(tours < 1_000_000, "aucun fil au repos après l'amorçage");
            std::thread::yield_now();
        }
        // Job lent déjà confié à ce fil, sans le réveiller : file non vide et
        // `attente` encore incrémenté, exactement l'instant où un second job
        // ne doit plus compter sur lui.
        {
            let mut etat = pool.etat();
            etat.jobs
                .push_back(Box::new(|| std::thread::sleep(Duration::from_millis(400))));
        }
        let outcome = run_with_timeout(&pool, Duration::from_millis(300), &inflight, 8, || 2u8);
        assert!(
            matches!(outcome, Outcome::Done(2)),
            "job enfilé derrière un job déjà en file : son délai a couru sans qu'il démarre"
        );
    }
}
