//! Transport Streamable HTTP **stateless** — HTTP/1.1 minimal sur `std::net`.
//!
//! Une seule route (`POST /mcp` par défaut) : chaque message JSON-RPC est un
//! POST, la réponse arrive en `application/json` ou en `text/event-stream`
//! (SSE request-scoped, un seul événement de réponse) selon l'en-tête
//! `Accept`. Pas de session, pas de listener `GET` (405), garde
//! DNS-rebinding sur `Origin`.
//!
//! Durcissement :
//! - timeout de **lecture** par opération (30 s par défaut, `.read_timeout()`)
//!   **et deadline totale par requête** (60 s) : un client qui dribble des
//!   octets est coupé lui aussi ;
//! - **TTL de connexion** (5 min par défaut) : une connexion keep-alive qui
//!   envoie une requête toutes les 29 s ne peut pas retenir son slot
//!   indéfiniment (le deadline par requête est réinitialisé à chaque
//!   requête — sans TTL, le plafond était saturable durablement par 256
//!   clients lents) ;
//! - plafond de connexions simultanées (`MAX_CONNECTIONS`, défaut 256, par
//!   serveur) : au-delà, la connexion est refusée immédiatement (`503`) —
//!   pas de spawn illimité ;
//! - **le corps est lu par tranches** (buffer 64 Ko réutilisé) : un
//!   `Content-Length` de 1 Mio n'est plus pré-alloué, un client qui
//!   annonce 1 Mio et n'envoie rien ne coûte plus 1 Mio de commit ;
//! - limites `MAX_HEADERS` (64) / `MAX_LINE` (16 Kio) → `431`, `MAX_BODY`
//!   (1 Mio) → `413` ;
//! - `Transfer-Encoding` présent ou `Content-Length` dupliqué → `400`
//!   (pas de désynchronisation type request smuggling derrière un proxy) —
//!   **contrôle appliqué AVANT le routage**, donc sur toutes les routes
//!   y compris `/health` ;
//! - `Content-Length` malformé → `400` (RFC 9112 §6.3 ; le `411` est réservé
//!   à l'absence du champ) ;
//! - en-tête `Host` requis et unique en HTTP/1.1 (`400` sinon, RFC 9112
//!   §3.2), lignes obs-fold rejetées (`400`), ligne d'en-tête sans
//!   `:` ou avec une espace avant `:` rejetée (`400`, RFC 9112 §5.1) ;
//! - en-têtes à valeur unique **dupliqués** (`Origin`, `Content-Type`,
//!   `Mcp-Method`, `Mcp-Name`) → `400` : retenir la dernière valeur rendait
//!   le verdict dépendant de l'ordre, divergent d'un proxy qui combine les
//!   valeurs (RFC 9110 §5.3) ou retient la première ;
//! - ligne de requête stricte : trois jetons séparés par des **espaces
//!   ASCII** seulement (RFC 9112 §3), version exacte `HTTP/1.0`/`HTTP/1.1`
//!   sensible à la casse → `505` sinon ; ligne malformée → `400` ;
//! - **corps consommé sur TOUTES les routes** (`/health` compris) : un
//!   `Content-Length` non consommé ferait de la requête suivante le corps
//!   résiduel → désynchronisation de la file de réponses ;
//! - `HEAD` : toute réponse à un `HEAD` est **sans corps** (RFC 9110 §9.3.2)
//!   — `HEAD /health` donne les en-têtes du `GET` (sondes qui parlent HEAD) ;
//! - échec d'`accept()` : pause brève (pas de boucle de spins si les
//!   descripteurs s'épuisent) ; échec de création de thread → `503` sur
//!   **cette** connexion, le serveur continue (jamais de panique du serveur) ;
//! - `Expect: 100-continue` honoré, et toute réponse d'erreur **ferme la
//!   connexion** (`Connection: close`) : aucun octet résiduel ne peut être
//!   réinterprété comme une requête suivante — on ne draine pas le corps
//!   restant (le drain nourrirait un client qui inonde), au prix possible
//!   d'un RST côté client avant lecture de la réponse (compromis assumé) ;
//! - `POST /mcp?query=…` accepté (la query est ignorée) ;
//! - `HTTP/1.0` sans `Connection: keep-alive` → connexion fermée ;
//! - réponses SSE avec **en-têtes anti-tampon** (`X-Accel-Buffering: no`,
//!   `Cache-Control: no-cache, no-transform`) pour les hébergements derrière
//!   un proxy qui tamponne les flux ; choix architectural : **pas de SSE
//!   longue durée** — request-scoped, un événement puis clôture ;
//! - panique d'un handler capturée (`catch_unwind`) → `-32603` JSON-RPC
//!   **avec l'`id` de la requête** (la réponse reste corrélable), la
//!   connexion du client reste propre. NB : compiler avec
//!   `panic = "abort"` neutralise `catch_unwind` — une panique de handler
//!   arrête alors le processus entier.

use crate::server::ServerCore;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Taille maximale d'un corps de requête (1 Mio par défaut).
pub const MAX_BODY: usize = 1 << 20;

/// Taille des chunks de lecture du corps (borne la mémoire allouée par
/// une requête : un `Content-Length` de 1 Mio ne pré-alloue plus 1 Mio).
pub const BODY_CHUNK: usize = 64 * 1024;

/// Nombre maximal d'en-têtes par requête (au-delà → `431`).
pub const MAX_HEADERS: usize = 64;

/// Longueur maximale d'une ligne (requête ou en-tête) — au-delà → `431`.
pub const MAX_LINE: usize = 16 * 1024;

/// Timeout de lecture par opération (anti-slowloris) — défaut 30 s.
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Timeout d'écriture par opération — défaut 10 s.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Deadline totale par requête (lecture en-têtes + corps) — défaut 60 s.
/// Coupe aussi les clients qui driblent des octets sous le timeout par
/// opération.
pub const REQUEST_DEADLINE: Duration = Duration::from_secs(60);

/// Durée de vie maximale d'une connexion keep-alive — défaut 5 min.
///
/// Borne le temps d'occupation d'un slot du plafond : sans elle, un client
/// qui envoie une requête valide toutes les 29 s (sous le `READ_TIMEOUT`)
/// retient son slot **indéfiniment** et 256 clients lents suffisent à
/// saturer le serveur pour tous les autres.
pub const CONNECTION_TTL: Duration = Duration::from_secs(300);

/// Nombre maximal de refus (503) dont l'écriture est en cours **en
/// parallèle**. Au-delà, la connexion est fermée sans réponse : mieux vaut
/// un RST qu'une boucle d'`accept` (ou une avalanche de threads) bloquée.
pub const MAX_REFUSALS_IN_FLIGHT: usize = 64;

/// Plafond de connexions simultanées **par serveur** — au-delà, refus
/// immédiat (pas de spawn). Défaut 256.
pub const MAX_CONNECTIONS: usize = 256;

/// Chemin de la sonde de vie : `GET {HEALTH_ROUTE}` → `200` « ok » sans
/// JSON-RPC (probes k8s/systemd/équilibreurs — point fort repris de
/// ViteMCP). `HEAD` donne les mêmes en-têtes sans le corps ; toute autre
/// méthode → `405`.
pub const HEALTH_ROUTE: &str = "/health";

/// En-têtes anti-tampon émis sur toute réponse SSE. `X-Accel-Buffering:
/// no` est la directive **par réponse** que nginx honore même quand
/// `proxy_buffering` est actif globalement (le cas des VPS qu'on ne
/// contrôle pas) ; `no-transform` interdit aux intermédiaires de
/// retoucher le flux. Le JSON ne les porte pas : rien à tamponner.
pub const SSE_ANTI_BUFFERING: &str =
    "Cache-Control: no-cache, no-transform\r\nX-Accel-Buffering: no\r\n";

/// Serveur HTTP attaché à un socket bindé
/// (résultat de [`crate::ServerBuilder::bind`]).
pub struct BoundServer {
    pub(crate) listener: TcpListener,
    pub(crate) core: Arc<ServerCore>,
    /// Partagés par `Arc` : aucun clone par connexion sur le chemin d'accept.
    pub(crate) route: Arc<String>,
    pub(crate) allow_origins: Arc<Vec<String>>,
    pub(crate) read_timeout: Duration,
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
}

impl BoundServer {
    /// Adresse locale effective — utile après un bind sur le port 0.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
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
    ///     .bind("127.0.0.1:8080")?;
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
        }
    }

    /// Bloque et sert les connexions — un thread par connexion, dans la
    /// limite de `max_connections` (défaut [`MAX_CONNECTIONS`]).
    ///
    /// Rend `Ok(())` quand [`BoundServer::handle`]/`ServerHandle::shutdown`
    /// est appelé — c'est le seul retour anticipé volontaire.
    pub fn serve(self) -> std::io::Result<()> {
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
                                    let _ = respond(
                                        &mut worker,
                                        "503 Service Unavailable",
                                        "",
                                        "application/json",
                                        br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":"too many connections"}}"#,
                                        false,
                                        false,
                                    );
                                    // Drain + shutdown(Write) : sans cela, le
                                    // `close()` part avec la requête du client
                                    // encore dans le tampon de réception → RST
                                    // → le tampon du client (qui contient le
                                    // 503) est purgé et il ne lit JAMAIS la
                                    // réponse.
                                    shutdown_gracefully(&mut worker);
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
                    // Trop de refus en vol : on rend le slot et on ferme net.
                    pending.fetch_sub(1, Ordering::Relaxed);
                }
                // Repli : écriture inline (bloquante, bornée par
                // `shutdown_gracefully`) — utilisé seulement si le thread n'a
                // pas pu être créé.
                let _ = respond(
                    &mut &stream,
                    "503 Service Unavailable",
                    "",
                    "application/json",
                    br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":"server saturated"}}"#,
                    false,
                    false,
                );
                shutdown_gracefully(&mut stream);
                continue;
            }
            let core = Arc::clone(&self.core);
            let route = Arc::clone(&self.route);
            let allow_origins = Arc::clone(&self.allow_origins);
            let live = Arc::clone(&self.live_connections);
            let shutdown = Arc::clone(&self.shutdown);
            // Clone pour le chemin d'erreur ci-dessous : la closure appartient
            // au spawn — si la création du thread échoue, elle est abandonnée
            // avec le flux qu'elle capture, et la réponse 503 part sur le flux
            // d'origine, resté la propriété de la boucle.
            let Ok(spawn_stream) = stream.try_clone() else {
                self.live_connections.fetch_sub(1, Ordering::Relaxed);
                continue;
            };
            let read_timeout = self.read_timeout;
            let connection_ttl = self.connection_ttl;
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
                    let _ = handle_connection(
                        spawn_stream,
                        &core,
                        &route,
                        &allow_origins,
                        read_timeout,
                        connection_ttl,
                        &shutdown,
                    );
                });
            if spawned.is_err() {
                // Création de thread impossible (pression système) : défaillance
                // ponctuelle, jamais une raison de tuer le serveur entier. Le
                // garde de la closure n'a jamais vécu : on libère le plafond ici.
                self.live_connections.fetch_sub(1, Ordering::Relaxed);
                let _ = respond(
                    &mut &stream,
                    "503 Service Unavailable",
                    "",
                    "application/json",
                    br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":"server saturated"}}"#,
                    false,
                    false,
                );
                // Même correctif que le refus de saturation : drain + shutdown
                // pour que le client lise réellement le 503.
                shutdown_gracefully(&mut stream);
            }
        }
        Ok(())
    }
}

/// Poignée d'arrêt d'un serveur lié — voir [`BoundServer::handle`].
#[derive(Clone)]
pub struct ServerHandle {
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    addr: SocketAddr,
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
}

/// Garde qui rend un slot de « refus en cours » en sortant de portée.
struct RefusalGuard(Arc<AtomicUsize>);

impl Drop for RefusalGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Ferme une socket **sans perdre la réponse déjà écrite**.
///
/// Le mécanisme : un `close()` alors que des octets reçus n'ont pas été lus
/// fait émettre un **RST** par le noyau — et le RST purge le tampon de
/// réception du client, qui contient la réponse qu'on vient d'écrire. Le
/// client voit alors « connexion réinitialisée » au lieu du message.
///
/// La parade (comme nginx/haproxy) : `shutdown(Write)` pour envoyer un FIN
/// propre, puis **vider le tampon de réception** avant de laisser le
/// `close()` se faire.
///
/// Le vidage est **borné à 50 ms** : assez pour laisser arriver les octets
/// déjà en vol (sans quoi le `close()` émet un RST et le client perd la
/// réponse — mesuré : 9/15 sans attente, 15/15 avec). Comme les refus sont
/// écrits hors de la boucle d'`accept`, cette attente ne retarde pas les
/// accepts suivants.
fn shutdown_gracefully(stream: &mut TcpStream) {
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
    let mut sink = [0u8; 8192];
    let mut drained = 0usize;
    loop {
        match stream.read(&mut sink) {
            Ok(0) => break,
            Ok(n) => {
                drained += n;
                if drained >= 64 * 1024 {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

/// Échec de lecture d'une ligne : trop longue, client trop lent, ou
/// connexion fermée.
enum LineFail {
    TooLong,
    Slow,
    Closed,
}

/// Ajuste le timeout de lecture de la socket si la `deadline` résiduelle
/// est plus courte que `read_timeout`.
///
/// Sur le chemin chaud nominal (`remaining >= read_timeout`), aucun syscall
/// n'est émis. Lorsque la deadline de requête ou le `connection_ttl` approche,
/// la lecture bloquante ne peut plus déborder de `read_timeout` au-delà de
/// l'échéance.
fn tighten_read_timeout(
    stream: &TcpStream,
    read_timeout: Duration,
    deadline: Instant,
) -> Option<()> {
    let now = Instant::now();
    if now >= deadline {
        return None;
    }
    let remaining = deadline.saturating_duration_since(now);
    if remaining < read_timeout {
        let _ = stream.set_read_timeout(Some(remaining.max(Duration::from_millis(1))));
    }
    Some(())
}

/// Lit une ligne (jusqu'à `\n` inclus) dans `out` — sans jamais la dépasser
/// `cap` octets.
///
/// Contrairement à `BufRead::read_line`, la mémoire consommée est bornée :
/// un client hostile ne peut pas faire grossir le buffer au-delà de `cap`.
/// `out` et `scratch` sont réutilisés d'un appel à l'autre (le chemin chaud
/// n'alloue que si la ligne dépasse la capacité retenue). La ligne doit être
/// du UTF-8 valide (sinon `Closed` → coupure propre) et la `deadline`
/// globale est contrôlée à chaque remplissage — un client qui drible des
/// octets est coupé aussi.
fn read_line_capped(
    reader: &mut BufReader<TcpStream>,
    out: &mut String,
    scratch: &mut Vec<u8>,
    cap: usize,
    read_timeout: Duration,
    deadline: Instant,
) -> Result<(), LineFail> {
    out.clear();
    scratch.clear();
    loop {
        if tighten_read_timeout(reader.get_ref(), read_timeout, deadline).is_none() {
            return Err(LineFail::Slow);
        }
        let available = match reader.fill_buf() {
            Ok(bytes) => bytes,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(LineFail::Slow);
            }
            Err(_) => return Err(LineFail::Closed),
        };
        if available.is_empty() {
            return Err(LineFail::Closed);
        }
        if let Some(pos) = available.iter().position(|&byte| byte == b'\n') {
            let chunk = &available[..=pos];
            if scratch.len() + chunk.len() > cap {
                return Err(LineFail::TooLong);
            }
            scratch.extend_from_slice(chunk);
            reader.consume(pos + 1);
            return match std::str::from_utf8(scratch) {
                Ok(text) => {
                    // **CRLF strict** (RFC 9112 §2.2 : la fin de ligne est
                    // CRLF). Un LF seul (« httpoxy ») est coupé immédiatement.
                    if !text.ends_with("\r\n") {
                        return Err(LineFail::Closed);
                    }
                    out.push_str(text);
                    Ok(())
                }
                Err(_) => Err(LineFail::Closed), // non-UTF8 : coupure propre
            };
        }
        if scratch.len() + available.len() > cap {
            return Err(LineFail::TooLong);
        }
        scratch.extend_from_slice(available);
        let filled = available.len();
        reader.consume(filled);
    }
}

fn handle_connection(
    stream: TcpStream,
    core: &ServerCore,
    route: &str,
    allow_origins: &[String],
    read_timeout: Duration,
    connection_ttl: Duration,
    shutdown: &std::sync::atomic::AtomicBool,
) -> std::io::Result<()> {
    // Buffer de lecture 4 Ko (et non les 8 Ko par défaut) : les en-têtes
    // tiennent largement dedans, et un corps plus grand que la capacité est
    // lu directement depuis la socket — moitié moins de mémoire par
    // connexion (1 Mio au plafond de 256 connexions).
    let mut reader = BufReader::with_capacity(4 * 1024, stream.try_clone()?);
    let mut stream = stream;
    // Buffers réutilisés sur toute la connexion (keep-alive inclus) : le
    // chemin chaud n'alloue que l'en-tête de réponse et la réplique JSON-RPC.
    let mut line = String::new();
    let mut scratch = Vec::new();
    let mut body = Vec::new();
    // Instant d'ouverture de la connexion : borne sa durée de vie totale.
    let connection_started = Instant::now();
    let connection_deadline = if connection_ttl.is_zero() {
        None
    } else {
        Some(connection_started + connection_ttl)
    };
    // Budget total d'une requête : proportionnel à `read_timeout` (×2, borné
    // à `REQUEST_DEADLINE` = 60 s). Un serveur configuré avec un
    // `read_timeout` court (ex. 400 ms) coupe aussi un client qui dribble
    // sous `read_timeout` en 800 ms au lieu d'attendre 60 s.
    let request_budget = read_timeout
        .saturating_mul(2)
        .min(REQUEST_DEADLINE)
        .max(read_timeout);
    loop {
        // Arrêt demandé : la connexion se ferme **entre deux requêtes** —
        // celle en cours a terminé proprement, la suivante ne démarre pas.
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        // TTL de connexion : au-delà, la connexion est fermée même si le
        // client est actif. Sans cette borne, un client qui drip-feed des
        // requêtes valides (sous le read_timeout) retient son slot de
        // connexion **indéfiniment** → 256 clients lents suffisent à
        // saturer le plafond pour tout le monde.
        //
        // TTL nul = pas de TTL (désactivé), comme documenté sur
        // [`crate::server::ServerBuilder::connection_ttl`].
        let now = Instant::now();
        if let Some(conn_end) = connection_deadline {
            if now >= conn_end {
                break;
            }
        }
        let mut deadline = now + request_budget;
        if let Some(conn_end) = connection_deadline {
            if conn_end < deadline {
                deadline = conn_end;
            }
        }

        // Ligne de requête — bornée, deadline globale.
        match read_line_capped(
            &mut reader,
            &mut line,
            &mut scratch,
            MAX_LINE,
            read_timeout,
            deadline,
        ) {
            Ok(()) => {}
            Err(LineFail::TooLong) => {
                respond_error(
                    &mut stream,
                    "431 Request Header Fields Too Large",
                    "line too long",
                    "",
                    false,
                )?;
                return Ok(());
            }
            // slowloris (timeout OU deadline globale) ou coupure : sans réponse
            Err(_) => return Ok(()),
        }
        // RFC 9112 §3 : la ligne de requête n'accepte qu'un **unique** espace
        // ASCII (SP) entre ses trois jetons (`method SP request-target SP
        // HTTP-version`), sans espace initial ni final.
        //
        // Le CRLF final est retiré **exactement une fois** avant le
        // découpage. Un CR ou LF résiduel *à l'intérieur* de la ligne
        // est refusé : `GET /health HTTP/1.1\r\r\n` est lu différemment par
        // un proxy strict (RFC 9112 §2.2 — fin de ligne = CRLF unique).
        let request_line = line.strip_suffix("\r\n").unwrap_or(&line);
        if request_line.contains(['\r', '\n']) {
            respond_error(
                &mut stream,
                "400 Bad Request",
                "invalid line ending in the request line",
                "",
                false,
            )?;
            return Ok(());
        }
        // Pas de `.filter(|p| !p.is_empty())` : un espace initial, final ou
        // doublé (`POST  /mcp HTTP/1.1`) produit un jeton vide et doit être
        // refusé en 400 (RFC 9112 §3).
        let mut parts = request_line.split(' ');
        let verb = parts.next().unwrap_or("");
        let full_path = parts.next().unwrap_or("");
        let version = parts.next().unwrap_or("");
        // Ligne de requête stricte : exactement trois jetons (méthode, cible,
        // version). RFC 9112 §3.
        if verb.is_empty() || full_path.is_empty() || version.is_empty() || parts.next().is_some() {
            respond_error(
                &mut stream,
                "400 Bad Request",
                "malformed request line",
                "",
                verb == "HEAD",
            )?;
            return Ok(());
        }
        // Le HTAB et les caractères de contrôle ne sont pas autorisés dans
        // la ligne de requête (RFC 9112 §3 : SP comme séparateur).
        if verb.contains('\t')
            || full_path.contains('\t')
            || version.contains('\t')
            || verb.chars().any(|c| c.is_control())
            || full_path.chars().any(|c| c.is_control())
            || version.chars().any(|c| c.is_control())
        {
            respond_error(
                &mut stream,
                "400 Bad Request",
                "control character in the request line",
                "",
                verb == "HEAD",
            )?;
            return Ok(());
        }
        // RFC 9110 §9.1 : `method = token` (`1*tchar`) ; RFC 9112 §3.2.1 :
        // `origin-form = absolute-path [ "?" query ]` commençant par `/`, en
        // ASCII visible et sans fragment `#`.
        if !is_valid_token(verb) || !is_valid_request_target(full_path) {
            respond_error(
                &mut stream,
                "400 Bad Request",
                "malformed request line",
                "",
                verb == "HEAD",
            )?;
            return Ok(());
        }
        // Version sensible à la casse (RFC 9112 §2.3 : le protocole est un
        // jeton exact) — `http/1.1` n'est pas une version, c'est du garbage.
        let http10 = version == "HTTP/1.0";
        if !http10 && version != "HTTP/1.1" {
            respond_error(
                &mut stream,
                "505 HTTP Version Not Supported",
                "unsupported HTTP version",
                "",
                verb == "HEAD",
            )?;
            return Ok(());
        }
        // La query string est ignorée : POST /mcp?x=1 == POST /mcp.
        // Verdicts calculés maintenant : `line` est réemprunté (mutable)
        // par la lecture des en-têtes, on ne garde que les booléens.
        let path = full_path.split('?').next().unwrap_or(full_path);
        let is_post = verb == "POST";
        let is_get = verb == "GET";
        let is_head = verb == "HEAD";
        let route_matched = path == route;
        let health_probe = path == HEALTH_ROUTE;

        let mut content_length: Option<usize> = None;
        let mut saw_content_length = false;
        let mut malformed_length = false;
        let mut duplicate_length = false;
        let mut transfer_encoding = false;
        // Origin/Host/Sec-Fetch-Site sans allocation : seul le verdict est
        // conservé, jamais la valeur.
        let mut origin_seen = false;
        let mut origin_duplicate = false;
        let mut origin_malformed = false;
        let mut origin_allowed = false;
        let mut sec_fetch_site_seen = false;
        let mut sec_fetch_site_duplicate = false;
        let mut sec_fetch_site_cross = false;
        let mut host_seen = false;
        let mut host_duplicate = false;
        let mut host_invalid = false;
        let mut accept_json = false;
        let mut accept_sse = false;
        let mut expect_continue = false;
        let mut expect_invalid = false;
        // `Content-Type` et `Content-Encoding` de la requête — vérifiés sans
        // allocation (seuls la présence et le verdict sont retenus).
        let mut content_type_seen = false;
        let mut content_type_json = false;
        let mut content_type_duplicate = false;
        let mut content_encoding_seen = false;
        let mut content_encoding_duplicate = false;
        // En-têtes de routage MCP 2026-07-28 (SEP-2243) — optionnels chez
        // nous (compatibilité), mais vérifiés s'ils sont présents.
        let mut mcp_method: Option<String> = None;
        let mut mcp_name: Option<String> = None;
        let mut mcp_method_duplicate = false;
        let mut mcp_name_duplicate = false;
        // `Connection: close` a-t-il été vu ? (HTTP/1.0 : la connexion est
        // fermée par défaut, `keep-alive` doit pouvoir la maintenir ouverte,
        // mais `close` reste ABSORBANT s'il apparaît).
        let mut connection_close_seen = false;
        let mut close = http10; // HTTP/1.0 : fermeture par défaut
        let mut header_count = 0usize;
        loop {
            match read_line_capped(
                &mut reader,
                &mut line,
                &mut scratch,
                MAX_LINE,
                read_timeout,
                deadline,
            ) {
                Ok(()) => {}
                Err(LineFail::TooLong) => {
                    respond_error(
                        &mut stream,
                        "431 Request Header Fields Too Large",
                        "line too long",
                        "",
                        is_head,
                    )?;
                    return Ok(());
                }
                Err(_) => return Ok(()),
            }
            // **Attention sécurité (anti-smuggling)** : on retire uniquement
            // le suffixe `\r\n` exact (jamais `.trim_end()`). Avec
            // `trim_end()`, une ligne composée d'espaces ou de tabulations
            // (`" \r\n"` ou `"\t\r\n"`) devenait vide *avant* le contrôle
            // `obs-fold` et terminait prématurément les en-têtes côté
            // serveur tandis qu'un proxy amont continuait à lire les
            // en-têtes suivants.
            let header = line.strip_suffix("\r\n").unwrap_or(&line);
            if header.is_empty() {
                break;
            }
            // Aucun `\r` ou `\n` résiduel n'est admis à l'intérieur d'une
            // ligne d'en-tête (`X: a\rTransfer-Encoding: chunked\r\n` ou
            // `\r\r\n` créeraient une divergence de découpage avec un proxy).
            if header.contains(['\r', '\n']) {
                respond_error(
                    &mut stream,
                    "400 Bad Request",
                    "invalid line ending in header",
                    "",
                    is_head,
                )?;
                return Ok(());
            }
            // obs-fold (RFC 9112 §5.2, obsolète) : une ligne de continuation
            // serait susceptible d'être réassemblée différemment par un proxy
            // — refus net plutôt qu'acceptation ambiguë.
            if header.starts_with(' ') || header.starts_with('\t') {
                respond_error(
                    &mut stream,
                    "400 Bad Request",
                    "obsolete header line folding (obs-fold)",
                    "",
                    is_head,
                )?;
                return Ok(());
            }
            header_count += 1;
            if header_count > MAX_HEADERS {
                respond_error(
                    &mut stream,
                    "431 Request Header Fields Too Large",
                    "too many headers",
                    "",
                    is_head,
                )?;
                return Ok(());
            }
            if let Some((name_raw, value_raw)) = header.split_once(':') {
                // RFC 9112 §5.1 : aucun espace entre le nom de champ et le
                // deux-points — une telle ligne est lue autrement par un
                // proxy strict (désynchronisation potentielle), refus net.
                if name_raw.ends_with(' ') || name_raw.ends_with('\t') {
                    respond_error(
                        &mut stream,
                        "400 Bad Request",
                        "whitespace before colon",
                        "",
                        is_head,
                    )?;
                    return Ok(());
                }
                if name_raw.is_empty() {
                    respond_error(
                        &mut stream,
                        "400 Bad Request",
                        "empty header name",
                        "",
                        is_head,
                    )?;
                    return Ok(());
                }
                // RFC 9110 §5.1 / §5.6.2 : `field-name = token` (`1*tchar`).
                // Tout espace Unicode, caractère de contrôle ou délimiteur
                // dans le nom d'en-tête est refusé sans ambiguïté.
                if !is_valid_token(name_raw) {
                    respond_error(
                        &mut stream,
                        "400 Bad Request",
                        "invalid header name",
                        "",
                        is_head,
                    )?;
                    return Ok(());
                }
                // RFC 9110 §5.5 : aucun caractère de contrôle (NUL, VT, FF,
                // DEL, contrôles C0/C1 hors HTAB) n'est autorisé dans une
                // valeur d'en-tête, et seul l'OWS HTTP (`SP` / `HTAB`) est
                // retiré aux extrémités (jamais `.trim()` qui absorberait
                // `\x0b`, `\x0c` ou des espaces Unicode `\u{00a0}`).
                if !is_valid_field_value(value_raw) {
                    respond_error(
                        &mut stream,
                        "400 Bad Request",
                        "control character in header value",
                        "",
                        is_head,
                    )?;
                    return Ok(());
                }
                let name = name_raw;
                let value = trim_ows(value_raw);
                if name.eq_ignore_ascii_case("content-length") {
                    if saw_content_length {
                        duplicate_length = true;
                    } else {
                        saw_content_length = true;
                        if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
                            content_length = value.parse().ok();
                            if content_length.is_none() {
                                // Que des chiffres mais dépasse usize :
                                // malformé lui aussi.
                                malformed_length = true;
                            }
                        } else {
                            malformed_length = true;
                        }
                    }
                } else if name.eq_ignore_ascii_case("transfer-encoding") {
                    transfer_encoding = true; // TE : refusé
                } else if name.eq_ignore_ascii_case("origin") {
                    if origin_seen {
                        origin_duplicate = true;
                    } else {
                        origin_seen = true;
                        if !is_valid_origin_syntax(value) {
                            origin_malformed = true;
                        } else {
                            // `Origin: null` (iframe sandboxée / data: URI) est
                            // une origine opaque forgeable depuis n'importe quel
                            // site tiers : jamais autorisée, même si `"null"`
                            // figurait par erreur dans `allow_origins`.
                            origin_allowed = !value.eq_ignore_ascii_case("null")
                                && allow_origins.iter().any(|o| o.eq_ignore_ascii_case(value));
                        }
                    }
                } else if name.eq_ignore_ascii_case("sec-fetch-site") {
                    if sec_fetch_site_seen {
                        sec_fetch_site_duplicate = true;
                    } else {
                        sec_fetch_site_seen = true;
                        sec_fetch_site_cross = value.eq_ignore_ascii_case("cross-site");
                    }
                } else if name.eq_ignore_ascii_case("host") {
                    if host_seen {
                        host_duplicate = true;
                    } else {
                        host_seen = true;
                        if !is_valid_host(value) {
                            host_invalid = true;
                        }
                    }
                } else if name.eq_ignore_ascii_case("accept") {
                    // Correspondance **par jeton exact** : `Accept:
                    // text/event-stream-x` ne doit pas être pris pour
                    // `text/event-stream`. Un paramètre `q=0` disqualifie le
                    // type de média (RFC 9110 §12.5.1).
                    accept_json |= header_has_media_type(value, "application/json");
                    accept_sse |= header_has_media_type(value, "text/event-stream");
                } else if name.eq_ignore_ascii_case("content-type") {
                    if content_type_seen {
                        content_type_duplicate = true;
                    } else {
                        content_type_seen = true;
                        content_type_json = is_json_content_type(value);
                    }
                } else if name.eq_ignore_ascii_case("content-encoding") {
                    if content_encoding_seen {
                        content_encoding_duplicate = true;
                    } else {
                        content_encoding_seen = true;
                    }
                } else if name.eq_ignore_ascii_case("mcp-method") {
                    if mcp_method.is_some() {
                        mcp_method_duplicate = true;
                    } else {
                        mcp_method = Some(value.to_string());
                    }
                } else if name.eq_ignore_ascii_case("mcp-name") {
                    if mcp_name.is_some() {
                        mcp_name_duplicate = true;
                    } else {
                        mcp_name = Some(value.to_string());
                    }
                } else if name.eq_ignore_ascii_case("connection") {
                    let mut has_close = false;
                    let mut has_keep_alive = false;
                    for token in value.split(',') {
                        let token = trim_ows(token);
                        if token.eq_ignore_ascii_case("close") {
                            has_close = true;
                        } else if token.eq_ignore_ascii_case("keep-alive") {
                            has_keep_alive = true;
                        }
                    }
                    if has_close {
                        close = true;
                        connection_close_seen = true;
                    }
                    if http10 && !connection_close_seen {
                        // HTTP/1.0 : la connexion est fermée par défaut ;
                        // seul `Connection: keep-alive` la maintient ouverte.
                        //
                        // `Connection: close` est **absorbant** : une fois
                        // présent, il reste — même si un `keep-alive` suit
                        // dans un autre en-tête (RFC 9112 §9.6 : « close »
                        // termine la connexion).
                        if has_keep_alive {
                            close = false;
                        }
                    }
                } else if name.eq_ignore_ascii_case("expect") {
                    // RFC 9110 §10.1.1 : toute attente autre que `100-continue`
                    // DOIT recevoir `417 Expectation Failed`.
                    if parse_expect_100_continue(value) {
                        if !expect_invalid {
                            expect_continue = true;
                        }
                    } else {
                        expect_invalid = true;
                        expect_continue = false;
                    }
                }
            } else {
                // RFC 9112 §5 : une ligne d'en-tête sans deux-points est
                // malformée — l'ignorer reviendrait à l'accepter sous une
                // lecture différente de celle d'un proxy en amont.
                respond_error(
                    &mut stream,
                    "400 Bad Request",
                    "malformed header",
                    "",
                    is_head,
                )?;
                return Ok(());
            }
        }

        // En-tete `Origin` duplique ou syntaxiquement invalide : refus 400.
        if origin_duplicate {
            respond_error(
                &mut stream,
                "400 Bad Request",
                "duplicated Origin header",
                "",
                is_head,
            )?;
            return Ok(());
        }
        if origin_malformed {
            respond_error(
                &mut stream,
                "400 Bad Request",
                "malformed Origin header",
                "",
                is_head,
            )?;
            return Ok(());
        }

        // En-têtes à valeur unique dupliqués (`Content-Type`,
        // `Content-Encoding`, `Sec-Fetch-Site`, `Mcp-Method`, `Mcp-Name`) :
        // requête ambiguë — refus net 400.
        if content_type_duplicate
            || content_encoding_duplicate
            || sec_fetch_site_duplicate
            || mcp_method_duplicate
            || mcp_name_duplicate
        {
            respond_error(
                &mut stream,
                "400 Bad Request",
                "duplicated single-value header",
                "",
                is_head,
            )?;
            return Ok(());
        }

        // Garde DNS-rebinding & Fetch Metadata :
        // 1) tout `Origin` non autorisé (ou `Origin: null`) → 403 ;
        // 2) `Sec-Fetch-Site: cross-site` sans `Origin` explicitement
        //    autorisé → 403 (défense en profondeur si `Origin` est omis).
        if (origin_seen && !origin_allowed)
            || (sec_fetch_site_cross && !(origin_seen && origin_allowed))
        {
            respond_error(
                &mut stream,
                "403 Forbidden",
                "origin not allowed",
                "",
                is_head,
            )?;
            return Ok(());
        }

        // RFC 9112 §3.2 : `Host` obligatoire en HTTP/1.1 ; et sur toute
        // version HTTP, un `Host` dupliqué ou de valeur invalide DOIT être
        // refusé en 400.
        if !http10 && !host_seen {
            respond_error(
                &mut stream,
                "400 Bad Request",
                "Host header required",
                "",
                is_head,
            )?;
            return Ok(());
        }
        if host_duplicate {
            respond_error(
                &mut stream,
                "400 Bad Request",
                "duplicated Host header",
                "",
                is_head,
            )?;
            return Ok(());
        }
        if host_invalid {
            respond_error(
                &mut stream,
                "400 Bad Request",
                "invalid Host header",
                "",
                is_head,
            )?;
            return Ok(());
        }

        // Anti request-smuggling : TE présent ou CL dupliqué = requête
        // ambigüe, refusée sans ambiguïté (RFC 9112 §6).
        //
        // **Contrôle AVANT le routage** : il s'applique donc à TOUTES les
        // routes, y compris `/health`. Placé après le routage (ancienne
        // version), `Transfer-Encoding: chunked` sur `/health` était
        // accepté (200) et les octets chunked résiduels étaient relus
        // comme une requête.
        if transfer_encoding || duplicate_length {
            respond_error(
                &mut stream,
                "400 Bad Request",
                "ambiguous request framing",
                "",
                is_head,
            )?;
            return Ok(());
        }

        // RFC 9110 §10.1.1 : un en-tête `Expect` portant une valeur autre
        // que `100-continue` DOIT être refusé en `417 Expectation Failed`.
        if expect_invalid {
            respond_error(
                &mut stream,
                "417 Expectation Failed",
                "unsupported Expect header",
                "",
                is_head,
            )?;
            return Ok(());
        }

        // Longueur du corps — calculée avant le routage pour que la
        // consommation du corps (obligatoire sur toute route) soit
        // uniforme.
        let len = match content_length {
            Some(len) => len,
            None if malformed_length => {
                // RFC 9112 §6.3 : un Content-Length présent mais invalide
                // (non numérique ou hors usize) rend la requête invalide.
                respond_error(
                    &mut stream,
                    "400 Bad Request",
                    "invalid Content-Length",
                    "",
                    is_head,
                )?;
                return Ok(());
            }
            // Pas de Content-Length : pour GET/HEAD c'est normal (aucun
            // corps attendu) ; pour POST c'est une requête invalide.
            None => {
                if !is_post {
                    0
                } else {
                    respond_error(
                        &mut stream,
                        "411 Length Required",
                        "Content-Length required",
                        "",
                        is_head,
                    )?;
                    return Ok(());
                }
            }
        };
        if len > MAX_BODY {
            // Le client peut déjà streamer le corps : on répond puis on
            // ferme — drainer 1 Mio+ nourrirait l'attaque qu'on refuse.
            respond_error(
                &mut stream,
                "413 Payload Too Large",
                "body too large",
                "",
                is_head,
            )?;
            return Ok(());
        }

        // Sonde de vie (point fort repris de ViteMCP) : `GET /health` →
        // `200` « ok » sans JSON-RPC, compatible keep-alive — pour les
        // probes k8s/systemd et les équilibreurs de charge. `HEAD` donne
        // les mêmes en-têtes sans le corps (RFC 9110 §9.3.2).
        if health_probe {
            if is_get || is_head {
                // **Aucun corps n'est admis sur la sonde.** Consommer un
                // corps annoncé obligerait à attendre les octets (jusqu'à la
                // deadline de 60 s) : un client qui annonce
                // `Content-Length: 1048576` sans jamais envoyer le corps
                // retiendrait un slot, et `Expect: 100-continue` (que la
                // sonde ne traite pas) provoquerait un blocage croisé.
                // Refuser est aussi la seule façon d'être sûr qu'aucun
                // reliquat ne devienne la requête suivante (désync de file).
                if len != 0 || content_encoding_seen {
                    respond_error(
                        &mut stream,
                        "400 Bad Request",
                        "body not allowed on the health probe",
                        "",
                        is_head,
                    )?;
                    return Ok(());
                }
                respond(
                    &mut stream,
                    "200 OK",
                    "",
                    "text/plain",
                    b"ok",
                    !close,
                    is_head,
                )?;
                if close {
                    break;
                }
                continue;
            }
            respond_error(
                &mut stream,
                "405 Method Not Allowed",
                "GET or HEAD only",
                "Allow: GET, HEAD\r\n",
                is_head,
            )?;
            return Ok(());
        }

        if !route_matched {
            respond_error(&mut stream, "404 Not Found", "unknown route", "", is_head)?;
            return Ok(());
        }

        if !is_post {
            respond_error(
                &mut stream,
                "405 Method Not Allowed",
                "POST only",
                "Allow: POST\r\n",
                is_head,
            )?;
            return Ok(());
        }

        // `Content-Encoding` : aucune compression de requête n'est supportée.
        // Ignorer `Content-Encoding: gzip` permettrait de traverser un WAF
        // en envoyant du JSON brut sous une annonce de compression (RFC 9110
        // §15.5.16 → 415).
        if content_encoding_seen {
            respond_error(
                &mut stream,
                "415 Unsupported Media Type",
                "Content-Encoding is not supported",
                "",
                is_head,
            )?;
            return Ok(());
        }

        // `Content-Type` : la spec MCP exige `application/json` sur les POST
        // Streamable HTTP. Un `text/plain` ou un `charset` non-UTF-8 serait
        // traité comme du JSON UTF-8 (source de confusion de type). Absent →
        // accepté (certains clients legacy n'envoient pas l'en-tête).
        if content_type_seen && !content_type_json {
            respond_error(
                &mut stream,
                "415 Unsupported Media Type",
                "expected Content-Type: application/json",
                "",
                is_head,
            )?;
            return Ok(());
        }

        // `Expect: 100-continue` : le client HTTP/1.1 attend notre feu vert
        // avant d'envoyer le corps — on l'envoie, puis on lit le corps.
        // RFC 9110 §10.1.1 : jamais de `100 Continue` vers un client HTTP/1.0
        // (qui lirait l'intermédiaire 1xx comme la réponse finale → désync).
        if expect_continue && !http10 && len > 0 {
            stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
            stream.flush()?;
        }

        // Lecture du corps **par tranches** dans le buffer `body` réutilisé :
        // aucune pré-allocation de `len` octets au-delà de `BODY_CHUNK` et
        // zéro copie intermédiaire depuis `BufReader`. Un client qui annonce
        // `Content-Length: 1048576` sans envoyer de corps ne coûte plus 1 Mio
        // de commit — l'amplification mémoire passe de ×9 800 à ×1.
        if read_body(&mut reader, &mut body, len, read_timeout, deadline).is_err() {
            // Corps incomplet à mi-chemin : on tente une réponse 400
            // puis on ferme — aucun octet résiduel ne doit être
            // réinterprété comme une requête suivante.
            let _ = respond_error(
                &mut stream,
                "400 Bad Request",
                "truncated or slow request body",
                "",
                false,
            );
            return Ok(());
        }

        let dispatch_result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            // En-têtes de routage MCP (SEP-2243) — optionnels, mais
            // vérifiés s'ils sont présents.
            if mcp_method.is_none() && mcp_name.is_none() {
                core.dispatch_with_headers(&body, None)
            } else {
                let headers = crate::server::McpHeaders {
                    method: mcp_method,
                    name: mcp_name,
                };
                core.dispatch_with_headers(&body, Some(&headers))
            }
        }));

        let reply = match dispatch_result {
            Ok(Some(reply)) => reply,
            Ok(None) => {
                // Notification : aucune réponse JSON-RPC attendue.
                respond(
                    &mut stream,
                    "202 Accepted",
                    "",
                    "application/json",
                    b"",
                    !close,
                    false,
                )?;
                if close {
                    break;
                }
                continue;
            }
            Err(_) => {
                // Panique du handler (code utilisateur) : réponse propre,
                // connexion fermée par prudence.
                //
                // **L'`id` de la requête est repris** : sans lui, le client
                // ne peut pas corréler la réponse d'erreur et expire en
                // timeout au lieu de lire le diagnostic (JSON-RPC 2.0 §5).
                // L'extraction n'a lieu QUE sur ce chemin d'erreur — le
                // chemin nominal ne parse pas le JSON une seconde fois.
                let request_id = extract_request_id(&body);
                let payload = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "error": { "code": -32603, "message": "internal error" }
                });
                respond(
                    &mut stream,
                    "200 OK",
                    "",
                    "application/json",
                    payload.to_string().as_bytes(),
                    false,
                    false,
                )?;
                shutdown_gracefully(&mut stream);
                return Ok(());
            }
        };

        // SSE request-scoped : le client ne veut qu'un flux d'événements.
        //
        // Choix architectural assumé : **pas de SSE longue durée** (progress,
        // notifications serveur→client). Le mode stateless ne tient aucun
        // flux ouvert — un POST, un événement, clôture. C'est ce qui rend le
        // transport immunisé contre les proxies de VPS qui tamponnent les
        // flux en cours (empilement jusqu'au seuil de leur buffer) : une
        // réponse terminée, avec `Connection: close` et un `Content-Length`
        // explicite, est toujours poussée par le proxy.
        //
        // Contrat de transport pour tout futur écrivain SSE : flusher après
        // CHAQUE événement (`respond` fait `write_all` + `flush`) et porter
        // [`SSE_ANTI_BUFFERING`].
        if accept_sse && !accept_json {
            let event = format_sse_event(&reply);
            respond(
                &mut stream,
                "200 OK",
                SSE_ANTI_BUFFERING,
                "text/event-stream",
                &event,
                false,
                false,
            )?;
            shutdown_gracefully(&mut stream);
            break; // un seul événement de réponse, puis clôture
        }

        respond(
            &mut stream,
            "200 OK",
            "",
            "application/json",
            &reply,
            !close,
            false,
        )?;
        if close {
            let _ = stream.shutdown(std::net::Shutdown::Write);
            break;
        }
    }
    Ok(())
}

/// Réponse d'erreur : la connexion est **toujours fermée** — `Connection:
/// close` explicite + `shutdown(Write)`, jamais réutilisée. On ne draine pas
/// le corps restant : le drain nourrirait un client qui inonde (le 413 ferme
/// net), au prix possible d'un RST côté client avant lecture de la réponse —
/// dans les deux cas, aucun octet résiduel ne peut être réinterprété comme
/// une requête suivante.
fn respond_error(
    stream: &mut TcpStream,
    status: &str,
    message: &str,
    extra_headers: &str,
    head: bool,
) -> std::io::Result<()> {
    let payload: &[u8] = &error_payload(message);
    respond(
        stream,
        status,
        extra_headers,
        "application/json",
        payload,
        false,
        head,
    )?;
    // Fermeture propre systématique : on envoie un FIN **et** on vide le
    // tampon de réception avant le close — sinon le noyau émet un RST avec
    // des octets non lus, RST qui purge chez le client la réponse qu'on
    // vient d'écrire (404/415/431 perdus quand la requête n'a pas été
    // intégralement consommée).
    shutdown_gracefully(stream);
    Ok(())
}

fn error_payload(message: &str) -> Vec<u8> {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": null,
        "error": { "code": -32600, "message": message }
    })
    .to_string()
    .into_bytes()
}

/// Lit un corps de `len` octets **par tranches** dans `body`.
///
/// La capacité initiale est bornée à [`BODY_CHUNK`] (64 Kio) et `body` est
/// réutilisé sur la connexion keep-alive : la mémoire engagée par requête
/// reste proportionnelle aux octets **effectivement reçus**, pas au
/// `Content-Length` annoncé. Un client qui annonce 1 Mio et n'envoie rien ne
/// consomme donc plus 1 Mio de commit (amplification ×9 800 → ×1).
fn read_body(
    reader: &mut BufReader<TcpStream>,
    body: &mut Vec<u8>,
    len: usize,
    read_timeout: Duration,
    deadline: Instant,
) -> std::io::Result<()> {
    body.clear();
    if body.capacity() > BODY_CHUNK && len <= BODY_CHUNK {
        body.shrink_to(BODY_CHUNK);
    }
    if len == 0 {
        return Ok(());
    }
    let initial = len.min(BODY_CHUNK);
    if body.capacity() < initial {
        body.reserve(initial - body.capacity());
    }
    let mut remaining = len;
    while remaining > 0 {
        if tighten_read_timeout(reader.get_ref(), read_timeout, deadline).is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "deadline exceeded",
            ));
        }
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "truncated body",
            ));
        }
        let take = remaining.min(available.len());
        body.extend_from_slice(&available[..take]);
        reader.consume(take);
        remaining -= take;
    }
    Ok(())
}

fn respond(
    stream: &mut impl Write,
    status: &str,
    extra_headers: &str,
    content_type: &str,
    body: &[u8],
    keep_alive: bool,
    head: bool,
) -> std::io::Result<()> {
    let connection = if keep_alive { "keep-alive" } else { "close" };
    // En-tête et corps assemblés dans un seul buffer → **un seul write** :
    // avec nodelay, tête et corps partent dans le même segment TCP (deux
    // syscalls et un paquet de gagnés par réponse).
    let mut buf = Vec::with_capacity(
        96 + status.len() + extra_headers.len() + content_type.len() + body.len(),
    );
    write!(
        buf,
        "HTTP/1.1 {status}\r\n{extra_headers}Content-Type: {content_type}\r\nContent-Length: {}\r\nConnection: {connection}\r\n\r\n",
        body.len()
    )?;
    // Réponse à HEAD : en-têtes du GET équivalent, **sans corps** (RFC 9110
    // §9.3.2). Un corps sur une réponse HEAD désynchroniserait un client
    // keep-alive, qui lirait ces octets comme la réponse suivante.
    if !head {
        buf.extend_from_slice(body);
    }
    stream.write_all(&buf)?;
    stream.flush()
}

/// Formate une charge utile en événement SSE (`data: ...\n\n`) en
/// neutralisant tout `\r` ou `\n` éventuel (défense en profondeur contre
/// l'injection de champs SSE `event:` / `id:` / `retry:`).
fn format_sse_event(reply: &[u8]) -> Vec<u8> {
    let mut event = Vec::with_capacity(reply.len() + 8);
    for (idx, line) in reply.split(|&b| b == b'\n').enumerate() {
        if idx > 0 {
            event.push(b'\n');
        }
        event.extend_from_slice(b"data: ");
        for &byte in line {
            if byte != b'\r' {
                event.push(byte);
            }
        }
    }
    event.extend_from_slice(b"\n\n");
    event
}

/// Extrait l'`id` JSON-RPC d'un corps de requête — *best-effort*.
///
/// Utilisé pour reprendre l'`id` dans la réponse d'erreur de panique : sans
/// lui, le client ne peut pas corréler la réponse (JSON-RPC 2.0 §5).
/// Un corps illisible ou un `id` de type interdit (booléen, tableau, objet,
/// flottant) → `null`.
fn extract_request_id(body: &[u8]) -> serde_json::Value {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.get("id").cloned())
        .filter(|id| match id {
            serde_json::Value::String(_) => true,
            serde_json::Value::Number(n) => n.is_i64() || n.is_u64(),
            _ => false,
        })
        .unwrap_or(serde_json::Value::Null)
}

/// Caractère de jeton HTTP (`tchar`, RFC 9110 §5.6.2).
fn is_tchar(byte: u8) -> bool {
    matches!(
        byte,
        b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.'
            | b'^' | b'_' | b'`' | b'|' | b'~' | b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z'
    )
}

/// Vrai si `s` est un jeton HTTP valide (`1*tchar`, RFC 9110 §5.6.2).
fn is_valid_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(is_tchar)
}

/// Vrai si `target` est une cible de requête `origin-form` valide
/// (`absolute-path [ "?" query ]`, RFC 9112 §3.2.1), en ASCII visible et
/// sans fragment `#`.
fn is_valid_request_target(target: &str) -> bool {
    target.starts_with('/')
        && target
            .bytes()
            .all(|b| (0x21..=0x7e).contains(&b) && b != b'#')
}

/// Retire uniquement l'OWS HTTP (`SP` 0x20 et `HTAB` 0x09 — RFC 9110 §5.6.3).
///
/// Contrairement à `str::trim()`, ne retire ni `\x0b` (VT), ni `\x0c` (FF),
/// ni `\r`/`\n`, ni les espaces Unicode (`\u{00a0}`, etc.).
fn trim_ows(s: &str) -> &str {
    s.trim_matches([' ', '\t'])
}

/// Vrai si la valeur d'en-tête respecte RFC 9110 §5.5 (aucun caractère de
/// contrôle C0/C1 ni DEL `0x7F`, seul `HTAB` est admis avec les octets
/// visibles).
fn is_valid_field_value(s: &str) -> bool {
    !s.bytes().any(|b| (b < 0x20 && b != b'\t') || b == 0x7f)
        && !s.chars().any(|c| c != '\t' && c.is_control())
}

/// Vrai si l'en-tête `Host` est syntaxiquement valide (`uri-host [ ":" port ]`,
/// RFC 9112 §3.2 / RFC 3986 §3.2.2) : non vide, sans userinfo (`@`), sans
/// chemin/requête/fragment (`/`, `\`, `?`, `#`), sans espace, et avec un
/// port `u16` valide s'il est présent.
fn is_valid_host(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    let port_opt = if let Some(rest) = value.strip_prefix('[') {
        let Some((ipv6, after)) = rest.split_once(']') else {
            return false;
        };
        if ipv6.is_empty()
            || !ipv6
                .bytes()
                .all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.')
        {
            return false;
        }
        if after.is_empty() {
            None
        } else if let Some(port) = after.strip_prefix(':') {
            Some(port)
        } else {
            return false;
        }
    } else {
        match value.split_once(':') {
            Some((host, port)) => {
                if !is_valid_reg_name(host) {
                    return false;
                }
                Some(port)
            }
            None => {
                if !is_valid_reg_name(value) {
                    return false;
                }
                None
            }
        }
    };
    if let Some(port) = port_opt {
        if port.is_empty()
            || !port.bytes().all(|b| b.is_ascii_digit())
            || port.parse::<u16>().is_err()
        {
            return false;
        }
    }
    true
}

fn is_valid_reg_name(host: &str) -> bool {
    !host.is_empty()
        && host.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'-' | b'.'
                        | b'_'
                        | b'~'
                        | b'!'
                        | b'$'
                        | b'&'
                        | b'\''
                        | b'('
                        | b')'
                        | b'*'
                        | b'+'
                        | b','
                        | b';'
                        | b'='
                        | b'%'
                )
        })
}

/// Vrai si la valeur `Origin` a une syntaxe RFC 6454 valide (`"null"` ou
/// `scheme "://" host [ ":" port ]` sans chemin, query, fragment, userinfo
/// ni espace).
fn is_valid_origin_syntax(value: &str) -> bool {
    if value.is_empty() || value.bytes().any(|b| b <= b' ' || b >= 0x7f) {
        return false;
    }
    if value.eq_ignore_ascii_case("null") {
        return true;
    }
    let Some((scheme, authority)) = value.split_once("://") else {
        return false;
    };
    if scheme.is_empty()
        || !scheme.bytes().next().unwrap_or(b'0').is_ascii_alphabetic()
        || !scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
    {
        return false;
    }
    is_valid_host(authority)
}

/// Vrai si `Content-Type` désigne `application/json` (sans second type
/// séparé par une virgule, et avec `charset=utf-8` si le paramètre `charset`
/// est présent).
fn is_json_content_type(value: &str) -> bool {
    if value.contains(',') {
        return false;
    }
    let mut parts = value.split(';');
    let Some(mime) = parts.next() else {
        return false;
    };
    if !trim_ows(mime).eq_ignore_ascii_case("application/json") {
        return false;
    }
    for param in parts {
        let param = trim_ows(param);
        if param.is_empty() {
            return false;
        }
        let Some((k, v)) = param.split_once('=') else {
            return false;
        };
        let k = trim_ows(k);
        let v = trim_ows(v).trim_matches('"');
        if k.is_empty() || v.is_empty() || !is_valid_token(k) {
            return false;
        }
        if k.eq_ignore_ascii_case("charset") && !v.eq_ignore_ascii_case("utf-8") {
            return false;
        }
    }
    true
}

/// Vrai si l'en-tête `Expect` ne contient que le jeton `100-continue`
/// (RFC 9110 §10.1.1).
fn parse_expect_100_continue(value: &str) -> bool {
    let mut saw_100 = false;
    for part in value.split(',') {
        let token = trim_ows(part);
        if token.eq_ignore_ascii_case("100-continue") {
            saw_100 = true;
        } else {
            return false;
        }
    }
    saw_100
}

/// Vrai si l'en-tête contient le **jeton exact** `token`.
///
/// La valeur d'un en-tête est une liste de jetons séparés par des virgules
/// (RFC 9110 §5.6.1) : `Accept: text/event-stream-x` ne contient **pas**
/// le jeton `text/event-stream`. Une correspondance par sous-chaîne créait
/// une divergence de parsing avec un proxy strict (et pouvait faire basculer
/// la réponse en SSE à tort).
#[cfg(test)]
fn header_has_token(value: &str, token: &str) -> bool {
    value
        .split(',')
        .map(trim_ows)
        .any(|part| part.eq_ignore_ascii_case(token))
}

/// Vrai si l'en-tête contient le **type de media** `media_type` avec un
/// poids non nul (`q != 0`).
///
/// Comme [`header_has_token`], mais les paramètres du type de media sont
/// analysés : `application/json; charset=utf-8` contient bien le type
/// `application/json`, tandis que `text/event-stream;q=0` est refusé
/// (RFC 9110 §12.5.1).
fn header_has_media_type(value: &str, media_type: &str) -> bool {
    value.split(',').map(trim_ows).any(|part| {
        let mut segments = part.split(';');
        let Some(mime) = segments.next() else {
            return false;
        };
        if !trim_ows(mime).eq_ignore_ascii_case(media_type) {
            return false;
        }
        for param in segments {
            if let Some((k, v)) = trim_ows(param).split_once('=') {
                if trim_ows(k).eq_ignore_ascii_case("q") && is_zero_qvalue(trim_ows(v)) {
                    return false;
                }
            }
        }
        true
    })
}

fn is_zero_qvalue(v: &str) -> bool {
    matches!(v, "0" | "0." | "0.0" | "0.00" | "0.000")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tchar_and_token_validation_follows_rfc9110() {
        assert!(is_valid_token("GET"));
        assert!(is_valid_token("POST"));
        assert!(is_valid_token("Content-Length"));
        assert!(is_valid_token("X-Custom_Header!#$%&'*+-.^`|~123"));

        assert!(!is_valid_token(""));
        assert!(!is_valid_token("Host "));
        assert!(!is_valid_token(" Host"));
        assert!(!is_valid_token("Host\t"));
        assert!(!is_valid_token("Host\u{00a0}"));
        assert!(!is_valid_token("Host:Name"));
        assert!(!is_valid_token("Host/Name"));
        assert!(!is_valid_token("Host\x00Name"));
        assert!(!is_valid_token("Host\x7f"));
        assert!(!is_valid_token("Été"));
    }

    #[test]
    fn request_target_validation_enforces_origin_form_without_fragments() {
        assert!(is_valid_request_target("/mcp"));
        assert!(is_valid_request_target("/health"));
        assert!(is_valid_request_target("/mcp?x=1&y=2"));
        assert!(is_valid_request_target("/api/v1/mcp"));

        // Non origin-form, fragment `#`, espaces, contrôle ou non-ASCII : refusés.
        assert!(!is_valid_request_target(""));
        assert!(!is_valid_request_target("mcp"));
        assert!(!is_valid_request_target("http://localhost/mcp"));
        assert!(!is_valid_request_target("*"));
        assert!(!is_valid_request_target("/mcp#frag"));
        assert!(!is_valid_request_target("/mcp?x=1#frag"));
        assert!(!is_valid_request_target("/mcp with space"));
        assert!(!is_valid_request_target("/mcp\t"));
        assert!(!is_valid_request_target("/mcp\x00"));
        assert!(!is_valid_request_target("/mcp\x7f"));
        assert!(!is_valid_request_target("/mcp\u{00a0}"));
    }

    #[test]
    fn trim_ows_only_strips_sp_and_htab() {
        assert_eq!(trim_ows(" \tvalue \t"), "value");
        // VT (\x0b), FF (\x0c) et NBSP (\u{00a0}) ne sont PAS de l'OWS HTTP.
        assert_eq!(trim_ows("\x0b2"), "\x0b2");
        assert_eq!(trim_ows("\x0c2"), "\x0c2");
        assert_eq!(trim_ows("\u{00a0}2"), "\u{00a0}2");
    }

    #[test]
    fn field_value_rejects_control_characters() {
        assert!(is_valid_field_value("application/json; charset=utf-8"));
        assert!(is_valid_field_value("avec\ttabulation"));
        assert!(is_valid_field_value("UTF-8 autorisé : café"));

        // Contrôles C0 (hors HTAB), DEL (0x7F) et contrôles C1 Unicode refusés.
        assert!(!is_valid_field_value("nul\x00byte"));
        assert!(!is_valid_field_value("bell\x07"));
        assert!(!is_valid_field_value("vt\x0b"));
        assert!(!is_valid_field_value("ff\x0c"));
        assert!(!is_valid_field_value("cr\r"));
        assert!(!is_valid_field_value("lf\n"));
        assert!(!is_valid_field_value("esc\x1b"));
        assert!(!is_valid_field_value("del\x7f"));
        assert!(!is_valid_field_value("nel\u{0085}"));
    }

    #[test]
    fn host_header_validation_covers_ipv4_ipv6_regname_and_ports() {
        assert!(is_valid_host("localhost"));
        assert!(is_valid_host("localhost:8080"));
        assert!(is_valid_host("127.0.0.1"));
        assert!(is_valid_host("127.0.0.1:65535"));
        assert!(is_valid_host("[::1]"));
        assert!(is_valid_host("[::1]:8080"));
        assert!(is_valid_host("[2001:db8::1]:443"));
        assert!(is_valid_host("api.example.com"));

        // Valeurs malformées ou hostiles (RFC 9112 §3.2 / RFC 9110 §7.2).
        assert!(!is_valid_host(""));
        assert!(!is_valid_host("user@example.com"));
        assert!(!is_valid_host("example.com/path"));
        assert!(!is_valid_host("example.com?query"));
        assert!(!is_valid_host("example.com#frag"));
        assert!(!is_valid_host("example.com\\path"));
        assert!(!is_valid_host("example .com"));
        assert!(!is_valid_host("example.com:"));
        assert!(!is_valid_host("example.com:abc"));
        assert!(!is_valid_host("example.com:65536"));
        assert!(!is_valid_host("example.com:99999"));
        assert!(!is_valid_host("example.com:80:80"));
        assert!(!is_valid_host("[::1"));
        assert!(!is_valid_host("[]"));
        assert!(!is_valid_host("[::1]:99999"));
        assert!(!is_valid_host("[::1]extra"));
    }

    #[test]
    fn origin_syntax_validation_rejects_malformed_origins() {
        assert!(is_valid_origin_syntax("https://trusted.example"));
        assert!(is_valid_origin_syntax("http://localhost:3000"));
        assert!(is_valid_origin_syntax("http://[::1]:8080"));
        assert!(is_valid_origin_syntax("null"));

        assert!(!is_valid_origin_syntax(""));
        assert!(!is_valid_origin_syntax("*"));
        assert!(!is_valid_origin_syntax("https://trusted.example/"));
        assert!(!is_valid_origin_syntax("https://trusted.example/path"));
        assert!(!is_valid_origin_syntax("https://user@trusted.example"));
        assert!(!is_valid_origin_syntax(
            "https://a.example https://b.example"
        ));
        assert!(!is_valid_origin_syntax("://no-scheme.example"));
        assert!(!is_valid_origin_syntax("1http://bad-scheme.example"));
        assert!(!is_valid_origin_syntax("https://"));
    }

    #[test]
    fn content_type_validation_enforces_json_and_utf8_charset() {
        assert!(is_json_content_type("application/json"));
        assert!(is_json_content_type("APPLICATION/JSON"));
        assert!(is_json_content_type("application/json; charset=utf-8"));
        assert!(is_json_content_type("application/json; charset=\"UTF-8\""));
        assert!(is_json_content_type("application/json; boundary=something"));

        assert!(!is_json_content_type(""));
        assert!(!is_json_content_type("text/plain"));
        assert!(!is_json_content_type("application/json-patch+json"));
        assert!(!is_json_content_type("application/json, text/plain"));
        assert!(!is_json_content_type("application/json; charset=utf-16"));
        assert!(!is_json_content_type(
            "application/json; charset=iso-8859-1"
        ));
        assert!(!is_json_content_type("application/json;"));
        assert!(!is_json_content_type("application/json; invalid_param"));
        assert!(!is_json_content_type("application/json; charset="));
    }

    #[test]
    fn expect_validation_only_accepts_100_continue() {
        assert!(parse_expect_100_continue("100-continue"));
        assert!(parse_expect_100_continue("100-Continue"));
        assert!(parse_expect_100_continue("100-continue, 100-continue"));

        assert!(!parse_expect_100_continue(""));
        assert!(!parse_expect_100_continue("not-100-continued"));
        assert!(!parse_expect_100_continue("100-continue, custom-expect"));
    }

    #[test]
    fn accept_media_type_honors_q_zero_exclusion() {
        assert!(header_has_media_type(
            "text/event-stream",
            "text/event-stream"
        ));
        assert!(header_has_media_type(
            "text/event-stream; charset=utf-8; q=0.8",
            "text/event-stream"
        ));
        assert!(!header_has_media_type(
            "text/event-stream;q=0",
            "text/event-stream"
        ));
        assert!(!header_has_media_type(
            "text/event-stream; q=0.000",
            "text/event-stream"
        ));
        assert!(!header_has_media_type(
            "text/event-stream-x",
            "text/event-stream"
        ));
        assert!(header_has_token("100-continue", "100-continue"));
        assert!(!header_has_token("not-100-continued", "100-continue"));
    }

    #[test]
    fn format_sse_event_neutralizes_embedded_newlines() {
        let normal = format_sse_event(br#"{"jsonrpc":"2.0","id":1,"result":{}}"#);
        assert_eq!(
            normal,
            b"data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n"
        );

        // Défense en profondeur : si un payload contenait des `\r\n` bruts,
        // chaque ligne est préfixée par `data: ` et aucun champ `event:` ne
        // peut être injecté.
        let injected = format_sse_event(b"line1\r\nevent: pwned\ndata: fake");
        assert_eq!(
            injected,
            b"data: line1\ndata: event: pwned\ndata: data: fake\n\n"
        );
    }

    #[test]
    fn extract_request_id_filters_out_invalid_id_types() {
        assert_eq!(
            extract_request_id(br#"{"jsonrpc":"2.0","id":42}"#),
            serde_json::json!(42)
        );
        assert_eq!(
            extract_request_id(br#"{"jsonrpc":"2.0","id":"req-1"}"#),
            serde_json::json!("req-1")
        );
        // Booléen, tableau, objet ou flottant -> Null.
        assert_eq!(
            extract_request_id(br#"{"jsonrpc":"2.0","id":true}"#),
            serde_json::Value::Null
        );
        assert_eq!(
            extract_request_id(br#"{"jsonrpc":"2.0","id":[1]}"#),
            serde_json::Value::Null
        );
        assert_eq!(
            extract_request_id(br#"{"jsonrpc":"2.0","id":{"a":1}}"#),
            serde_json::Value::Null
        );
        assert_eq!(
            extract_request_id(br#"{"jsonrpc":"2.0","id":1.5}"#),
            serde_json::Value::Null
        );
        assert_eq!(extract_request_id(b"not json"), serde_json::Value::Null);
    }

    #[test]
    fn respond_writes_exact_framing_for_get_and_head() {
        let mut get_buf = Vec::new();
        respond(&mut get_buf, "200 OK", "", "text/plain", b"ok", true, false).unwrap();
        assert_eq!(
            get_buf,
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nok"
        );

        let mut head_buf = Vec::new();
        respond(
            &mut head_buf,
            "200 OK",
            "",
            "text/plain",
            b"ok",
            false,
            true,
        )
        .unwrap();
        assert_eq!(
            head_buf,
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\n"
        );
    }

    fn connected_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        server
            .set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        (client, server)
    }

    #[test]
    fn read_line_capped_enforces_cap_crlf_utf8_and_deadline() {
        // 1. Ligne CRLF valide dans la borne.
        let (mut client, server) = connected_pair();
        client.write_all(b"POST /mcp HTTP/1.1\r\n").unwrap();
        let mut reader = BufReader::new(server);
        let mut out = String::new();
        let mut scratch = Vec::new();
        let res = read_line_capped(
            &mut reader,
            &mut out,
            &mut scratch,
            64,
            Duration::from_millis(300),
            Instant::now() + Duration::from_secs(2),
        );
        assert!(res.is_ok());
        assert_eq!(out, "POST /mcp HTTP/1.1\r\n");

        // 2. Dépassement de `cap` -> TooLong.
        client.write_all(b"X-Long: 0123456789abcdef\r\n").unwrap();
        let res = read_line_capped(
            &mut reader,
            &mut out,
            &mut scratch,
            10,
            Duration::from_millis(300),
            Instant::now() + Duration::from_secs(2),
        );
        assert!(matches!(res, Err(LineFail::TooLong)));

        // 3. LF nu sans CR -> Closed.
        let (mut client, server) = connected_pair();
        client.write_all(b"GET /health HTTP/1.1\n").unwrap();
        let mut reader = BufReader::new(server);
        let res = read_line_capped(
            &mut reader,
            &mut out,
            &mut scratch,
            64,
            Duration::from_millis(300),
            Instant::now() + Duration::from_secs(2),
        );
        assert!(matches!(res, Err(LineFail::Closed)));

        // 4. UTF-8 invalide -> Closed.
        let (mut client, server) = connected_pair();
        client.write_all(b"GET /\xff\xfe HTTP/1.1\r\n").unwrap();
        let mut reader = BufReader::new(server);
        let res = read_line_capped(
            &mut reader,
            &mut out,
            &mut scratch,
            64,
            Duration::from_millis(300),
            Instant::now() + Duration::from_secs(2),
        );
        assert!(matches!(res, Err(LineFail::Closed)));

        // 5. Deadline dépassée -> Slow.
        let (_client, server) = connected_pair();
        let mut reader = BufReader::new(server);
        let res = read_line_capped(
            &mut reader,
            &mut out,
            &mut scratch,
            64,
            Duration::from_secs(10),
            Instant::now() + Duration::from_millis(40),
        );
        assert!(matches!(res, Err(LineFail::Slow)));
    }

    #[test]
    fn read_body_reads_chunks_shrinks_capacity_and_rejects_truncation() {
        let (mut client, server) = connected_pair();
        let payload = vec![b'A'; BODY_CHUNK + 1024];
        std::thread::spawn(move || {
            client.write_all(&payload).unwrap();
            client.write_all(b"short").unwrap();
            // Tronqué pour le 3e appel (annonce 50 octets, n'envoie que 2).
            client.write_all(b"xy").unwrap();
        });

        let mut reader = BufReader::new(server);
        let mut body = Vec::new();
        read_body(
            &mut reader,
            &mut body,
            BODY_CHUNK + 1024,
            Duration::from_millis(300),
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(body.len(), BODY_CHUNK + 1024);
        assert!(body.capacity() >= BODY_CHUNK + 1024);

        // Appel suivant avec petit corps : la capacité excédentaire est réduite à BODY_CHUNK.
        read_body(
            &mut reader,
            &mut body,
            5,
            Duration::from_millis(300),
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(&body, b"short");
        assert!(body.capacity() <= BODY_CHUNK);

        // Corps tronqué (EOF avant `len`) -> UnexpectedEof.
        let err = read_body(
            &mut reader,
            &mut body,
            50,
            Duration::from_millis(300),
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }
}
