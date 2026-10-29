//! Le registre **dynamique** : un serveur MCP dont la surface d'outils
//! change pendant qu'il sert — le socle des gateways.
//!
//! [`ServerCore`] est un **instantané** immuable : ses caches de listes
//! (`tools/list`, `server/discover`…) sont calculés une fois, ce qui rend le
//! chemin chaud sans resérialisation. Un gateway, lui, reflète un état qui
//! bouge — serveurs détectés qui apparaissent et disparaissent, outils
//! activés/désactivés à chaud. [`Gateway`] résout la tension sans toucher au
//! noyau : il garde le [`ServerBuilder`] vivant derrière un mutex, publie le
//! snapshot courant derrière un `RwLock`, et chaque `edit()` reconstruit un
//! [`ServerCore`] neuf puis l'échange atomiquement. Les requêtes en vol
//! finissent sur le snapshot où elles ont commencé — un lecteur ne voit
//! jamais un catalogue à moitié reconstruit.
//!
//! ```no_run
//! use litemcp::prelude::*;
//! use std::sync::Arc;
//!
//! # fn main() -> std::io::Result<()> {
//! // Un gateway qui reflète des serveurs détectés : un outil par serveur UP.
//! let gw = Arc::new(Gateway::new(
//!     ServerBuilder::new("gateway", "1.0.0")
//!         .tool("ping", "Sonner le gateway", |_args| {
//!             Ok(serde_json::json!({ "pong": true }))
//!         }),
//! ));
//!
//! // Détection : l'outil apparaît au prochain `tools/list` — sans redémarrer.
//! gw.edit(|b| b.tool("srv__echo", "Echo du serveur détecté", |args| {
//!     Ok(args.clone())
//! }));
//!
//! // Retrait quand le serveur disparaît (voir aussi
//! // `ServerBuilder::remove_tool`) — l'outil devient `-32602` immédiatement.
//! gw.edit(|mut b| {
//!     b.remove_tool("srv__echo");
//!     b
//! });
//!
//! // Dispatch embarqué (l'app hôte possède sa propre route HTTP) ou socket
//! // dédié : le snapshot courant sert la requête.
//! let snapshot = gw.snapshot();
//! let _ = snapshot.dispatch(br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
//! # Ok(())
//! # }
//! ```
//!
//! Verrous et embouteillages : `dispatch` ne prend que le `RwLock` en lecture
//! (clone d'`Arc`) ; `edit` clone le builder sous le mutex, reconstruit le
//! noyau HORS verrou (la closure ne peut pas se bloquer sur le gateway), puis
//! reprend le mutex pour remplacer l'état et échanger le snapshot SOUS LE
//! MÊME verrou — le snapshot publié ne peut jamais être plus ancien que l'état
//! du builder (aucune perte de mise à jour, aucun ordre de publication
//! inversé). Le coût d'un `edit` est celui d'un clone de builder (cartes
//! d'`Arc`) plus une construction de noyau — microsecondes à l'échelle d'un
//! gateway ; la fréquence naturelle d'appel est « à chaque changement de
//! détection », pas « à chaque requête ».

use crate::server::{McpHeaders, ServerBuilder, ServerCore};
use std::sync::{Arc, Mutex, RwLock};

/// Un serveur MCP **vivante** : la surface d'outils est modifiable pendant
/// que le dispatch sert les requêtes.
///
/// Le [`ServerCore`] publié par [`Gateway::snapshot`] reste un instantané
/// immuable (tous ses caches de listes restent valides) ; l'échange de
/// snapshot est l'événement de mutation. Cloner le [`Gateway`] est possible
/// mais inutile : tout clone partagerait… rien — construisez-en un par
/// process et partagez-le par `Arc`.
///
/// Le champ `builder` est un [`Mutex`] (et non `RwLock`) : `edit` le clone
/// pour appliquer la mutation fonctionnelle du builder (méthodes
/// consommatrices `fn tool(mut self) -> Self`), puis le remplace — deux
/// `edit` concurrents sont sérialisés, aucun ne perd sa mutation.
pub struct Gateway {
    builder: ReentrantBuilder,
    current: RwLock<Arc<ServerCore>>,
}

impl Gateway {
    /// Construit le gateway et son premier snapshot.
    #[must_use]
    pub fn new(builder: ServerBuilder) -> Self {
        let core = Arc::new(builder.core());
        Gateway {
            builder: ReentrantBuilder::new(builder),
            current: RwLock::new(core),
        }
    }

    /// Construit le gateway derrière un `Arc` — la forme d'usage (le gateway
    /// est partagé entre le transport et le code de détection).
    #[must_use]
    pub fn shared(builder: ServerBuilder) -> Arc<Self> {
        Arc::new(Self::new(builder))
    }

    /// Snapshot courant — immuable, clonable en `Arc` à coût nul. Les
    /// transports qui possèdent leur propre boucle HTTP (app hôte embarquant
    /// le dispatch) gardent ce handle et appellent [`ServerCore::dispatch`]
    /// à chaque requête ; le handle reste VALIDE après un `edit` (il sert
    /// l'ancien instantané jusqu'à sa fin) mais ne voit pas les nouveautés :
    /// reprenez [`Gateway::snapshot`] par requête pour toujours refléter le
    /// catalogue courant.
    #[must_use]
    pub fn snapshot(&self) -> Arc<ServerCore> {
        Arc::clone(&self.read_current())
    }

    /// Applique une mutation fonctionnelle au builder puis reconstruit et
    /// échange le snapshot. La closure reçoit le builder **par valeur**
    /// (méthodes consommatrices : `.tool()`, `.resource()`, `.prompt()`…)
    /// et le rend — le style du builder est préservé tel quel :
    ///
    /// ```no_run
    /// # use litemcp::prelude::*;
    /// # fn main() {
    /// # let gw = Gateway::new(ServerBuilder::new("gw", "1.0"));
    /// gw.edit(|b| b.tool("nouveau", "Apparu au prochain tools/list", |_a| Ok(serde_json::json!({}))));
    /// gw.edit(|mut b| {
    ///     b.remove_tool("nouveau");
    ///     b
    /// });
    /// # }
    /// ```
    ///
    /// Deux `edit` concurrents sont sérialisés par le mutex interne — chaque
    /// mutation part de l'état publié par la précédente, aucune n'est perdue.
    /// Le snapshot n'est échangé **qu'après** une reconstruction complète :
    /// un panic dans la closure laisse le snapshot courant intact (l'edit a
    /// échoué, le service continue sur l'ancien catalogue).
    ///
    /// # Réentrance
    ///
    /// La closure s'exécute **sous le verrou réentrant** du builder, ce qui
    /// garantit que deux `edit` concurrents (threads distincts) sont
    /// sérialisés : chaque mutation part de l'état publié par la précédente,
    /// **aucune n'est perdue**.
    ///
    /// Appeler [`Gateway::edit`] **depuis** la closure est une réentrance et
    /// est **refusé** par une panique explicite (message actionnable) : le
    /// verrou interne n'est pas réentrant, et la mutation imbriquée serait
    /// de toute façon écrasée par le retour de la closure extérieure (le
    /// builder est cloné avant `f`). Chaînez les mutations avec des `edit`
    /// successifs (`edit(|b| b.tool(..))` puis un second `edit(|b| …)`) —
    /// ou [`Gateway::edit_batch`] pour un lot.
    ///
    /// [`Gateway::dispatch`] et [`Gateway::snapshot`] (lectures) restent
    /// possibles depuis la closure : ils ne prennent que la `RwLock` en
    /// lecture.
    pub fn edit(&self, f: impl FnOnce(ServerBuilder) -> ServerBuilder) {
        // Tout se passe sous le **verrou réentrant** du builder :
        // - sérialisation : deux `edit` concurrents (threads distincts) sont
        //   ordonnés, chaque mutation part de l'état publié par la
        //   précédente (aucune perte) ;
        // - réentrance : une closure qui ré-appelle `edit` sur le même
        //   gateway ne se bloque pas (un `std::sync::Mutex` l'aurait figée
        //   définitivement — audit 2026-10-27, C1) ;
        // - publication : l'échange du snapshot se fait sous le même verrou
        //   que le remplacement de l'état → le snapshot publié ne peut
        //   jamais être plus ancien que l'état du builder (aucune
        //   publication dans le désordre, aucune perte de mise à jour).
        //
        // La publication n'a lieu qu'au niveau **le plus externe** : un `edit`
        // imbriqué ne doit pas prendre le `RwLock` en écriture pendant que
        // l'externe le détient (deadlock). Sa mutation est de toute façon
        // écrasée par le retour de la closure externe (le builder est cloné
        // avant `f`) — c'est documenté dans la section « Réentrance ».
        self.builder.with(|builder| {
            let rebuilt = f(builder.clone());
            let core = Arc::new(rebuilt.core());
            *builder = rebuilt;
            // Publication SOUS le même verrou que le remplacement de l'état :
            // le snapshot publié ne peut jamais être plus ancien que l'état
            // du builder (aucune perte de mise à jour, aucune publication
            // dans le désordre).
            *self
                .current
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = core;
        });
    }

    /// Applique **plusieurs** mutations en une seule reconstruction — le
    /// chemin naturel pour une détection qui annonce plusieurs serveurs.
    ///
    /// Un seul snapshot est reconstruit et échangé : le catalogue ne passe
    /// jamais par un état intermédiaire (pas de « fenêtre » où un seul
    /// serveur est visible). Les mutations sont appliquées dans l'ordre.
    ///
    /// ```no_run
    /// # use litemcp::prelude::*;
    /// # fn main() {
    /// # let gw = Gateway::new(ServerBuilder::new("gw", "1.0"));
    /// gw.edit_batch(|b| {
    ///     b.tool("srv_a", "A", |_a| Ok(serde_json::json!({})))
    ///         .tool("srv_b", "B", |_a| Ok(serde_json::json!({})))
    /// });
    /// # }
    /// ```
    pub fn edit_batch(&self, f: impl FnOnce(ServerBuilder) -> ServerBuilder) {
        self.edit(f)
    }

    /// Retire une **ressource** du registre — le pendant retrait de
    /// [`ServerBuilder::remove_tool`] pour les ressources : un serveur
    /// détecté disparaît, sa ressource disparaît de `resources/list`.
    ///
    /// Retourne `true` si la ressource existait. Un outil absent → `false`,
    /// aucun autre changement.
    pub fn remove_resource(&self, uri: &str) -> bool {
        let mut removed = false;
        self.edit(|mut builder| {
            removed = builder.remove_resource(uri);
            builder
        });
        removed
    }

    /// Retire un **prompt** du registre — le pendant retrait de
    /// [`ServerBuilder::remove_tool`] pour les prompts.
    ///
    /// Retourne `true` si le prompt existait.
    pub fn remove_prompt(&self, name: &str) -> bool {
        let mut removed = false;
        self.edit(|mut builder| {
            removed = builder.remove_prompt(name);
            builder
        });
        removed
    }

    /// Nombre d'outils du snapshot courant — pour les journaux et les tests.
    #[must_use]
    pub fn tool_count(&self) -> usize {
        self.read_current().tool_names().len()
    }

    /// Dispatch sur le snapshot courant — voir [`ServerCore::dispatch`].
    /// `None` = notification (aucune réponse).
    pub fn dispatch(&self, body: &[u8]) -> Option<Vec<u8>> {
        self.snapshot().dispatch(body)
    }

    /// Dispatch sur le snapshot courant, avec vérification des en-têtes de
    /// routage MCP (SEP-2243) — voir
    /// [`ServerCore::dispatch_with_headers`].
    pub fn dispatch_with_headers(
        &self,
        body: &[u8],
        headers: Option<&McpHeaders>,
    ) -> Option<Vec<u8>> {
        self.snapshot().dispatch_with_headers(body, headers)
    }

    fn read_current(&self) -> Arc<ServerCore> {
        Arc::clone(
            &self
                .current
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

// Verrou **réentrant** portant l'état du builder.
//
// `std::sync::Mutex` n'est pas réentrant : une closure d'[`Gateway::edit`]
// qui ré-appelle `edit` sur le même gateway se bloquerait définitivement
// (audit 2026-10-27, C1 — deadlock reproductible à 100 %).
//
// Ce verrou refuse la réentrance **explicitement** (panique portant un
// message actionnable) au lieu de figer le thread, tout en sérialisant les
// **threads distincts** : la sémantique « deux `edit` concurrents sont
// sérialisés, aucune mutation n'est perdue » est intégralement conservée,
// et la closure s'exécute toujours sous le verrou (pas de perte de mise à
// jour par publication dans le désordre).
//
// Détection de la réentrance : un drapeau **thread-local** porte l'adresse
// du verrou détenu. Aucune donnée n'est déréférencée à travers cette
// adresse (comparaison d'entiers seulement) — donc aucun `unsafe`.
thread_local! {
    /// Identité du verrou `ReentrantBuilder` détenu par le thread courant
    /// (`0` = aucun).
    static HELD_LOCK: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Verrou **réentrant** portant l'état du builder.
///
/// `std::sync::Mutex` n'est pas réentrant : une closure d'[`Gateway::edit`]
/// qui ré-appelle `edit` sur le même gateway se bloquerait définitivement
/// (audit 2026-10-27, C1 — deadlock reproductible à 100 %).
///
/// Ce verrou refuse la réentrance **explicitement** (panique portant un
/// message actionnable) au lieu de figer le thread, tout en sérialisant les
/// **threads distincts** : la sémantique « deux `edit` concurrents sont
/// sérialisés, aucune mutation n'est perdue » est intégralement conservée,
/// et la closure s'exécute toujours sous le verrou (pas de perte de mise à
/// jour par publication dans le désordre).
///
/// Détection de la réentrance : un drapeau **thread-local** porte
/// l'identifiant du verrou détenu. Aucune donnée n'est déréférencée à
/// travers cet identifiant (comparaison d'entiers seulement) — donc aucun
/// `unsafe`.
struct ReentrantBuilder {
    state: Mutex<ServerBuilder>,
    /// Identité unique de ce verrou (détection de réentrance).
    id: u64,
}

impl ReentrantBuilder {
    fn new(builder: ServerBuilder) -> Self {
        ReentrantBuilder {
            state: Mutex::new(builder),
            id: next_lock_id(),
        }
    }

    /// Exécute `f` sous le verrou.
    ///
    /// - **Threads distincts** : sérialisés par le `Mutex` — chaque mutation
    ///   part de l'état publié par la précédente (aucune perte).
    /// - **Réentrance** (le thread courant détient déjà ce verrou) :
    ///   refusée par une panique explicite — un `Mutex` standard figerait
    ///   le thread sans fin.
    fn with<R>(&self, f: impl FnOnce(&mut ServerBuilder) -> R) -> R {
        let me = HELD_LOCK.get();
        if me == self.id {
            panic!(
                "litemcp/gateway : `Gateway::edit` a été appelé DEPUIS la closure d'un \
                 autre `Gateway::edit` sur le même gateway — c'est une réentrance, \
                 interdite (le verrou interne n'est pas réentrant). Enchaînez les \
                 mutations : `edit(|b| …)` puis un second `edit(|b| …)`."
            );
        }
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        HELD_LOCK.set(self.id);
        // Garde de restauration : le drapeau thread-local revient à son état
        // précédent même si la closure panique.
        let _guard = HeldGuard { previous: me };
        f(&mut guard)
    }
}

/// Restaure le drapeau thread-local à la sortie du verrou (panique incluse).
struct HeldGuard {
    previous: u64,
}

impl Drop for HeldGuard {
    fn drop(&mut self) {
        HELD_LOCK.set(self.previous);
    }
}

/// Identifiant unique et croissant pour chaque verrou — deux
/// `ReentrantBuilder` ont donc deux identités distinctes (le pointeur du
/// thread-local serait identique pour tous).
fn next_lock_id() -> u64 {
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl Default for Gateway {
    /// Un gateway vide nommé "gateway" — point de départ d'un registre
    /// rempli par `edit`.
    fn default() -> Self {
        Self::new(ServerBuilder::new("gateway", "0.0.0"))
    }
}

impl std::fmt::Debug for Gateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gateway")
            .field("tools", &self.tool_count())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ToolError;
    use serde_json::{Value, json};

    fn rpc(method: &str, id: u64) -> String {
        json!({ "jsonrpc": "2.0", "id": id, "method": method }).to_string()
    }

    fn call(name: &str, id: u64) -> String {
        json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": name, "arguments": {} }
        })
        .to_string()
    }

    fn tool_names(reply: &[u8]) -> Vec<String> {
        let v: Value = serde_json::from_slice(reply).unwrap();
        v["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap_or("").to_string())
            .collect()
    }

    fn build(name: &str) -> ServerBuilder {
        let nom = name.to_string();
        ServerBuilder::new("gateway-test", "1.0.0").tool(name, "outil initial", move |_args| {
            Ok(json!({ "from": nom }))
        })
    }

    #[test]
    fn outil_ajoute_apres_construction_devient_servi() {
        let gw = Gateway::new(build("a"));
        // avant l'edit : un seul outil
        let reply = gw.dispatch(rpc("tools/list", 1).as_bytes()).unwrap();
        assert_eq!(tool_names(&reply), vec!["a".to_string()]);

        gw.edit(|b| b.tool("b", "outil ajouté", |_args| Ok(json!({ "from": "b" }))));

        // après l'edit : les deux outils, et l'appel du nouveau réussit
        let reply = gw.dispatch(rpc("tools/list", 2).as_bytes()).unwrap();
        assert_eq!(tool_names(&reply), vec!["a".to_string(), "b".to_string()]);
        let reply = gw.dispatch(call("b", 3).as_bytes()).unwrap();
        let v: Value = serde_json::from_slice(&reply).unwrap();
        assert_eq!(v["result"]["structuredContent"]["from"], "b");
    }

    #[test]
    fn outil_retire_devient_erreur_32602_et_le_restant_survit() {
        let gw = Gateway::new(build("a").tool("b", "à retirer", |_args| Ok(json!({}))));

        gw.edit(|mut b| {
            assert!(
                b.remove_tool("b"),
                "remove_tool sur un outil présent → true"
            );
            b
        });

        let reply = gw.dispatch(rpc("tools/list", 1).as_bytes()).unwrap();
        assert_eq!(tool_names(&reply), vec!["a".to_string()]);
        let reply = gw.dispatch(call("b", 2).as_bytes()).unwrap();
        let v: Value = serde_json::from_slice(&reply).unwrap();
        assert_eq!(v["error"]["code"], -32602, "outil retiré → Unknown tool");
        // l'outil restant continue d'être servi
        let reply = gw.dispatch(call("a", 3).as_bytes()).unwrap();
        let v: Value = serde_json::from_slice(&reply).unwrap();
        assert_eq!(v["result"]["structuredContent"]["from"], "a");
        // remove sur un absent → false, sans toucher au reste
        gw.edit(|mut b| {
            assert!(!b.remove_tool("inconnu"));
            b
        });
        assert_eq!(gw.tool_count(), 1);
    }

    #[test]
    fn remove_tool_purge_les_annotations_en_attente_reel() {
        let gw = Gateway::new(build("a"));
        // annotation en attente pour un outil PAS encore enregistré
        gw.edit(|b| {
            let b = b.annotate("y", crate::protocol::ToolAnnotations::new().read_only(true));
            b.tool("y", "avec annotation", |_args| Ok(json!({})))
        });
        let reply = gw.dispatch(rpc("tools/list", 1).as_bytes()).unwrap();
        let v: Value = serde_json::from_slice(&reply).unwrap();
        let y = v["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "y")
            .unwrap();
        assert_eq!(y["annotations"]["readOnlyHint"], true);

        // remove puis re-add : l'annotation NE doit PAS survivre
        gw.edit(|mut b| {
            b.remove_tool("y");
            b.tool("y", "sans annotation", |_args| Ok(json!({})))
        });
        let reply = gw.dispatch(rpc("tools/list", 2).as_bytes()).unwrap();
        let v: Value = serde_json::from_slice(&reply).unwrap();
        let y = v["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "y")
            .unwrap();
        assert!(
            y.get("annotations").is_none(),
            "remove_tool purge les annotations en attente : {y}"
        );
    }

    #[test]
    fn edit_concurrent_avec_dispatch_ne_perd_aucune_mutation() {
        let gw = Arc::new(Gateway::new(build("a")));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // **Barrière** : les lecteurs sont garantis ACTIFS (déjà en train de
        // dispatch) avant le premier `edit`. Sans elle, si les 128 edits
        // finissent avant la première schedulation d'un lecteur, celui-ci ne
        // sert rien et l'assertion `tours > 0` tombe — un flaky observé 6
        // échecs sur 8 runs. Le harnais doit être déterministe.
        let barrier = Arc::new(std::sync::Barrier::new(5)); // 4 lecteurs + main
        // **Drapeaux de service** (flaky résiduel observé en CI : run
        // Release v0.10.0, job verify) : la barrière garantit que le lecteur
        // a DÉMARRÉ, pas qu'il a obtenu une tranche de CPU — un lecteur
        // préempté pendant toute la salve de 128 edits (microsecondes)
        // peut finir avec `tours == 0` sur un runner à 2 cœurs. Chaque
        // lecteur signale son premier dispatch terminé, et le main attend
        // les quatre signaux (attente bornée) avant de lever `stop` :
        // `tours > 0` devient déterministe sans affaiblir l'invariant
        // testé (aucune mutation perdue, catalogue jamais à moitié
        // reconstruit).
        let served: Vec<Arc<std::sync::atomic::AtomicBool>> = (0..4)
            .map(|_| Arc::new(std::sync::atomic::AtomicBool::new(false)))
            .collect();
        let mut readers = Vec::new();
        for flag in &served {
            let (gw, stop, barrier, flag) = (
                Arc::clone(&gw),
                Arc::clone(&stop),
                Arc::clone(&barrier),
                Arc::clone(flag),
            );
            readers.push(std::thread::spawn(move || {
                let mut tours = 0u64;
                barrier.wait();
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    // chaque lecture doit rendre une réponse complète : jamais
                    // un catalogue à moitié reconstruit
                    let reply = gw.dispatch(rpc("tools/list", tours).as_bytes()).unwrap();
                    let names = tool_names(&reply);
                    assert!(!names.is_empty(), "catalogue vide intercepté");
                    assert!(
                        names.contains(&"a".to_string()),
                        "l'outil de base a disparu : {names:?}"
                    );
                    tours += 1;
                    if tours == 1 {
                        flag.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                tours
            }));
        }
        barrier.wait(); // les lecteurs servent déjà quand le premier edit part
        for i in 0..64 {
            let nom = format!("outil-{i}");
            gw.edit(move |b| b.tool(&nom, "temporaire", move |_args| Ok(json!({ "n": i }))));
            gw.edit(move |mut b| {
                b.remove_tool(&format!("outil-{i}"));
                b
            });
        }
        // Les quatre lecteurs ont servi au moins une requête avant l'arrêt
        // (attente bornée à 5 s — un dispatch dure des microsecondes ; le
        // plafond ne sert que sur un runner pathologiquement saturé).
        let echeance = std::time::Instant::now() + std::time::Duration::from_secs(5);
        for flag in &served {
            while !flag.load(std::sync::atomic::Ordering::Relaxed)
                && std::time::Instant::now() < echeance
            {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for r in readers {
            assert!(r.join().unwrap() > 0, "le lecteur n'a rien servi");
        }
        // toutes les mutations sont passées : état final propre
        assert_eq!(gw.tool_count(), 1, "64 ajouts + 64 retraits → retour à 1");
        let reply = gw.dispatch(call("a", 999).as_bytes()).unwrap();
        let v: Value = serde_json::from_slice(&reply).unwrap();
        assert_eq!(v["result"]["structuredContent"]["from"], "a");
    }

    #[test]
    fn edit_reentrant_refuse_avec_un_message_clair() {
        // C1 (audit 2026-10-27) : la closure s'exécutait SOUS un
        // `std::sync::Mutex` (non réentrant) → un `edit` qui ré-appelle
        // `edit` sur le même gateway FIGEAIT le thread définitivement
        // (reproductible 100 % avant correction, aucun message, aucun
        // déverrouillage).
        //
        // Après correction : la réentrance est détectée et refusée par une
        // PANIQUE explicite, actionnable — un deadlock muet devient une
        // erreur immédiate et lisible.
        let gw = Arc::new(Gateway::new(build("a")));
        let g2 = Arc::clone(&gw);
        let handle = std::thread::spawn(move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                gw.edit(|_b| {
                    g2.edit(|b| b.tool("reentrant", "x", |_a| Ok(json!({}))));
                    ServerBuilder::new("gateway-test", "1.0.0")
                });
            }))
        });
        let mut fini = false;
        for _ in 0..100 {
            if handle.is_finished() {
                fini = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            fini,
            "DEADLOCK : `edit` réentrant sur le même gateway bloque le thread \
             (le Mutex du builder n'est pas réentrant)"
        );
        let paniqué = handle.join().unwrap().is_err();
        assert!(
            paniqué,
            "la réentrance doit être refusée par une panique explicite \
             (et non exécutée silencieusement)"
        );
    }

    /// Le message de la panique doit être ACTIONNABLE (l'opérateur doit
    /// comprendre quoi changer).
    #[test]
    fn message_de_reentrance_actionnable() {
        let gw = Arc::new(Gateway::new(build("a")));
        let g2 = Arc::clone(&gw);
        let message = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            gw.edit(|_b| {
                g2.edit(|b| b.tool("reentrant", "x", |_a| Ok(json!({}))));
                ServerBuilder::new("gateway-test", "1.0.0")
            });
        }))
        .unwrap_err();
        let texte = message
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| message.downcast_ref::<&str>().map(|s| (*s).to_string()))
            .unwrap_or_default();
        assert!(
            texte.contains("réentrance") && texte.contains("edit"),
            "le message doit nommer le problème et la solution : {texte}"
        );
    }

    #[test]
    fn edition_concurrente_les_mutations_ne_se_perdent_pas() {
        // Le snapshot publié ne doit jamais être plus ancien que l'état du
        // builder : deux `edit` concurrents doivent produire un catalogue
        // contenant TOUS les outils ajoutés (publication dans le désordre
        // impossible — l'échange se fait sous le même verrou que le
        // remplacement de l'état).
        let gw = Arc::new(Gateway::new(ServerBuilder::new("gw", "1.0")));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let lecteurs = Arc::clone(&stop);
        let gw_lecteur = Arc::clone(&gw);
        let lecteur = std::thread::spawn(move || {
            while !lecteurs.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = gw_lecteur.snapshot();
            }
        });
        let mut writers = Vec::new();
        for t in 0..8usize {
            let gw = Arc::clone(&gw);
            writers.push(std::thread::spawn(move || {
                for k in 0..40usize {
                    let nom = format!("t{t}_{k}");
                    let pour_edit = nom.clone();
                    gw.edit(move |b| b.tool(&pour_edit, "x", move |_a| Ok(json!({}))));
                }
            }));
        }
        for w in writers {
            let _ = w.join();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = lecteur.join();

        let reply = gw
            .dispatch(br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
            .unwrap();
        let v: Value = serde_json::from_slice(&reply).unwrap();
        let noms: Vec<String> = v["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            noms.len(),
            8 * 40,
            "tous les ajouts doivent être dans le catalogue final (aucune perte) : {} outils",
            noms.len()
        );
    }

    #[test]
    fn panique_dans_edit_le_snapshot_courant_intact() {
        let gw = Gateway::new(build("a"));
        let before = gw.snapshot();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            gw.edit(|_b| {
                panic!("mutation ratée");
            })
        }));
        assert!(outcome.is_err(), "le panic doit se propager à l'appelant");
        // le service continue sur l'ancien snapshot, inchangé
        let reply = gw.dispatch(rpc("tools/list", 1).as_bytes()).unwrap();
        assert_eq!(tool_names(&reply), vec!["a".to_string()]);
        let after = gw.snapshot();
        assert!(
            Arc::ptr_eq(&before, &after),
            "un edit qui panique ne doit pas échanger le snapshot"
        );
    }

    #[test]
    fn dispatch_with_headers_transmis_au_snapshot() {
        let gw = Gateway::new(build("a"));
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "a", "arguments": {} }
        })
        .to_string();
        // en-tête cohérent → réponse normale
        let headers = crate::server::McpHeaders {
            method: Some("tools/call".to_string()),
            name: Some("a".to_string()),
        };
        let reply = gw
            .dispatch_with_headers(body.as_bytes(), Some(&headers))
            .unwrap();
        assert!(serde_json::from_slice::<Value>(&reply).unwrap()["result"].is_object());
        // en-tête incohérent → -32020 (mismatch)
        let headers = crate::server::McpHeaders {
            method: Some("ping".to_string()),
            name: None,
        };
        let reply = gw
            .dispatch_with_headers(body.as_bytes(), Some(&headers))
            .unwrap();
        let v: Value = serde_json::from_slice(&reply).unwrap();
        assert_eq!(
            v["error"]["code"],
            crate::jsonrpc::ErrorObject::HEADER_MISMATCH
        );
    }

    #[test]
    fn erreurs_d_outils_classifiees_traversent_les_edits() {
        let gw = Gateway::new(build("a"));
        gw.edit(|b| {
            b.tool("rate", "erreur structurée", |_args| {
                Err(ToolError::rate_limited("quota").retry_after_ms(500))
            })
        });
        let reply = gw.dispatch(call("rate", 1).as_bytes()).unwrap();
        let v: Value = serde_json::from_slice(&reply).unwrap();
        assert_eq!(v["result"]["isError"], true);
        assert_eq!(
            v["result"]["structuredContent"]["error"]["code"],
            "rate_limited"
        );
    }

    #[test]
    fn resources_et_prompts_survivent_aux_edits_d_outils() {
        let gw =
            Gateway::new(ServerBuilder::new("gw", "1.0").tool("a", "outil", |_a| Ok(json!({}))));
        gw.edit(|b| {
            let b = b.resource("mcp://etat", "état", "doc vivant", || {
                Ok(crate::protocol::ResourceContents {
                    uri: "mcp://etat".to_string(),
                    mime_type: Some("application/json".to_string()),
                    text: "{}".to_string(),
                })
            });
            b.prompt("salut", "prompt simple", |_args| {
                Ok(vec![crate::protocol::PromptMessage::user("bonjour")])
            })
        });
        let reply = gw.dispatch(rpc("resources/list", 1).as_bytes()).unwrap();
        let v: Value = serde_json::from_slice(&reply).unwrap();
        assert_eq!(v["result"]["resources"][0]["uri"], "mcp://etat");
        let reply = gw.dispatch(rpc("prompts/list", 2).as_bytes()).unwrap();
        let v: Value = serde_json::from_slice(&reply).unwrap();
        assert_eq!(v["result"]["prompts"][0]["name"], "salut");
        // et l'outil ajouté dans le MÊME edit est là aussi
        let reply = gw.dispatch(rpc("tools/list", 3).as_bytes()).unwrap();
        assert!(tool_names(&reply).contains(&"a".to_string()));
    }

    #[test]
    fn tool_names_et_debug_sont_coherents() {
        let gw = Gateway::new(build("a"));
        assert_eq!(gw.tool_count(), 1);
        let snap = gw.snapshot();
        assert_eq!(snap.tool_names(), vec!["a".to_string()]);
        let dbg = format!("{gw:?}");
        assert!(dbg.contains("tools: 1"), "{dbg}");
        // Default : gateway vide constructible
        let vide = Gateway::default();
        assert_eq!(vide.tool_count(), 0);
    }
}
