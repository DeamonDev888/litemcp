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
//! // dédié ([`Gateway::bind`] — transport HTTP intégré) : le snapshot
//! // courant sert la requête. Un corps nu ne passe pas le profil strict —
//! // `dispatch_unchecked` est réservé aux appelants qui garantissent déjà
//! // des corps MCP 2026-07-28 conformes.
//! let snapshot = gw.snapshot();
//! let _ = snapshot.dispatch_unchecked(br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
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
        // Un compteur d'inflight **par gateway**, partagé par tous ses
        // snapshots : les handlers détachés d'un ancien snapshot (timeout)
        // restent comptés dans le plafond global après un `edit` — sinon
        // chaque noyau reconstruit repartait de zéro et `Gateway::edit`
        // contournait `MAX_INFLIGHT_HANDLERS`. Deux gateways, deux
        // compteurs ; un serveur indépendant (bind sans gateway), le sien.
        // L'injection vit dans le builder stocké : chaque `edit` clone le
        // builder et le noyau reconstruit hérite du même compteur.
        let builder =
            builder.share_inflight_counter(Arc::new(std::sync::atomic::AtomicUsize::new(0)));
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

    /// Retire un **outil** du registre — le pendant direct de
    /// [`ServerBuilder::remove_tool`] : un serveur détecté disparaît, son
    /// outil disparaît de `tools/list`, sans passer par une closure
    /// d'`edit`.
    ///
    /// Retourne `true` si l'outil existait. Un outil absent → `false`,
    /// aucun autre changement. Les métadonnées en attente pour ce nom
    /// (annotations, `outputSchema`, capacités requises) sont purgées en
    /// même temps — un ré-enregistrement du même nom repart propre, comme
    /// pour un retrait opéré dans un `edit`.
    ///
    /// ```no_run
    /// # use litemcp::prelude::*;
    /// # fn main() {
    /// # let gw = Gateway::new(ServerBuilder::new("gw", "1.0")
    /// #     .tool("srv__echo", "Echo du serveur détecté", |_a| Ok(serde_json::json!({}))));
    /// let retire = gw.remove_tool("srv__echo"); // `true` : quitte `tools/list`
    /// # let _ = retire;
    /// # }
    /// ```
    pub fn remove_tool(&self, name: &str) -> bool {
        let mut removed = false;
        self.edit(|mut builder| {
            removed = builder.remove_tool(name);
            builder
        });
        removed
    }

    /// Retire un **gabarit de ressource** du registre — le pendant direct de
    /// [`ServerBuilder::remove_resource_template`] : le gabarit disparaît de
    /// `resources/templates/list`.
    ///
    /// Retourne `true` si le gabarit existait, `false` sinon (aucun autre
    /// changement). Les ressources **fixes** ne sont pas touchées — voir
    /// [`Gateway::remove_resource`].
    pub fn remove_resource_template(&self, uri_template: &str) -> bool {
        let mut removed = false;
        self.edit(|mut builder| {
            removed = builder.remove_resource_template(uri_template);
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
    /// `None` = notification (aucune réponse). Le profil configuré
    /// s'applique : en strict (défaut), un corps sans `_meta` est refusé en
    /// `-32602`.
    ///
    /// **Garde de récursion** : un handler qui re-dispatche est coupé
    /// au-delà de `MAX_DISPATCH_DEPTH` dispatchs imbriqués sur le thread
    /// courant (tous gateways confondus) par une erreur `-32603` — voir
    /// `recursion_limit_response`.
    pub fn dispatch(&self, body: &[u8]) -> Option<Vec<u8>> {
        let Some(_garde) = Self::enter_dispatch() else {
            return recursion_limit_response(body).map(|(corps, _)| corps);
        };
        self.snapshot().dispatch(body)
    }

    /// Dispatch sur le snapshot courant **sans contrôle de profil** — voir
    /// [`ServerCore::dispatch_unchecked`]. Réservé aux appelants qui
    /// garantissent déjà des corps conformes (tests, harnais de mesure).
    ///
    /// **Garde de récursion** : identique à [`Gateway::dispatch`].
    pub fn dispatch_unchecked(&self, body: &[u8]) -> Option<Vec<u8>> {
        let Some(_garde) = Self::enter_dispatch() else {
            return recursion_limit_response(body).map(|(corps, _)| corps);
        };
        self.snapshot().dispatch_unchecked(body)
    }

    /// Dispatch sur le snapshot courant, avec vérification des en-têtes de
    /// routage MCP (SEP-2243) — voir
    /// [`ServerCore::dispatch_with_headers`].
    ///
    /// **Garde de récursion** : identique à [`Gateway::dispatch`].
    pub fn dispatch_with_headers(
        &self,
        body: &[u8],
        headers: Option<&McpHeaders>,
    ) -> Option<Vec<u8>> {
        let Some(_garde) = Self::enter_dispatch() else {
            return recursion_limit_response(body).map(|(corps, _)| corps);
        };
        self.snapshot().dispatch_with_headers(body, headers)
    }

    /// Dispatch sur le snapshot courant avec statut structuré — voir
    /// [`ServerCore::dispatch_request`].
    ///
    /// **Garde de récursion** : identique à [`Gateway::dispatch`] — le
    /// `-32603` porte le statut canonique du noyau pour ce code
    /// (`HttpStatus::from_error_code`).
    pub fn dispatch_request(
        &self,
        body: &[u8],
        headers: Option<&McpHeaders>,
    ) -> Option<crate::server::DispatchResponse> {
        let Some(_garde) = Self::enter_dispatch() else {
            // Le statut suit le corps rendu : `-32603` d'ordinaire, `-32700`
            // (400) quand le corps de la requête était illisible.
            return recursion_limit_response(body).map(|(corps, code)| {
                crate::server::DispatchResponse {
                    body: corps,
                    status: crate::server::HttpStatus::from_error_code(code),
                }
            });
        };
        self.snapshot().dispatch_request(body, headers)
    }

    /// Dispatch sur le snapshot courant avec une identité **déjà établie** —
    /// voir [`ServerCore::dispatch_authenticated`].
    ///
    /// L'autorisation s'applique au **snapshot courant** : un `edit` qui
    /// échange le catalogue pendant qu'une requête s'exécute ne vaut que pour
    /// le dispatch suivant (la requête en cours garde le snapshot où elle a
    /// commencé). Un dispatch **imbriqué** (handler appelant
    /// [`Gateway::dispatch`]) hérite de l'identité du fil, comme
    /// [`crate::progress`] et [`crate::cancel`] pour leurs canaux.
    ///
    /// **Garde de récursion** : identique à [`Gateway::dispatch`] — le
    /// `-32603` porte le statut canonique du noyau pour ce code
    /// (`HttpStatus::from_error_code`).
    pub fn dispatch_authenticated(
        &self,
        body: &[u8],
        headers: Option<&McpHeaders>,
        context: &crate::auth::AuthContext,
    ) -> Option<crate::server::DispatchResponse> {
        let Some(_garde) = Self::enter_dispatch() else {
            return recursion_limit_response(body).map(|(corps, code)| {
                crate::server::DispatchResponse {
                    body: corps,
                    status: crate::server::HttpStatus::from_error_code(code),
                }
            });
        };
        self.snapshot()
            .dispatch_authenticated(body, headers, context)
    }

    /// **Vérifie puis dispatche** sur le snapshot courant — voir
    /// [`ServerCore::dispatch_verified`] (contrat C8).
    ///
    /// C'est le point d'entrée d'une passerelle qui reçoit des requêtes
    /// brutes : la vérification et la décision d'autorisation portent sur le
    /// **snapshot courant**, comme [`Gateway::dispatch_authenticated`] — un
    /// `edit` concurrent ne vaut que pour le dispatch suivant.
    pub fn dispatch_verified(
        &self,
        request: &crate::auth::VerifiedRequest<'_>,
    ) -> crate::auth::AuthResponse {
        let Some(_garde) = Self::enter_dispatch() else {
            return recursion_limit_response(request.body).map_or_else(
                || crate::auth::AuthResponse::new(500, Vec::new(), Vec::new()),
                |(corps, code)| {
                    crate::auth::AuthResponse::new(
                        crate::server::HttpStatus::from_error_code(code).code(),
                        Vec::new(),
                        corps,
                    )
                },
            );
        };
        self.snapshot().dispatch_verified(request)
    }

    /// Dispatch sur le snapshot courant avec progression **request-scoped**
    /// — voir [`ServerCore::dispatch_request_with_progress`]. Le canal
    /// fourni reçoit les `notifications/progress` émises par le handler
    /// pendant l'exécution ([`crate::progress::report`]) : c'est le point
    /// d'entrée d'un hôte qui sert lui-même le flux SSE du POST, **sans**
    /// contourner la garde de récursion en reprenant [`Gateway::snapshot`]
    /// à chaque requête (audit 2026-11-25, A1).
    ///
    /// **Garde de récursion** : identique à [`Gateway::dispatch`] — au-delà
    /// de `MAX_DISPATCH_DEPTH` dispatchs imbriqués sur le thread courant,
    /// l'appel reçoit une enveloppe `-32603` au lieu d'exécuter le corps,
    /// avec le statut canonique du noyau pour ce code.
    pub fn dispatch_request_with_progress(
        &self,
        body: &[u8],
        headers: Option<&McpHeaders>,
        channel: Arc<dyn crate::progress::ProgressChannel>,
    ) -> Option<crate::server::DispatchResponse> {
        let Some(_garde) = Self::enter_dispatch() else {
            // Le statut suit le corps rendu : `-32603` d'ordinaire, `-32700`
            // (400) quand le corps de la requête était illisible.
            return recursion_limit_response(body).map(|(corps, code)| {
                crate::server::DispatchResponse {
                    body: corps,
                    status: crate::server::HttpStatus::from_error_code(code),
                }
            });
        };
        self.snapshot()
            .dispatch_request_with_progress(body, headers, channel)
    }

    /// Idem [`Gateway::dispatch_request_with_progress`], avec une identité
    /// **déjà établie** par le transport hôte — voir
    /// [`ServerCore::dispatch_authenticated_with_progress`].
    ///
    /// **Garde de récursion** : identique à [`Gateway::dispatch`].
    pub fn dispatch_authenticated_with_progress(
        &self,
        body: &[u8],
        headers: Option<&McpHeaders>,
        context: &crate::auth::AuthContext,
        channel: Arc<dyn crate::progress::ProgressChannel>,
    ) -> Option<crate::server::DispatchResponse> {
        let Some(_garde) = Self::enter_dispatch() else {
            return recursion_limit_response(body).map(|(corps, code)| {
                crate::server::DispatchResponse {
                    body: corps,
                    status: crate::server::HttpStatus::from_error_code(code),
                }
            });
        };
        self.snapshot()
            .dispatch_authenticated_with_progress(body, headers, context, channel)
    }

    /// Lie le gateway au **transport HTTP intégré** — un socket dédié qui
    /// sert le registre vivant sans que l'app hôte réécrive une boucle
    /// socket : `POST /mcp` (réponse JSON ou flux SSE de progression),
    /// sonde `/health`, gardes `Origin`/`Host`, plafond de connexions,
    /// arrêt propre ([`GatewayServer::handle`] /
    /// [`crate::http::ServerHandle::shutdown`]).
    ///
    /// Chaque requête re-publie un snapshot **au moment où elle est
    /// servie** : une édition ([`Gateway::edit`],
    /// [`Gateway::remove_tool`]…) survenue entre deux requêtes — y compris
    /// sur la même connexion keep-alive — est visible dès la requête
    /// suivante. La cohérence est **par requête**, sans snapshot figé : une
    /// requête en vol finit sur le snapshot où elle a commencé, jamais sur
    /// un catalogue à moitié reconstruit (voir [`Gateway::edit`]).
    ///
    /// # Configuration du transport
    ///
    /// Celle du chemin [`crate::ServerBuilder::bind`] **sans aucune
    /// option** : route `/mcp`, aucun hook de requête, aucune origine CORS
    /// autorisée (le préflight d'un navigateur est refusé, hors exception
    /// bouclage même-origine), hôtes locaux par défaut, timeouts et
    /// plafonds par défaut, pas d'annonce MCP Control, pas d'anti-zombie,
    /// pas de TLS. Une gateway qui a besoin d'une de ces options (TLS,
    /// CORS, hooks, autorisation) monte [`Gateway::snapshot`] dans son
    /// propre socket durci — ou passe par un reverse proxy amont. La liste
    /// `Access-Control-Allow-Headers` du préflight est la base commune ;
    /// elle n'est pas recalculée quand un `edit` ajoute un outil portant
    /// des `Mcp-Param-*`.
    ///
    /// # Exemple
    ///
    /// ```no_run
    /// use litemcp::prelude::*;
    /// use std::sync::Arc;
    ///
    /// # fn main() -> std::io::Result<()> {
    /// let gw = Arc::new(Gateway::new(
    ///     ServerBuilder::new("gateway", "1.0.0")
    ///         .tool("ping", "Sonner le gateway", |_a| {
    ///             Ok(serde_json::json!({ "pong": true }))
    ///         }),
    /// ));
    /// let serveur = gw.bind("127.0.0.1:23080")?;
    /// let handle = serveur.handle();
    /// std::thread::spawn(move || {
    ///     let _ = serveur.serve();
    /// });
    /// // … plus tard :
    /// handle.shutdown();
    /// # Ok(())
    /// # }
    /// ```
    pub fn bind(self: &Arc<Self>, addr: &str) -> std::io::Result<GatewayServer> {
        // L'instantané initial n'est PAS celui qui sert : il n'alimente que
        // l'état du `Drop` du serveur lié — chaque requête re-publie un
        // snapshot depuis le gateway (voir `serve_with_gateway`).
        let bound = crate::http::BoundServer::bind_for_gateway(addr, self.snapshot())?;
        Ok(GatewayServer {
            inner: bound,
            gateway: Arc::clone(self),
        })
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

/// Serveur HTTP intégré lié à un [`Gateway`] dynamique — le résultat de
/// [`Gateway::bind`].
///
/// La plomberie est celle d'un [`crate::http::BoundServer`] : un thread par
/// connexion dans la limite du plafond, keep-alive borné, gardes
/// `Origin`/`Host`, arrêt gracieux. La seule différence est la source du
/// noyau : chaque requête re-publie un snapshot du gateway (cohérence **par
/// requête**, pas de snapshot figé).
pub struct GatewayServer {
    inner: crate::http::BoundServer,
    gateway: Arc<Gateway>,
}

impl GatewayServer {
    /// Adresse locale effective — utile après un bind sur le port 0.
    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.inner.local_addr()
    }

    /// Poignée d'arrêt — à prendre **avant** [`GatewayServer::serve`].
    /// [`crate::http::ServerHandle::shutdown`] est idempotente : la boucle
    /// d'accept cesse immédiatement, les connexions vivantes se ferment
    /// **entre deux requêtes**, la requête en cours termine proprement.
    #[must_use]
    pub fn handle(&self) -> crate::http::ServerHandle {
        self.inner.handle()
    }

    /// Bloque et sert les connexions — chaque requête est servie par le
    /// snapshot **courant** du gateway. Rend `Ok(())` après un arrêt demandé
    /// ([`GatewayServer::handle`]).
    pub fn serve(self) -> std::io::Result<()> {
        self.inner.serve_with_gateway(self.gateway)
    }
}

// Garde **anti-récursion** des dispatchs d'un gateway.
//
// Un handler qui re-dispatche sur le **même** gateway (`gw.dispatch` appelé
// depuis un outil) crée une récursion : chaque niveau empile dispatch →
// noyau → handler → dispatch. Au-delà de quelques dizaines de niveaux, la
// pile du thread déborde — et un débordement de pile n'est **pas** une
// panique : `catch_unwind` du transport ne l'attrape pas, tout le process
// meurt (audit 2026-11-13, § 6.2 — mesuré : exit `0xC00000FD` /
// `-1073741571`, `thread 'main' has overflowed its stack`).
//
// Détection : un compteur **thread-local** porte le nombre de dispatchs
// Gateway empilés sur le thread courant (même patron que `HELD_LOCK` :
// compteur thread-local, aucun `unsafe`, restauré par un garde au `Drop` —
// panique incluse). Au-delà de la borne, le niveau qui dépasserait rend une
// erreur `-32603` au lieu de continuer : l'appelant reçoit une enveloppe
// JSON-RPC propre et le process survit.
/// Profondeur maximale d'imbrication des dispatchs de gateway sur un thread
/// — le 33ᵉ niveau imbriqué est refusé en `-32603`. Largement au-dessus de tout
/// enchaînement légitime (un gateway qui relaie vers un autre serveur via
/// un outil ajoute un niveau par saut volontaire) et très en dessous du
/// débordement de pile réel.
const MAX_DISPATCH_DEPTH: usize = 32;

thread_local! {
    /// Nombre de dispatchs Gateway actuellement empilés sur le thread
    /// courant (`0` = aucun).
    static DISPATCH_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Restaure le compteur de profondeur à la sortie du dispatch (panique
/// incluse).
struct DispatchDepthGuard;

impl Drop for DispatchDepthGuard {
    fn drop(&mut self) {
        DISPATCH_DEPTH.set(DISPATCH_DEPTH.get() - 1);
    }
}

/// Profondeur de dispatch d'un gateway sur ce fil (`0` hors dispatch).
///
/// Le noyau (`src/server.rs`) la lit avant de lancer un handler borné par
/// `tool_timeout` et la réinstalle dans le fil du handler : la garde de
/// récursion est **thread-locale**, un fil neuf repartirait de `0` et
/// l'auto-appel d'un gateway gagnerait un niveau par saut de fil.
pub(crate) fn dispatch_depth() -> usize {
    DISPATCH_DEPTH.get()
}

/// Restaure la profondeur du fil appelant au `Drop`.
pub(crate) struct DepthBinding(usize);

impl Drop for DepthBinding {
    fn drop(&mut self) {
        DISPATCH_DEPTH.set(self.0);
    }
}

/// Installe `depth` comme profondeur courante sur ce fil — voir
/// [`dispatch_depth`].
pub(crate) fn bind_dispatch_depth(depth: usize) -> DepthBinding {
    DepthBinding(DISPATCH_DEPTH.replace(depth))
}

impl Gateway {
    /// Entre dans un dispatch gardé — `None` = profondeur maximale atteinte
    /// (l'appelant rend alors [`recursion_limit_response`] sans exécuter le
    /// corps).
    fn enter_dispatch() -> Option<DispatchDepthGuard> {
        let depth = DISPATCH_DEPTH.get();
        if depth >= MAX_DISPATCH_DEPTH {
            return None;
        }
        DISPATCH_DEPTH.set(depth + 1);
        Some(DispatchDepthGuard)
    }
}

/// Enveloppe d'erreur rendue au-delà de [`MAX_DISPATCH_DEPTH`] dispatchs
/// imbriqués sur le thread courant ; le code JSON-RPC employé est rendu avec
/// les octets, pour que le statut HTTP du chemin `dispatch_request` suive le
/// corps (400 pour une erreur de parsing).
///
/// `-32603` avec l'`id` de la requête quand il est lisible (JSON-RPC 2.0 §5 —
/// l'`id` d'un type interdit est rendu null, comme pour toute requête non
/// décodable) ; `None` pour une notification (sans champ `id` — aucune réponse
/// ne lui est jamais envoyée). Un corps **illisible** rend l'erreur de parsing
/// du chemin nominal (`-32700`, `id: null`) : `None` y laisserait le client
/// attendre une réponse qui n'arrive jamais.
fn recursion_limit_response(body: &[u8]) -> Option<(Vec<u8>, i64)> {
    let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(body) else {
        // Même verdict que le chemin nominal (`-32700`, `id: null`) : refuser
        // en silence ferait attendre un client qui a bien envoyé une requête.
        return Some((
            crate::jsonrpc::Outgoing::parse_error().to_vec(),
            crate::jsonrpc::ErrorObject::PARSE_ERROR,
        ));
    };
    let id = match parsed.get("id") {
        // Pas d'`id` : notification — le dispatch n'y répond jamais.
        None => return None,
        Some(value @ (serde_json::Value::String(_) | serde_json::Value::Number(_))) => {
            value.clone()
        }
        // `id: null` explicite ou type interdit : `id` null dans la réponse.
        Some(_) => serde_json::Value::Null,
    };
    Some((
        crate::jsonrpc::Outgoing::failure(
            id,
            crate::jsonrpc::ErrorObject::new(
                crate::jsonrpc::ErrorObject::INTERNAL_ERROR,
                format!(
                    "gateway recursion limit exceeded (more than {MAX_DISPATCH_DEPTH} \
                     nested dispatches on the same thread, across all gateways, \
                     from within a handler)"
                ),
            ),
        )
        .to_vec(),
        crate::jsonrpc::ErrorObject::INTERNAL_ERROR,
    ))
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
        let reply = gw
            .dispatch_unchecked(rpc("tools/list", 1).as_bytes())
            .unwrap();
        assert_eq!(tool_names(&reply), vec!["a".to_string()]);

        gw.edit(|b| b.tool("b", "outil ajouté", |_args| Ok(json!({ "from": "b" }))));

        // après l'edit : les deux outils, et l'appel du nouveau réussit
        let reply = gw
            .dispatch_unchecked(rpc("tools/list", 2).as_bytes())
            .unwrap();
        assert_eq!(tool_names(&reply), vec!["a".to_string(), "b".to_string()]);
        let reply = gw.dispatch_unchecked(call("b", 3).as_bytes()).unwrap();
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

        let reply = gw
            .dispatch_unchecked(rpc("tools/list", 1).as_bytes())
            .unwrap();
        assert_eq!(tool_names(&reply), vec!["a".to_string()]);
        let reply = gw.dispatch_unchecked(call("b", 2).as_bytes()).unwrap();
        let v: Value = serde_json::from_slice(&reply).unwrap();
        assert_eq!(v["error"]["code"], -32602, "outil retiré → Unknown tool");
        // l'outil restant continue d'être servi
        let reply = gw.dispatch_unchecked(call("a", 3).as_bytes()).unwrap();
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
        let reply = gw
            .dispatch_unchecked(rpc("tools/list", 1).as_bytes())
            .unwrap();
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
        let reply = gw
            .dispatch_unchecked(rpc("tools/list", 2).as_bytes())
            .unwrap();
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
                    let reply = gw
                        .dispatch_unchecked(rpc("tools/list", tours).as_bytes())
                        .unwrap();
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
        // Attente bornée à 5 s — un dispatch dure des microsecondes ; le
        // plafond ne sert que sur un runner pathologiquement saturé.
        // `checked_add` : aucun `+` sur `Instant` (débordement = panique).
        let echeance = std::time::Instant::now().checked_add(std::time::Duration::from_secs(5));
        for flag in &served {
            while !flag.load(std::sync::atomic::Ordering::Relaxed)
                && echeance.is_none_or(|fin| std::time::Instant::now() < fin)
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
        let reply = gw.dispatch_unchecked(call("a", 999).as_bytes()).unwrap();
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
            .dispatch_unchecked(br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
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
        let reply = gw
            .dispatch_unchecked(rpc("tools/list", 1).as_bytes())
            .unwrap();
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
        // Corps strict (profil par défaut) : `_meta` obligatoire, sinon le
        // refus de profil précède le contrôle des en-têtes.
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {
                "name": "a", "arguments": {},
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        })
        .to_string();
        // en-tête cohérent → réponse normale
        let headers = crate::server::McpHeaders {
            protocol_version: Some("2026-07-28".to_string()),
            method: Some("tools/call".to_string()),
            name: Some("a".to_string()),
            ..Default::default()
        };
        let reply = gw
            .dispatch_with_headers(body.as_bytes(), Some(&headers))
            .unwrap();
        assert!(serde_json::from_slice::<Value>(&reply).unwrap()["result"].is_object());
        // en-tête incohérent → -32020 (mismatch)
        let headers = crate::server::McpHeaders {
            protocol_version: Some("2026-07-28".to_string()),
            method: Some("ping".to_string()),
            name: None,
            ..Default::default()
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
        let reply = gw.dispatch_unchecked(call("rate", 1).as_bytes()).unwrap();
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
        let reply = gw
            .dispatch_unchecked(rpc("resources/list", 1).as_bytes())
            .unwrap();
        let v: Value = serde_json::from_slice(&reply).unwrap();
        assert_eq!(v["result"]["resources"][0]["uri"], "mcp://etat");
        let reply = gw
            .dispatch_unchecked(rpc("prompts/list", 2).as_bytes())
            .unwrap();
        let v: Value = serde_json::from_slice(&reply).unwrap();
        assert_eq!(v["result"]["prompts"][0]["name"], "salut");
        // et l'outil ajouté dans le MÊME edit est là aussi
        let reply = gw
            .dispatch_unchecked(rpc("tools/list", 3).as_bytes())
            .unwrap();
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

    // ---------------------------------------------- garde de récursion

    /// Un corps **illisible** au niveau limite rend la même erreur de parsing
    /// que le chemin nominal (`-32700`, `id: null`) — et non `None`, qui
    /// laissait le client attendre une réponse jamais écrite (audit
    /// 2026-11-13, R4). La profondeur est posée à la borne comme le ferait le
    /// 32ᵉ dispatch empilé ; le garde la restaure au `Drop`.
    #[test]
    fn corps_illisible_a_la_profondeur_limite_rend_l_erreur_de_parsing() {
        let gw = Gateway::new(build("a"));
        let _profondeur = bind_dispatch_depth(MAX_DISPATCH_DEPTH);
        for corps in [&b"pas du json"[..], b"{\"jsonrpc\":\"2.0\",", b""] {
            let reply = gw.dispatch(corps).unwrap_or_else(|| {
                panic!("profondeur limite + corps {corps:?} : une réponse est due, pas None")
            });
            let v: Value = serde_json::from_slice(&reply).unwrap();
            assert_eq!(
                v["error"]["code"],
                json!(-32700),
                "même verdict que le chemin nominal pour un corps illisible ({corps:?}) : {v}"
            );
            assert!(
                v["id"].is_null(),
                "un corps non décodable rend `id: null` ({corps:?}) : {v}"
            );
        }
    }
}
