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
    /// Refus en cours d'écriture (503) — borné, pour que la boucle d'`accept`
    /// ne soit jamais bloquée par les refus.
    pub(crate) refusals_in_flight: Arc<AtomicUsize>,
}

impl BoundServer {
    /// Adresse locale effective — utile après un bind sur le port 0.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Bloque et sert les connexions — un thread par connexion, dans la
    /// limite de `max_connections` (défaut [`MAX_CONNECTIONS`]).
    pub fn serve(self) -> std::io::Result<()> {
        for stream in self.listener.incoming() {
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
                                        br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":"trop de connexions"}}"#,
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
                    br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":"moteur sature"}}"#,
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
            // Clone pour le chemin d'erreur ci-dessous : la closure appartient
            // au spawn — si la création du thread échoue, elle est abandonnée
            // avec le flux qu'elle capture, et la réponse 503 part sur le flux
            // d'origine, resté la propriété de la boucle.
            let Ok(spawn_stream) = stream.try_clone() else {
                self.live_connections.fetch_sub(1, Ordering::Relaxed);
                continue;
            };
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
                        self.connection_ttl,
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
                    br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":"serveur sature"}}"#,
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
    deadline: Instant,
) -> Result<(), LineFail> {
    out.clear();
    scratch.clear();
    loop {
        if Instant::now() >= deadline {
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
                    // CRLF). Un LF seul (« httpoxy ») ou un double CR
                    // (`\r\r\n`) seraient lus différemment par un proxy
                    // strict → divergence de parsing.
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
    connection_ttl: Duration,
) -> std::io::Result<()> {
    // Buffer de lecture 4 Ko (et non les 8 Ko par défaut) : les en-têtes
    // tiennent largement dedans, et un corps plus grand que la capacité est
    // lu directement depuis la socket — moitié moins de mémoire par
    // connexion (1 Mio au plafond de 256 connexions).
    let mut reader = BufReader::with_capacity(4 * 1024, stream.try_clone()?);
    let mut stream = stream;
    // Buffers réutilisés sur toute la connexion (keep-alive inclus) : le
    // chemin chaud n'alloue que le corps, l'en-tête de réponse et la
    // réplique JSON-RPC.
    let mut line = String::new();
    let mut scratch = Vec::new();
    // Instant d'ouverture de la connexion : borne sa durée de vie totale.
    let connection_started = Instant::now();
    loop {
        // TTL de connexion : au-delà, la connexion est fermée même si le
        // client est actif. Sans cette borne, un client qui drip-feed des
        // requêtes valides (sous le read_timeout) retient son slot de
        // connexion **indéfiniment** → 256 clients lents suffisent à
        // saturer le plafond pour tout le monde.
        //
        // TTL nul = pas de TTL (désactivé), comme documenté sur
        // [`crate::server::ServerBuilder::connection_ttl`].
        if !connection_ttl.is_zero() && connection_started.elapsed() >= connection_ttl {
            break;
        }
        let deadline = Instant::now() + REQUEST_DEADLINE;

        // Ligne de requête — bornée, deadline globale.
        match read_line_capped(&mut reader, &mut line, &mut scratch, MAX_LINE, deadline) {
            Ok(()) => {}
            Err(LineFail::TooLong) => {
                respond(
                    &mut stream,
                    "431 Request Header Fields Too Large",
                    "",
                    "application/json",
                    br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"ligne trop longue"}}"#,
                    false,
                    false,
                )?;
                return Ok(());
            }
            // slowloris (timeout OU deadline globale) ou coupure : sans réponse
            Err(_) => return Ok(()),
        }
        // RFC 9112 §3 : la ligne de requête n'accepte que des espaces ASCII
        // (SP) comme séparateurs. `split_ascii_whitespace` inclurait aussi
        // `\t`, `\n`, `\r`, `\x0b`, `\x0c` — un proxy strict (RFC 9112 §3
        // n'autorise que SP) lirait la requête différemment → divergence de
        // parsing. On découpe donc explicitement sur SP.
        //
        // Le CRLF final est retiré **exactement une fois** avant le
        // découpage (sinon il resterait collé au dernier jeton :
        // `HTTP/1.1\r\n`). Un CR ou LF résiduel *à l'intérieur* de la ligne
        // est refusé : `GET /health HTTP/1.1\r\r\n` est lu différemment par
        // un proxy strict (RFC 9112 §2.2 — fin de ligne = CRLF unique).
        let request_line = line.strip_suffix("\r\n").unwrap_or(&line);
        if request_line.contains(['\r', '\n']) {
            respond_error(
                &mut stream,
                "400 Bad Request",
                "fin de ligne invalide dans la ligne de requête",
                "",
                false,
            )?;
            return Ok(());
        }
        let mut parts = request_line.split(' ').filter(|part| !part.is_empty());
        let verb = parts.next().unwrap_or("");
        let full_path = parts.next().unwrap_or("");
        let version = parts.next().unwrap_or("");
        // Ligne de requête stricte : exactement trois jetons (méthode, cible,
        // version). RFC 9112 §3.
        if verb.is_empty() || full_path.is_empty() || version.is_empty() || parts.next().is_some() {
            respond_error(
                &mut stream,
                "400 Bad Request",
                "ligne de requête invalide",
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
                "caractère de contrôle dans la ligne de requête",
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
                "version HTTP non supportée",
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
        // Origin/Host sans allocation : seul le verdict est conservé, jamais
        // la valeur — le garde ne compare qu'avec la liste autorisée.
        let mut origin_seen = false;
        let mut origin_duplicate = false;
        let mut origin_allowed = false;
        let mut host_seen = false;
        let mut host_duplicate = false;
        let mut accept_json = false;
        let mut accept_sse = false;
        let mut expect_continue = false;
        // `Content-Type` de la requête — vérifié sur les routes POST.
        let mut content_type: Option<String> = None;
        // En-têtes de routage MCP 2026-07-28 (SEP-2243) — optionnels chez
        // nous (compatibilité), mais vérifiés s'ils sont présents.
        let mut mcp_method: Option<String> = None;
        let mut mcp_name: Option<String> = None;
        // `Connection: close` a-t-il été vu ? (HTTP/1.0 : la connexion est
        // fermée par défaut, `keep-alive` doit pouvoir la maintenir ouverte,
        // mais `close` reste ABSORBANT s'il apparaît).
        let mut connection_close_seen = false;
        let mut close = http10; // HTTP/1.0 : fermeture par défaut
        let mut header_count = 0usize;
        loop {
            match read_line_capped(&mut reader, &mut line, &mut scratch, MAX_LINE, deadline) {
                Ok(()) => {}
                Err(LineFail::TooLong) => {
                    respond(
                        &mut stream,
                        "431 Request Header Fields Too Large",
                        "",
                        "application/json",
                        r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"ligne trop longue"}}"#.as_bytes(),
                        false,
                        is_head,
                    )?;
                    return Ok(());
                }
                Err(_) => return Ok(()),
            }
            let header = line.trim_end();
            if header.is_empty() {
                break;
            }
            // obs-fold (RFC 9112 §5.2, obsolète) : une ligne de continuation
            // serait susceptible d'être réassemblée différemment par un proxy
            // — refus net plutôt qu'acceptation ambiguë.
            if header.starts_with(' ') || header.starts_with('\t') {
                respond_error(
                    &mut stream,
                    "400 Bad Request",
                    "ligne d'en-tête obsolète (obs-fold)",
                    "",
                    is_head,
                )?;
                return Ok(());
            }
            header_count += 1;
            if header_count > MAX_HEADERS {
                respond(
                    &mut stream,
                    "431 Request Header Fields Too Large",
                    "",
                    "application/json",
                    r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"trop d'en-têtes"}}"#.as_bytes(),
                    false,
                    is_head,
                )?;
                return Ok(());
            }
            if let Some((name_raw, value)) = header.split_once(':') {
                // RFC 9112 §5.1 : aucun espace entre le nom de champ et le
                // deux-points — une telle ligne est lue autrement par un
                // proxy strict (désynchronisation potentielle), refus net.
                if name_raw.ends_with(' ') || name_raw.ends_with('\t') {
                    respond_error(
                        &mut stream,
                        "400 Bad Request",
                        "espace avant deux-points",
                        "",
                        is_head,
                    )?;
                    return Ok(());
                }
                // Comparaison insensible à la casse sans réallouer le nom ni
                // la valeur — le chemin d'en-têtes n'alloue rien.
                let name = name_raw.trim();
                let value = value.trim();
                if name.is_empty() {
                    respond_error(
                        &mut stream,
                        "400 Bad Request",
                        "nom d'en-tête vide",
                        "",
                        is_head,
                    )?;
                    return Ok(());
                }
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
                        origin_allowed =
                            allow_origins.iter().any(|o| o.eq_ignore_ascii_case(value));
                    }
                } else if name.eq_ignore_ascii_case("host") {
                    if host_seen {
                        host_duplicate = true;
                    } else {
                        host_seen = true;
                    }
                } else if name.eq_ignore_ascii_case("accept") {
                    // Correspondance **par jeton exact** : `Accept:
                    // text/event-stream-x` ne doit pas être pris pour
                    // `text/event-stream` (une correspondance par
                    // sous-chaîne créait une divergence avec un proxy
                    // strict et pouvait basculer la réponse en SSE).
                    //
                    // Les paramètres de media type sont ignorés :
                    // `application/json; charset=utf-8` contient bien le
                    // jeton `application/json` (RFC 9110 §12.5.1).
                    accept_json |= header_has_media_type(value, "application/json");
                    accept_sse |= header_has_media_type(value, "text/event-stream");
                } else if name.eq_ignore_ascii_case("content-type") {
                    content_type = Some(value.to_string());
                } else if name.eq_ignore_ascii_case("mcp-method") {
                    mcp_method = Some(value.to_string());
                } else if name.eq_ignore_ascii_case("mcp-name") {
                    mcp_name = Some(value.to_string());
                } else if name.eq_ignore_ascii_case("connection") {
                    let mut has_close = false;
                    let mut has_keep_alive = false;
                    for token in value.split(',') {
                        let token = token.trim();
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
                } else if name.eq_ignore_ascii_case("expect")
                    && header_has_token(value, "100-continue")
                {
                    expect_continue = true;
                }
            } else {
                // RFC 9112 §5 : une ligne d'en-tête sans deux-points est
                // malformée — l'ignorer reviendrait à l'accepter sous une
                // lecture différente de celle d'un proxy en amont.
                respond_error(
                    &mut stream,
                    "400 Bad Request",
                    "en-tête malformé",
                    "",
                    is_head,
                )?;
                return Ok(());
            }
        }

        // En-tete `Origin` duplique : requete ambigue (contournement possible
        // de la garde DNS-rebinding) - refus 400.
        if origin_duplicate {
            respond_error(
                &mut stream,
                "400 Bad Request",
                "en-tete Origin duplique",
                "",
                is_head,
            )?;
            return Ok(());
        }

        // Garde DNS-rebinding : un navigateur qui parle au serveur envoie un
        // `Origin` ; par défaut on refuse, sauf origines explicitement permises.
        if origin_seen && !origin_allowed {
            respond_error(&mut stream, "403 Forbidden", "origin refusée", "", is_head)?;
            return Ok(());
        }

        // RFC 9112 §3.2 : `Host` obligatoire — et unique — en HTTP/1.1.
        // HTTP/1.0 en est dispensé (le champ y est facultatif).
        if !http10 {
            if !host_seen {
                respond_error(
                    &mut stream,
                    "400 Bad Request",
                    "en-tête Host requis",
                    "",
                    is_head,
                )?;
                return Ok(());
            }
            if host_duplicate {
                respond_error(
                    &mut stream,
                    "400 Bad Request",
                    "en-tête Host dupliqué",
                    "",
                    is_head,
                )?;
                return Ok(());
            }
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
                "encodage de requête ambigu",
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
                    "Content-Length invalide",
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
                        "Content-Length requis",
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
                "corps trop volumineux",
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
                if len != 0 {
                    respond_error(
                        &mut stream,
                        "400 Bad Request",
                        "corps non admis sur la sonde de vie",
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
                "GET ou HEAD uniquement",
                "Allow: GET, HEAD\r\n",
                is_head,
            )?;
            return Ok(());
        }

        if !route_matched {
            respond_error(&mut stream, "404 Not Found", "route inconnue", "", is_head)?;
            return Ok(());
        }

        if !is_post {
            respond_error(
                &mut stream,
                "405 Method Not Allowed",
                "POST uniquement",
                "Allow: POST\r\n",
                is_head,
            )?;
            return Ok(());
        }

        // `Content-Type` : la spec MCP exige `application/json` sur les POST
        // Streamable HTTP. Un `text/plain` serait traité comme du JSON
        // (source de confusion de type). Absent → accepté (certains
        // clients legacy n'envoient pas l'en-tête).
        if let Some(value) = &content_type {
            let media_type = value
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if media_type != "application/json" {
                respond_error(
                    &mut stream,
                    "415 Unsupported Media Type",
                    "Content-Type attendu : application/json",
                    "",
                    is_head,
                )?;
                return Ok(());
            }
        }

        // `Expect: 100-continue` : le client attend notre feu vert avant
        // d'envoyer le corps — on l'envoie, puis on lit le corps.
        if expect_continue && len > 0 {
            stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
            stream.flush()?;
        }

        // Lecture du corps **par tranches** : un buffer de 64 Ko réutilisé
        // au lieu d'une pré-allocation de `len` octets. Un client qui
        // annonce `Content-Length: 1048576` sans envoyer de corps ne
        // coûte plus 1 Mio de commit — l'amplification mémoire passe de
        // ×9 800 à ×1.
        let body = match read_body(&mut reader, len, deadline) {
            Ok(body) => body,
            Err(_) => {
                // Corps incomplet à mi-chemin : on tente une réponse 400
                // puis on ferme — aucun octet résiduel ne doit être
                // réinterprété comme une requête suivante.
                let _ = respond_error(
                    &mut stream,
                    "400 Bad Request",
                    "corps tronque ou trop lent",
                    "",
                    false,
                );
                return Ok(());
            }
        };

        let dispatch_result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            // En-têtes de routage MCP (SEP-2243) — optionnels, mais
            // vérifiés s'ils sont présents.
            let headers = crate::server::McpHeaders {
                method: mcp_method.clone(),
                name: mcp_name.clone(),
            };
            core.dispatch_with_headers(&body, Some(&headers))
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
                    "error": { "code": -32603, "message": "erreur interne" }
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
            let mut event = Vec::with_capacity(reply.len() + 8);
            event.extend_from_slice(b"data: ");
            event.extend_from_slice(&reply);
            event.extend_from_slice(b"\n\n");
            respond(
                &mut stream,
                "200 OK",
                SSE_ANTI_BUFFERING,
                "text/event-stream",
                &event,
                false,
                false,
            )?;
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
    // vient d'écrire (404/415 perdus quand le corps n'a pas été consommé).
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

/// Lit un corps de `len` octets **par tranches**.
///
/// Le buffer de lecture est borné à [`BODY_CHUNK`] (64 Kio) et réutilisé :
/// la mémoire allouée par requête est proportionnelle aux octets
/// **effectivement reçus**, pas au `Content-Length` annoncé. Un client qui
/// annonce 1 Mio et n'envoie rien ne consomme donc plus 1 Mio de commit
/// (amplification ×9 800 → ×1).
///
/// Le résultat est concaténé dans un unique `Vec` de la taille exacte — le
/// dispatch JSON-RPC a besoin d'un slice contigu.
fn read_body(
    reader: &mut BufReader<TcpStream>,
    len: usize,
    deadline: Instant,
) -> std::io::Result<Vec<u8>> {
    if len == 0 {
        return Ok(Vec::new());
    }
    let mut body = Vec::with_capacity(len.min(BODY_CHUNK));
    let mut chunk = vec![0u8; BODY_CHUNK.min(len)];
    let mut remaining = len;
    while remaining > 0 {
        let wanted = remaining.min(chunk.len());
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "deadline dépassée",
            ));
        }
        let n = reader.read(&mut chunk[..wanted])?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "corps tronqué",
            ));
        }
        body.extend_from_slice(&chunk[..n]);
        remaining -= n;
    }
    Ok(body)
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

/// Extrait l'`id` JSON-RPC d'un corps de requête — *best-effort*.
///
/// Utilisé pour reprendre l'`id` dans la réponse d'erreur de panique : sans
/// lui, le client ne peut pas corréler la réponse (JSON-RPC 2.0 §5).
/// Un corps illisible ou sans `id` → `null`.
fn extract_request_id(body: &[u8]) -> serde_json::Value {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.get("id").cloned())
        .unwrap_or(serde_json::Value::Null)
}

/// Vrai si l'en-tête contient le **jeton exact** `token`.
///
/// La valeur d'un en-tête est une liste de jetons séparés par des virgules
/// (RFC 9110 §5.6.1) : `Accept: text/event-stream-x` ne contient **pas**
/// le jeton `text/event-stream`. Une correspondance par sous-chaîne créait
/// une divergence de parsing avec un proxy strict (et pouvait faire basculer
/// la réponse en SSE à tort).
fn header_has_token(value: &str, token: &str) -> bool {
    value
        .split(',')
        .map(|part| part.trim())
        .any(|part| part.eq_ignore_ascii_case(token))
}

/// Vrai si l'en-tête contient le **type de media** `media_type`.
///
/// Comme [`header_has_token`], mais les paramètres du type de media sont
/// ignorés : `application/json; charset=utf-8` contient bien le type
/// `application/json` (RFC 9110 §12.5.1 — *« media type parameters »*).
fn header_has_media_type(value: &str, media_type: &str) -> bool {
    value.split(',').map(|part| part.trim()).any(|part| {
        part.split(';')
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case(media_type)
    })
}
