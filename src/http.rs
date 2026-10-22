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
//! - plafond de connexions simultanées (`MAX_CONNECTIONS`, défaut 256, par
//!   serveur) : au-delà, la connexion est refusée immédiatement (`503`) —
//!   pas de spawn illimité ;
//! - limites `MAX_HEADERS` (64) / `MAX_LINE` (16 Kio) → `431`, `MAX_BODY`
//!   (1 Mio) → `413` ;
//! - `Transfer-Encoding` présent ou `Content-Length` dupliqué → `400`
//!   (pas de désynchronisation type request smuggling derrière un proxy) ;
//! - `Content-Length` malformé → `400` (RFC 9112 §6.3 ; le `411` est réservé
//!   à l'absence du champ) ;
//! - en-tête `Host` requis et unique en HTTP/1.1 (`400` sinon, RFC 9112
//!   §3.2), lignes obs-fold rejetées (`400`), ligne de requête malformée →
//!   `400`, version autre que HTTP/1.0/1.1 → `505` ;
//! - `Expect: 100-continue` honoré, et toute réponse d'erreur **ferme la
//!   connexion** (`Connection: close`) : aucun octet résiduel ne peut être
//!   réinterprété comme une requête suivante — on ne draine pas le corps
//!   restant (le drain nourrirait un client qui inonde), au prix possible
//!   d'un RST côté client avant lecture de la réponse (compromis assumé) ;
//! - `POST /mcp?query=…` accepté (la query est ignorée) ;
//! - `HTTP/1.0` sans `Connection: keep-alive` → connexion fermée ;
//! - panique d'un handler capturée (`catch_unwind`) → `-32603` JSON-RPC,
//!   la connexion du client reste propre. NB : compiler avec
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

/// Plafond de connexions simultanées **par serveur** — au-delà, refus
/// immédiat (pas de spawn). Défaut 256.
pub const MAX_CONNECTIONS: usize = 256;

/// Serveur HTTP attaché à un socket bindé
/// (résultat de [`crate::ServerBuilder::bind`]).
pub struct BoundServer {
    pub(crate) listener: TcpListener,
    pub(crate) core: Arc<ServerCore>,
    pub(crate) route: String,
    pub(crate) allow_origins: Vec<String>,
    pub(crate) read_timeout: Duration,
    pub(crate) max_connections: usize,
    /// Connexions vivantes de **cette** instance — le plafond est par
    /// serveur, pas global au processus.
    pub(crate) live_connections: Arc<AtomicUsize>,
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
            let Ok(stream) = stream else {
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
                let _ = respond(
                    &mut &stream,
                    "503 Service Unavailable",
                    "",
                    "application/json",
                    br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":"trop de connexions"}}"#,
                    false,
                );
                continue;
            }
            let core = Arc::clone(&self.core);
            let route = self.route.clone();
            let allow_origins = self.allow_origins.clone();
            let live = Arc::clone(&self.live_connections);
            std::thread::spawn(move || {
                struct LiveGuard(Arc<AtomicUsize>);
                impl Drop for LiveGuard {
                    fn drop(&mut self) {
                        self.0.fetch_sub(1, Ordering::Relaxed);
                    }
                }
                let _guard = LiveGuard(live);
                let _ = handle_connection(stream, &core, &route, &allow_origins);
            });
        }
        Ok(())
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
/// La ligne doit être du UTF-8 valide (sinon `Closed` → coupure propre) et
/// la `deadline` globale est contrôlée à chaque remplissage — un client qui
/// drible des octets est coupé aussi.
fn read_line_capped(
    reader: &mut BufReader<TcpStream>,
    out: &mut String,
    cap: usize,
    deadline: Instant,
) -> Result<(), LineFail> {
    out.clear();
    let mut bytes = Vec::new();
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
            if bytes.len() + chunk.len() > cap {
                return Err(LineFail::TooLong);
            }
            bytes.extend_from_slice(chunk);
            reader.consume(pos + 1);
            return match String::from_utf8(bytes) {
                Ok(text) => {
                    *out = text;
                    Ok(())
                }
                Err(_) => Err(LineFail::Closed), // non-UTF8 : coupure propre
            };
        }
        if bytes.len() + available.len() > cap {
            return Err(LineFail::TooLong);
        }
        bytes.extend_from_slice(available);
        let filled = available.len();
        reader.consume(filled);
    }
}

/// État du corps : à drainer (borné) ou connexion à fermer.
fn handle_connection(
    stream: TcpStream,
    core: &ServerCore,
    route: &str,
    allow_origins: &[String],
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut stream = stream;
    loop {
        let deadline = Instant::now() + REQUEST_DEADLINE;

        // Ligne de requête — bornée, deadline globale.
        let mut request_line = String::new();
        match read_line_capped(&mut reader, &mut request_line, MAX_LINE, deadline) {
            Ok(()) => {}
            Err(LineFail::TooLong) => {
                respond(
                    &mut stream,
                    "431 Request Header Fields Too Large",
                    "",
                    "application/json",
                    br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"ligne trop longue"}}"#,
                    false,
                )?;
                return Ok(());
            }
            // slowloris (timeout OU deadline globale) ou coupure : sans réponse
            Err(_) => return Ok(()),
        }
        let mut parts = request_line.split_whitespace();
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
            )?;
            return Ok(());
        }
        let http10 = version.eq_ignore_ascii_case("HTTP/1.0");
        if !http10 && !version.eq_ignore_ascii_case("HTTP/1.1") {
            respond_error(
                &mut stream,
                "505 HTTP Version Not Supported",
                "version HTTP non supportée",
                "",
            )?;
            return Ok(());
        }
        // La query string est ignorée : POST /mcp?x=1 == POST /mcp.
        let path = full_path.split('?').next().unwrap_or(full_path);

        let mut content_length: Option<usize> = None;
        let mut saw_content_length = false;
        let mut malformed_length = false;
        let mut duplicate_length = false;
        let mut transfer_encoding = false;
        let mut origin: Option<String> = None;
        let mut origin_duplicate = false;
        let mut host: Option<String> = None;
        let mut host_duplicate = false;
        let mut accept = String::new();
        let mut expect_continue = false;
        let mut close = http10; // HTTP/1.0 : fermeture par défaut
        let mut header_count = 0usize;
        loop {
            let mut header = String::new();
            match read_line_capped(&mut reader, &mut header, MAX_LINE, deadline) {
                Ok(()) => {}
                Err(LineFail::TooLong) => {
                    respond(
                        &mut stream,
                        "431 Request Header Fields Too Large",
                        "",
                        "application/json",
                        r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"ligne trop longue"}}"#.as_bytes(),
                        false,
                    )?;
                    return Ok(());
                }
                Err(_) => return Ok(()),
            }
            let header = header.trim_end();
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
                )?;
                return Ok(());
            }
            if let Some((name, value)) = header.split_once(':') {
                let name = name.trim().to_ascii_lowercase();
                let value = value.trim();
                match name.as_str() {
                    "content-length" => {
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
                    }
                    "transfer-encoding" => transfer_encoding = true, // TE : refusé
                    "origin" => {
                        if origin.is_some() {
                            origin_duplicate = true;
                        } else {
                            origin = Some(value.to_string());
                        }
                    }
                    "host" => {
                        if host.is_some() {
                            host_duplicate = true;
                        } else {
                            host = Some(value.to_string());
                        }
                    }
                    "accept" => {
                        let v = value.to_ascii_lowercase();
                        if accept.is_empty() {
                            accept = v;
                        } else {
                            accept.push_str(", ");
                            accept.push_str(&v);
                        }
                    }
                    "connection" => {
                        let lower = value.to_ascii_lowercase();
                        let tokens: Vec<&str> = lower.split(',').map(str::trim).collect();
                        if tokens.contains(&"close") {
                            close = true;
                        }
                        if http10 {
                            if tokens.contains(&"keep-alive") && !tokens.contains(&"close") {
                                close = false;
                            } else if !tokens.contains(&"keep-alive") {
                                close = true;
                            }
                        }
                    }
                    "expect" if value.to_ascii_lowercase().contains("100-continue") => {
                        expect_continue = true;
                    }
                    _ => {}
                }
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
            )?;
            return Ok(());
        }

        // Garde DNS-rebinding : un navigateur qui parle au serveur envoie un
        // `Origin` ; par défaut on refuse, sauf origines explicitement permises.
        if let Some(origin) = &origin {
            let allowed = allow_origins.iter().any(|o| o.eq_ignore_ascii_case(origin));
            if !allowed {
                respond_error(&mut stream, "403 Forbidden", "origin refusée", "")?;
                return Ok(());
            }
        }

        // RFC 9112 §3.2 : `Host` obligatoire — et unique — en HTTP/1.1.
        // HTTP/1.0 en est dispensé (le champ y est facultatif).
        if !http10 {
            if host.is_none() {
                respond_error(&mut stream, "400 Bad Request", "en-tête Host requis", "")?;
                return Ok(());
            }
            if host_duplicate {
                respond_error(&mut stream, "400 Bad Request", "en-tête Host dupliqué", "")?;
                return Ok(());
            }
        }

        if path != route {
            respond_error(&mut stream, "404 Not Found", "route inconnue", "")?;
            return Ok(());
        }

        if verb != "POST" {
            respond_error(
                &mut stream,
                "405 Method Not Allowed",
                "POST uniquement",
                "Allow: POST\r\n",
            )?;
            return Ok(());
        }

        // Anti request-smuggling : TE présent ou CL dupliqué = requête
        // ambigüe, refusée sans ambiguïté (RFC 9112 §6).
        if transfer_encoding || duplicate_length {
            respond_error(
                &mut stream,
                "400 Bad Request",
                "encodage de requête ambigu",
                "",
            )?;
            return Ok(());
        }

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
                )?;
                return Ok(());
            }
            None => {
                respond_error(
                    &mut stream,
                    "411 Length Required",
                    "Content-Length requis",
                    "",
                )?;
                return Ok(());
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
            )?;
            return Ok(());
        }

        // `Expect: 100-continue` : le client attend notre feu vert avant
        // d'envoyer le corps — on l'envoie, puis on lit le corps.
        if expect_continue && len > 0 {
            stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
            stream.flush()?;
        }

        let mut body = vec![0u8; len];
        if read_exact_deadline(&mut reader, &mut body, deadline).is_err() {
            // Corps incomplet a mi-chemin : on tente une reponse 400 puis on
            // ferme - aucun octet residuel ne doit etre reinterprete.
            let _ = respond_error(
                &mut stream,
                "400 Bad Request",
                "corps tronque ou trop lent",
                "",
            );
            return Ok(());
        }

        let dispatch_result = std::panic::catch_unwind(AssertUnwindSafe(|| core.dispatch(&body)));

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
                )?;
                if close {
                    break;
                }
                continue;
            }
            Err(_) => {
                // Panique du handler (code utilisateur) : réponse propre,
                // connexion fermée par prudence.
                let payload = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": { "code": -32603, "message": "erreur interne" }
                });
                respond(
                    &mut stream,
                    "200 OK",
                    "",
                    "application/json",
                    payload.to_string().as_bytes(),
                    false,
                )?;
                return Ok(());
            }
        };

        // SSE request-scoped : le client ne veut qu'un flux d'événements.
        let sse_only = accept.contains("text/event-stream") && !accept.contains("application/json");
        if sse_only {
            let mut event = Vec::with_capacity(reply.len() + 8);
            event.extend_from_slice(b"data: ");
            event.extend_from_slice(&reply);
            event.extend_from_slice(b"\n\n");
            respond(
                &mut stream,
                "200 OK",
                "",
                "text/event-stream",
                &event,
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
) -> std::io::Result<()> {
    let payload: &[u8] = &error_payload(message);
    respond(
        stream,
        status,
        extra_headers,
        "application/json",
        payload,
        false,
    )?;
    // Fermeture propre systématique : pas de demi-requête résiduelle possible.
    let _ = stream.shutdown(std::net::Shutdown::Write);
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

/// `read_exact` avec contrôle de la deadline globale entre deux remplissages.
fn read_exact_deadline(
    reader: &mut BufReader<TcpStream>,
    buf: &mut [u8],
    deadline: Instant,
) -> std::io::Result<()> {
    let mut filled = 0usize;
    while filled < buf.len() {
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "deadline dépassée",
            ));
        }
        let n = reader.read(&mut buf[filled..])?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "corps tronqué",
            ));
        }
        filled += n;
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
) -> std::io::Result<()> {
    let connection = if keep_alive { "keep-alive" } else { "close" };
    let head = format!(
        "HTTP/1.1 {status}\r\n{extra_headers}Content-Type: {content_type}\r\nContent-Length: {}\r\nConnection: {connection}\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}
