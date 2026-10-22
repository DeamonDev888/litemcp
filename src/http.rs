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
//! - plafond de connexions simultanées (`MAX_CONNECTIONS`, défaut 256) :
//!   au-delà, la connexion est refusée immédiatement — pas de spawn illimité ;
//! - limites `MAX_HEADERS` (64) / `MAX_LINE` (16 Kio) → `431`, `MAX_BODY`
//!   (1 Mio) → `413` ;
//! - `Transfer-Encoding` présent ou `Content-Length` dupliqué → `400`
//!   (pas de désynchronisation type request smuggling derrière un proxy) ;
//! - `Expect: 100-continue` honoré, et toute réponse d'erreur pré-corps
//!   **draine ou ferme** : jamais d'octets résiduels réinterprétés comme une
//!   requête suivante ;
//! - `POST /mcp?query=…` accepté (la query est ignorée) ;
//! - `HTTP/1.0` sans `Connection: keep-alive` → connexion fermée ;
//! - panique d'un handler capturée (`catch_unwind`) → `-32603` JSON-RPC,
//!   la connexion du client reste propre.

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

/// Plafond de connexions simultanées — au-delà, refus immédiat (pas de
/// spawn). Défaut 256.
pub const MAX_CONNECTIONS: usize = 256;

static LIVE_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

/// Serveur HTTP attaché à un socket bindé
/// (résultat de [`crate::ServerBuilder::bind`]).
pub struct BoundServer {
    pub(crate) listener: TcpListener,
    pub(crate) core: Arc<ServerCore>,
    pub(crate) route: String,
    pub(crate) allow_origins: Vec<String>,
    pub(crate) read_timeout: Duration,
    pub(crate) max_connections: usize,
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
            // Plafond de concurrence : au-delà, on refuse net — chaque
            // connexion vivante coûte un thread + un buffer.
            if LIVE_CONNECTIONS.load(Ordering::Relaxed) >= self.max_connections {
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
            LIVE_CONNECTIONS.fetch_add(1, Ordering::Relaxed);
            let _ = stream.set_read_timeout(Some(self.read_timeout));
            let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
            let _ = stream.set_nodelay(true);
            let core = Arc::clone(&self.core);
            let route = self.route.clone();
            let allow_origins = self.allow_origins.clone();
            std::thread::spawn(move || {
                let _ = handle_connection(stream, &core, &route, &allow_origins);
                LIVE_CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
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
        let version = parts.next().unwrap_or("HTTP/1.1");
        let http10 = version.eq_ignore_ascii_case("HTTP/1.0");
        // La query string est ignorée : POST /mcp?x=1 == POST /mcp.
        let path = full_path.split('?').next().unwrap_or(full_path);

        let mut content_length: Option<usize> = None;
        let mut duplicate_length = false;
        let mut transfer_encoding = false;
        let mut origin: Option<String> = None;
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
                        if content_length.is_some() {
                            duplicate_length = true; // CL-CL : jamais interprété
                        } else {
                            content_length = value.parse().ok();
                        }
                    }
                    "transfer-encoding" => transfer_encoding = true, // TE : refusé
                    "origin" => origin = Some(value.to_string()),
                    "accept" => accept = value.to_ascii_lowercase(),
                    "connection" if value.eq_ignore_ascii_case("close") => close = true,
                    "expect" if value.to_ascii_lowercase().contains("100-continue") => {
                        expect_continue = true;
                    }
                    _ => {}
                }
            }
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
            // Corps incomplet à mi-chemin : connexion non réutilisable.
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
            Err(panicked) => {
                // Panique du handler (code utilisateur) : réponse propre,
                // connexion fermée par prudence.
                let message = if let Some(text) = panicked.downcast_ref::<&str>() {
                    (*text).to_string()
                } else if let Some(text) = panicked.downcast_ref::<String>() {
                    text.clone()
                } else {
                    "panique de l'outil".to_string()
                };
                let payload = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": { "code": -32603, "message": format!("erreur interne : {message}") }
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

/// Réponse d'erreur pré-corps : si un `Content-Length` valide et dans la
/// limite a déjà été lu, le corps est **drainé** (borné par `MAX_BODY`) pour
/// préserver le keep-alive ; sinon la connexion est **fermée** — dans les
/// deux cas, aucun octet résiduel ne peut être réinterprété comme une
/// requête suivante.
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
