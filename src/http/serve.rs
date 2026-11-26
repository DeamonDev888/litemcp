//! Serveur lié : `BoundServer`, boucle d'accept et poignée d'arrêt.

use super::conn::Conn;
use super::connection::{ConnectionEnv, CoreSource, handle_connection};
use super::response::{ResponseHead, respond_version, shutdown_gracefully, shutdown_plain};
use super::*;

/// Serveur HTTP attaché à un socket bindé
/// (résultat de [`crate::ServerBuilder::bind`]) — et moteur, derrière
/// [`crate::gateway::GatewayServer`], du socket dédié d'un gateway
/// ([`crate::Gateway::bind`]).
pub struct BoundServer {
    pub(crate) listener: TcpListener,
    pub(crate) core: Arc<ServerCore>,
    /// Partagés par `Arc` : aucun clone par connexion sur le chemin d'accept.
    pub(crate) route: Arc<String>,
    pub(crate) allow_origins: Arc<Vec<String>>,
    /// Hôtes acceptés dans `Host` (voir [`crate::ServerBuilder::allowed_hosts`]) —
    /// vide : règle locale par défaut.
    pub(crate) allowed_hosts: Arc<Vec<String>>,
    /// Hooks de requête ([`crate::ServerBuilder::on_request`]) — vide : le
    /// transport ne retient aucun en-tête et le chemin chaud est inchangé.
    pub(crate) request_hooks: Arc<Vec<Arc<RequestHook>>>,
    /// Liste `Access-Control-Allow-Headers` du préflight CORS — calculée
    /// **une fois au bind** (voir `ServerBuilder::finish`) et pré-jointe :
    /// la connexion l'annonce sans aucune allocation (B3, audit
    /// 2026-11-25).
    pub(crate) allow_preflight_headers: String,
    pub(crate) read_timeout: Duration,
    /// Plafond **explicite** de l'échéance globale d'écriture (`None` :
    /// défaut dérivé de `read_timeout`, voir [`write_deadline`]).
    pub(crate) write_deadline: Option<Duration>,
    pub(crate) max_connections: usize,
    /// Durée de vie maximale d'une connexion (garde-fou anti-saturation).
    pub(crate) connection_ttl: Duration,
    /// Connexions vivantes de **cette** instance — le plafond est par
    /// serveur, pas global au processus.
    pub(crate) live_connections: Arc<AtomicUsize>,
    /// Refus en cours d'écriture (503) — borné, pour que la boucle d'accept
    /// ne soit jamais bloquée par les refus.
    pub(crate) refusals_in_flight: Arc<AtomicUsize>,
    /// Demande d'arrêt (voir [`BoundServer::handle`]) — la boucle d'accept
    /// et les boucles keep-alive la consultent entre deux requêtes.
    pub(crate) shutdown: Arc<std::sync::atomic::AtomicBool>,
    /// Annonce au MCP Control : `(nom, port)` quand
    /// [`crate::ServerBuilder::announce`] a été appelé, `None` sinon. Présent
    /// ⇒ l'écriture de l'annonce a été **tentée** au bind ; le `Drop` de
    /// [`BoundServer`] tente le retrait correspondant (no-op si rien n'avait
    /// été écrit — le retrait ne part que si `(pid, port)` **et** l'identité
    /// d'instance (`instance_id`) du fichier correspondent).
    pub(crate) announce_name: Option<(String, u16)>,
    /// Identité d'instance de ce serveur lié — générée à chaque bind, publiée
    /// dans l'annonce (registry) quand `.announce()` est actif et exposée par
    /// la sonde `/health` (en-tête `X-Instance-Id`) : une sonde de registre
    /// peut alors prouver QUELLE instance occupe le port — un refus TCP n'y
    /// suffit pas et un port réutilisé non plus. Toujours non nulle ; le
    /// coût se limite au chemin froid de la sonde.
    pub(crate) instance_id: u64,
    /// Anti-zombie ([`crate::antizombie`]) — `Some` quand
    /// [`crate::ServerBuilder::anti_zombie`] est actif : le `Drop` draine
    /// les connexions (attente bornée) puis publie le bilan de fin de vie,
    /// lisible via [`ServerHandle::zombie_report`].
    pub(crate) zombie: Option<Arc<crate::antizombie::ZombieState>>,
    /// Configuration TLS 1.3 effective (feature `tls`) — `None` : socket en
    /// clair, chemin historique inchangé.
    #[cfg(feature = "tls")]
    pub(crate) tls: Option<Arc<rustls::ServerConfig>>,
    /// Empreinte du certificat servi (feuille), pour l'épinglage client.
    #[cfg(feature = "tls")]
    pub(crate) certificate_fingerprint: Option<crate::tls::CertificateFingerprint>,
}

impl BoundServer {
    /// Adresse locale effective — utile après un bind sur le port 0.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Empreinte du certificat TLS servi (feuille) : `sha256:` + SHA-256 du
    /// DER, hexadécimal minuscule — la valeur à épingler côté client
    /// ([`crate::auth::PinnedServer`]). `None` sans TLS configuré.
    #[cfg(feature = "tls")]
    #[must_use]
    pub fn certificate_fingerprint(&self) -> Option<crate::tls::CertificateFingerprint> {
        self.certificate_fingerprint
    }

    /// Échéance globale d'écriture effective : plafond explicite
    /// ([`crate::server::ServerBuilder::write_deadline`], relevé au minimum
    /// [`WRITE_DEADLINE_MIN`]) ou défaut dérivé de `read_timeout` (voir
    /// [`write_deadline`]).
    fn write_budget(&self) -> Duration {
        self.write_deadline.map_or_else(
            || write_deadline(self.read_timeout),
            |plafond| plafond.max(WRITE_DEADLINE_MIN),
        )
    }

    /// Poignée d'arrêt — cloneable, à prendre **avant** [`BoundServer::serve`].
    ///
    /// [`ServerHandle::shutdown`] est idempotente : la boucle d'accept
    /// cesse immédiatement (une socket de réveil la débloque), `serve()`
    /// rend `Ok(())`, et chaque connexion vivante se ferme **entre deux
    /// requêtes** — la requête en cours termine proprement. Une connexion
    /// idle (bloquée en lecture sans requête) se ferme à son
    /// [`crate::server::ServerBuilder::read_timeout`] : un arrêt ne peut
    /// pas interrompre une lecture bloquante en cours.
    ///
    /// ```no_run
    /// use litemcp::prelude::*;
    ///
    /// # fn main() -> std::io::Result<()> {
    /// let bound = ServerBuilder::new("calc", "1.0.0")
    ///     .tool("add", "Additionne", |_args| Ok(serde_json::json!({})))
    ///     .bind("127.0.0.1:23080")?;
    /// let handle = bound.handle();
    /// std::thread::spawn(move || {
    ///     let _ = bound.serve();
    /// });
    /// // … plus tard :
    /// handle.shutdown();
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn handle(&self) -> ServerHandle {
        let mut addr = self
            .listener
            .local_addr()
            .unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], 0)));
        // Un listener lié sur `0.0.0.0` ou `[::]` renvoie une IP non
        // spécifiée : sous Windows, `TcpStream::connect_timeout` vers
        // `0.0.0.0:port` échoue en `AddrNotAvailable` sans réveiller
        // `accept`. On substitue donc l'adresse de bouclage locale.
        if addr.ip().is_unspecified() {
            let loopback = match addr {
                SocketAddr::V4(_) => std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                SocketAddr::V6(_) => std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
            };
            addr.set_ip(loopback);
        }
        ServerHandle {
            shutdown: Arc::clone(&self.shutdown),
            addr,
            zombie: self.zombie.clone(),
        }
    }

    /// Bloque et sert les connexions — un thread par connexion, dans la
    /// limite de `max_connections` (défaut [`MAX_CONNECTIONS`]).
    ///
    /// Rend `Ok(())` quand [`BoundServer::handle`]/`ServerHandle::shutdown`
    /// est appelé — c'est le seul retour anticipé volontaire. À la fin de vie
    /// du serveur lié (arrêt gracieux **ou** `drop` sans `serve()`), le `Drop`
    /// retire l'annonce MCP Control (si [`crate::ServerBuilder::announce`]) :
    /// un moteur MCP Control ne voit plus un process arrêté proprement.
    pub fn serve(self) -> std::io::Result<()> {
        // Le retrait de l'annonce est assuré par `Drop for BoundServer`,
        // déclenché quand le `self` consommé ici finit de vivre — ce qui
        // couvre aussi un `BoundServer` lié puis droppé sans `serve()`.
        self.serve_loop(None)
    }

    /// Sert ce serveur en reprenant un snapshot du gateway **à chaque
    /// requête** — le socle de [`crate::gateway::GatewayServer::serve`]
    /// (voir [`crate::Gateway::bind`]). La plomberie est exactement celle
    /// de [`Self::serve`] : même boucle d'accept, mêmes plafonds, même
    /// arrêt gracieux.
    pub(crate) fn serve_with_gateway(
        self,
        gateway: Arc<crate::gateway::Gateway>,
    ) -> std::io::Result<()> {
        self.serve_loop(Some(gateway))
    }

    /// Boucle d'accept — corps de [`Self::serve`], qui ne fait plus que la
    /// déléguer (le retrait d'annonce vit dans le `Drop`). `gateway` :
    /// source dynamique du noyau — chaque requête re-publie un snapshot du
    /// gateway (voir [`crate::Gateway::bind`]) ; `None` : noyau figé du
    /// serveur lié (chemin historique).
    fn serve_loop(self, gateway: Option<Arc<crate::gateway::Gateway>>) -> std::io::Result<()> {
        let is_loopback_listener = self
            .listener
            .local_addr()
            .ok()
            .is_some_and(|a| a.ip().is_loopback());
        // Identité d'instance exposée par la sonde `/health` (chemin froid,
        // formatée une seule fois) : une sonde de registre lit cet en-tête
        // pour prouver quelle instance occupe le port.
        let instance_headers = format!("X-Instance-Id: {:016x}\r\n", self.instance_id);
        // Budget d'écriture des refus (échéance globale, voir `write_deadline`).
        // La version HTTP de la requête n'est **pas connue** ici : le refus
        // part sans lire la requête, la ligne de statut reste donc
        // `HTTP/1.1` (limite documentée, `docs/SECURITE.md`).
        let write_budget = self.write_budget();
        for stream in self.listener.incoming() {
            // Arrêt demandé : la socket de réveil (ou une connexion en
            // attente) est abandonnée ici — le listener se ferme au retour,
            // les threads de connexion vivotent jusqu'à leur prochaine
            // itération (voir `handle_connection`).
            if self.shutdown.load(Ordering::Relaxed) {
                return Ok(());
            }
            let Ok(mut stream) = stream else {
                // Échec d'accept (ex. descripteurs épuisés) : pause brève —
                // sans elle, une erreur persistante transforme la boucle
                // d'accept en boucle de spins à 100 % d'un cœur.
                std::thread::sleep(Duration::from_millis(10));
                continue;
            };
            // Timeouts posés avant toute écriture : même la réponse 503 ne
            // peut pas bloquer l'accept loop sur un client qui ne lit pas.
            let _ = stream.set_read_timeout(Some(self.read_timeout));
            let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
            let _ = stream.set_nodelay(true);
            // Plafond de concurrence (par serveur) : au-delà, on refuse net —
            // chaque connexion vivante coûte un thread + un buffer.
            let prev = self.live_connections.fetch_add(1, Ordering::Relaxed);
            if prev >= self.max_connections {
                self.live_connections.fetch_sub(1, Ordering::Relaxed);
                // Le refus (503) est écrit **hors de la boucle d'accept**.
                //
                // Pourquoi : fermer proprement sans perdre la réponse exige
                // d'attendre les octets déjà en vol (sinon le `close()` émet
                // un RST qui purge le 503 chez le client). Cette attente ne
                // doit pas retarder les accepts suivants — un refus coûtait
                // jusqu'à 240 ms de latence à la boucle.
                //
                // Le nombre de refus en cours est **borné** : au-delà, la
                // connexion est fermée sans réponse (mieux vaut un RST qu'une
                // avalanche de threads).
                let pending = Arc::clone(&self.refusals_in_flight);
                let slot = pending.fetch_add(1, Ordering::Relaxed);
                if slot < MAX_REFUSALS_IN_FLIGHT {
                    // Un clone pour le thread : l'original reste disponible
                    // pour le repli inline si le spawn échoue.
                    match stream.try_clone() {
                        Ok(mut worker) => {
                            let spawned = std::thread::Builder::new()
                                .name("litemcp-refus".into())
                                .spawn(move || {
                                    let _guard = RefusalGuard(pending);
                                    let _ = respond_version(
                                        &mut worker,
                                        &ResponseHead {
                                            version: "HTTP/1.1",
                                            status: "503 Service Unavailable",
                                            content_type: "application/json",
                                        },
                                        "",
                                        br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"too many connections","data":{"reason":"overloaded"}}}"#,
                                        false,
                                        false,
                                        write_budget,
                                    );
                                    // Drain + shutdown(Write) : sans cela, le
                                    // `close()` part avec la requête du client
                                    // encore dans le tampon de réception → RST
                                    // → le tampon du client (qui contient le
                                    // 503) est purgé et il ne lit JAMAIS la
                                    // réponse.
                                    shutdown_plain(&mut worker);
                                });
                            if spawned.is_ok() {
                                drop(stream);
                                continue;
                            }
                            // Spawn impossible : le garde n'a jamais vécu.
                            self.refusals_in_flight.fetch_sub(1, Ordering::Relaxed);
                        }
                        Err(_) => {
                            self.refusals_in_flight.fetch_sub(1, Ordering::Relaxed);
                        }
                    }
                } else {
                    // Trop de refus en vol : on rend le slot et on ferme SANS
                    // répondre — écrire un 503 inline (drain 50 ms) bloquerait
                    // la boucle d'accept (latence d'acceptation ×N pendant la
                    // vague). Mieux vaut un RST pour CE client qu'un serveur
                    // gelé pour tous : c'est ce que la doc promet.
                    pending.fetch_sub(1, Ordering::Relaxed);
                    drop(stream);
                    continue;
                }
                // Repli (spawn impossible uniquement) : écriture inline
                // bloquante, bornée par `shutdown_gracefully`.
                let _ = respond_version(
                    &mut stream,
                    &ResponseHead {
                        version: "HTTP/1.1",
                        status: "503 Service Unavailable",
                        content_type: "application/json",
                    },
                    "",
                    br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"server saturated","data":{"reason":"overloaded"}}}"#,
                    false,
                    false,
                    write_budget,
                );
                shutdown_plain(&mut stream);
                continue;
            }
            let core = Arc::clone(&self.core);
            // Gateway dynamique : un clone d'`Arc` par connexion — le
            // snapshot, lui, est repris à chaque requête (voir `CoreSource`
            // dans `handle_connection`).
            let gateway_conn = gateway.as_ref().map(Arc::clone);
            let route = Arc::clone(&self.route);
            let allow_origins = Arc::clone(&self.allow_origins);
            let allowed_hosts = Arc::clone(&self.allowed_hosts);
            let request_hooks = Arc::clone(&self.request_hooks);
            // Schéma externe annoncé au document RFC 9728 (`https` sous TLS).
            #[cfg(feature = "tls")]
            let scheme = if self.tls.is_some() { "https" } else { "http" };
            #[cfg(not(feature = "tls"))]
            let scheme = "http";
            let live = Arc::clone(&self.live_connections);
            let shutdown = Arc::clone(&self.shutdown);
            // Clone pour le chemin d'erreur ci-dessous : la closure appartient
            // au spawn — si la création du thread échoue, elle est abandonnée
            // avec le flux qu'elle capture, et la réponse 503 part sur le flux
            // d'origine, resté la propriété de la boucle.
            //
            // Connexion servie : session TLS 1.3 quand `.tls(...)` est
            // configuré, socket nue sinon (chemin historique inchangé).
            #[cfg(feature = "tls")]
            let mut conn = match &self.tls {
                Some(config) => match rustls::ServerConnection::new(Arc::clone(config)) {
                    Ok(session) => Conn::Tls(Arc::new(Mutex::new(rustls::StreamOwned {
                        conn: session,
                        sock: stream,
                    }))),
                    // Configuration de session impossible (mémoire) : le slot
                    // est rendu, la connexion abandonnée — jamais de panique.
                    Err(_) => {
                        self.live_connections.fetch_sub(1, Ordering::Relaxed);
                        continue;
                    }
                },
                None => Conn::Plain(stream),
            };
            #[cfg(not(feature = "tls"))]
            let mut conn = Conn::Plain(stream);
            let Ok(spawn_stream) = conn.try_clone() else {
                self.live_connections.fetch_sub(1, Ordering::Relaxed);
                continue;
            };
            let read_timeout = self.read_timeout;
            let connection_ttl = self.connection_ttl;
            // Possédé par la closure (comme les Arc ci-dessus) : l'en-tête
            // d'identité est pré-formaté une fois par serveur, cloné par
            // connexion (petite chaîne, chemin d'accept seulement).
            let instance_headers = instance_headers.clone();
            // Même mécanique : la liste du préflight est pré-jointe au bind,
            // clonée par connexion (petite chaîne, chemin d'accept).
            let allow_preflight_headers = self.allow_preflight_headers.clone();
            let spawned = std::thread::Builder::new()
                .name("litemcp-conn".into())
                .spawn(move || {
                    struct LiveGuard(Arc<AtomicUsize>);
                    impl Drop for LiveGuard {
                        fn drop(&mut self) {
                            self.0.fetch_sub(1, Ordering::Relaxed);
                        }
                    }
                    let _guard = LiveGuard(live);
                    // Source du noyau : gateway dynamique (snapshot repris
                    // à chaque requête) ou noyau figé du serveur lié.
                    let core = match gateway_conn.as_deref() {
                        Some(gw) => CoreSource::Gateway(gw),
                        None => CoreSource::Static(&core),
                    };
                    let _ = handle_connection(
                        spawn_stream,
                        ConnectionEnv {
                            core,
                            route: &route,
                            allow_origins: &allow_origins,
                            allowed_hosts: &allowed_hosts,
                            request_hooks: &request_hooks,
                            instance_headers: &instance_headers,
                            allow_preflight_headers: &allow_preflight_headers,
                            read_timeout,
                            write_budget,
                            connection_ttl,
                            shutdown: &shutdown,
                            is_loopback_listener,
                            scheme,
                        },
                    );
                });
            if spawned.is_err() {
                // Création de thread impossible (pression système) : défaillance
                // ponctuelle, jamais une raison de tuer le serveur entier. Le
                // garde de la closure n'a jamais vécu : on libère le plafond ici.
                self.live_connections.fetch_sub(1, Ordering::Relaxed);
                let _ = respond_version(
                    &mut conn,
                    &ResponseHead {
                        version: "HTTP/1.1",
                        status: "503 Service Unavailable",
                        content_type: "application/json",
                    },
                    "",
                    br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"server saturated","data":{"reason":"overloaded"}}}"#,
                    false,
                    false,
                    write_budget,
                );
                // Même correctif que le refus de saturation : drain + shutdown
                // pour que le client lise réellement le 503.
                shutdown_gracefully(&mut conn);
            }
        }
        Ok(())
    }
}

impl BoundServer {
    /// Lie un socket au nom d'un gateway dynamique — le socle de
    /// [`crate::Gateway::bind`]. La configuration du transport est celle du
    /// chemin [`crate::ServerBuilder::bind`] **sans aucune option** : route
    /// `/mcp`, aucun hook de requête, aucune origine CORS autorisée, hôtes
    /// locaux par défaut (règle `host_autorise`), timeouts et plafonds par
    /// défaut, pas d'annonce MCP Control, pas d'anti-zombie, pas de TLS —
    /// le gateway n'expose pas de configuration de certificat ; le TLS
    /// reste le fait d'un bind `ServerBuilder` ou d'un reverse proxy amont.
    ///
    /// Le noyau reçu n'est **pas** celui qui sert : c'est l'instantané
    /// initial du gateway, conservé pour l'état du `Drop` — chaque requête
    /// re-publie un snapshot depuis le gateway (câblé par
    /// `serve_with_gateway`, voir le `CoreSource` de la connexion).
    pub(crate) fn bind_for_gateway(addr: &str, core: Arc<ServerCore>) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
        Ok(BoundServer {
            listener,
            core,
            route: Arc::new("/mcp".to_string()),
            // Liste `Access-Control-Allow-Headers` du préflight : la même
            // base qu'un `ServerBuilder::finish` sans vérificateur ni
            // déclaration utilisateur (B3, audit 2026-11-25). Les
            // `Mcp-Param-*` déclarés par des outils ajoutés après le bind ne
            // réécrivent pas la liste — même limite que le chemin statique,
            // où la liste est figée au bind.
            allow_preflight_headers: String::from(
                "Content-Type, Accept, MCP-Protocol-Version, Mcp-Method, Mcp-Name",
            ),
            allow_origins: Arc::new(Vec::new()),
            allowed_hosts: Arc::new(Vec::new()),
            request_hooks: Arc::new(Vec::new()),
            read_timeout: READ_TIMEOUT,
            write_deadline: None,
            max_connections: MAX_CONNECTIONS,
            connection_ttl: CONNECTION_TTL,
            live_connections: Arc::new(AtomicUsize::new(0)),
            refusals_in_flight: Arc::new(AtomicUsize::new(0)),
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            announce_name: None,
            instance_id: crate::registry::generate_instance_id(port),
            zombie: None,
            #[cfg(feature = "tls")]
            tls: None,
            #[cfg(feature = "tls")]
            certificate_fingerprint: None,
        })
    }
}

/// Fin de vie d'un serveur lié : retrait de l'annonce MCP Control.
///
/// Le retrait vit dans le `Drop` plutôt que dans [`BoundServer::serve`] pour
/// couvrir **tous** les chemins de fin de vie : arrêt gracieux (`serve`
/// consomme `self`), serveur lié jamais servi (bind puis drop — sinon
/// l'annonce resterait, affirmant un serveur vivant), abandon du builder
/// après un bind partiel. Un arrêt brutal (kill) ne passe pas par ici : le
/// fichier reste, et le lecteur (MCP Control) tranche avec sa sonde de vie.
impl Drop for BoundServer {
    fn drop(&mut self) {
        if let Some((name, port)) = self.announce_name.take() {
            // Identité comprise : un redémarrage dans le même process sur le
            // même port a publié une autre instance — son annonce survit au
            // retrait de celle-ci (un port réutilisé ne prouve pas
            // l'identité de l'annonceur).
            crate::registry::retract_instance(&name, port, self.instance_id);
        }
        if let Some(zombie) = &self.zombie {
            // Drain borné des threads de connexion puis bilan — le rapport
            // devient lisible via `ServerHandle::zombie_report`. L'attente
            // est plafonnée par `drain_timeout` : un `Drop` ne pende jamais
            // indéfiniment. `handlers_alive` est lu APRÈS l'attente (closure) :
            // un handler qui a terminé pendant le drain n'est plus compté.
            zombie.finish(&self.live_connections, || self.core.live_handler_threads());
        }
    }
}

/// Poignée d'arrêt d'un serveur lié — voir [`BoundServer::handle`].
#[derive(Clone)]
pub struct ServerHandle {
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    addr: SocketAddr,
    /// Anti-zombie — partagé avec le [`BoundServer`] : le bilan publié par
    /// son `Drop` reste lisible ici, même après la fin du serveur.
    zombie: Option<Arc<crate::antizombie::ZombieState>>,
}

impl ServerHandle {
    /// Demande l'arrêt du serveur — idempotent.
    ///
    /// La boucle d'accept est réveillée par une socket factice : `serve()`
    /// rend `Ok(())` promptement. Les connexions vivantes ferment entre
    /// deux requêtes ; une connexion bloquée en lecture ferme à son
    /// `read_timeout`. Les threads déjà lancés ne sont **pas** tués — la
    /// requête en cours termine.
    pub fn shutdown(&self) {
        if !self.shutdown.swap(true, Ordering::Relaxed) {
            // Premier appel uniquement : réveil de l'accept (connect non
            // bloquant pour ne pas pendre si le listener meurt entre-temps).
            let _ = std::net::TcpStream::connect_timeout(&self.addr, Duration::from_millis(250));
        }
    }

    /// Bilan anti-zombie de fin de vie — `Some` après la fin de vie du
    /// [`BoundServer`] (arrêt gracieux ou `drop`) quand
    /// [`crate::ServerBuilder::anti_zombie`] était actif, `None` sinon
    /// (module désactivé) et tant que la fin de vie n'est pas atteinte.
    ///
    /// Le bilan est publié par le `Drop` du serveur lié — dans le thread qui
    /// le possédait : après un `serve()` dans un thread dédié, lisez-le
    /// après le `join` (ou constatez simplement que `Some` finit par
    /// arriver).
    #[must_use]
    pub fn zombie_report(&self) -> Option<crate::antizombie::ZombieReport> {
        self.zombie.as_ref().and_then(|state| state.report())
    }
}

/// Garde qui rend un slot de « refus en cours » en sortant de portée.
struct RefusalGuard(Arc<AtomicUsize>);

impl Drop for RefusalGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bornes du budget d'écriture : jamais sous 1 s, jamais au-delà de 60 s,
    /// et deux fois `read_timeout` entre les deux.
    #[test]
    fn write_deadline_est_borne_et_proportionnel() {
        assert_eq!(write_deadline(Duration::ZERO), WRITE_DEADLINE_MIN);
        assert_eq!(
            write_deadline(Duration::from_millis(300)),
            WRITE_DEADLINE_MIN
        );
        assert_eq!(
            write_deadline(Duration::from_secs(15)),
            Duration::from_secs(30)
        );
        assert_eq!(write_deadline(READ_TIMEOUT), WRITE_DEADLINE_MAX);
        assert_eq!(write_deadline(Duration::MAX), WRITE_DEADLINE_MAX);
    }

    /// `.write_deadline()` **remplace** le plafond par défaut dérivé de
    /// `read_timeout` — et une valeur sous la borne basse y est relevée.
    #[test]
    fn write_deadline_du_builder_remplace_le_defaut() {
        let defaut = crate::server::ServerBuilder::new("budget", "1.0.0")
            .read_timeout(Duration::from_millis(300))
            .bind("127.0.0.1:0")
            .expect("bind");
        assert_eq!(defaut.write_budget(), WRITE_DEADLINE_MIN);
        drop(defaut);

        let releve = crate::server::ServerBuilder::new("budget", "1.0.0")
            .read_timeout(Duration::from_millis(300))
            .write_deadline(Duration::from_secs(300))
            .bind("127.0.0.1:0")
            .expect("bind");
        assert_eq!(
            releve.write_budget(),
            Duration::from_secs(300),
            "le plafond explicite doit remplacer le défaut, sans être borné à 60 s"
        );
        drop(releve);

        // Défaut inchangé : `read_timeout × 2` borné à 60 s.
        let defaut_long = crate::server::ServerBuilder::new("budget", "1.0.0")
            .read_timeout(Duration::from_secs(15))
            .bind("127.0.0.1:0")
            .expect("bind");
        assert_eq!(defaut_long.write_budget(), Duration::from_secs(30));
    }
}
