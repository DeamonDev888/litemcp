//! Transport Streamable HTTP **stateless** — HTTP/1.1 minimal sur `std::net`.
//!
//! Une seule route (`POST /mcp` par défaut) : chaque message JSON-RPC est un
//! POST, la réponse arrive en `application/json` ou en `text/event-stream`
//! (SSE request-scoped, un seul événement de réponse) selon l'en-tête
//! `Accept`. Pas de session, pas de listener `GET` (405), garde
//! DNS-rebinding sur `Origin`.
//!
//! Durcissement (Phase 3) : timeout de lecture par opération (anti-slowloris,
//! configurable via [`crate::ServerBuilder::read_timeout`]), timeout
//! d'écriture, limite du nombre d'en-têtes (`431`), limite de longueur de
//! ligne (`431`), support `Expect: 100-continue`.

use crate::server::ServerCore;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

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

/// Serveur HTTP attaché à un socket bindé
/// (résultat de [`crate::ServerBuilder::bind`]).
pub struct BoundServer {
    pub(crate) listener: TcpListener,
    pub(crate) core: Arc<ServerCore>,
    pub(crate) route: String,
    pub(crate) allow_origins: Vec<String>,
    pub(crate) read_timeout: Duration,
}

impl BoundServer {
    /// Adresse locale effective — utile après un bind sur le port 0.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Bloque et sert les connexions — un thread par connexion.
    pub fn serve(self) -> std::io::Result<()> {
        for stream in self.listener.incoming() {
            let Ok(stream) = stream else {
                continue;
            };
            let _ = stream.set_read_timeout(Some(self.read_timeout));
            let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
            let core = Arc::clone(&self.core);
            let route = self.route.clone();
            let allow_origins = self.allow_origins.clone();
            std::thread::spawn(move || {
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
    TimedOut,
    Closed,
}

/// Lit une ligne (jusqu'à `\n` inclus) dans `out` — sans jamais la dépasser
/// `cap` octets.
///
/// Contrairement à `BufRead::read_line`, la mémoire consommée est bornée :
/// un client hostile ne peut pas faire grossir le buffer au-delà de `cap`.
/// `Ok(())` : ligne complète dans `out`.
fn read_line_capped(
    reader: &mut BufReader<TcpStream>,
    out: &mut String,
    cap: usize,
) -> Result<(), LineFail> {
    out.clear();
    loop {
        let available = match reader.fill_buf() {
            Ok(bytes) => bytes,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(LineFail::TimedOut);
            }
            Err(_) => return Err(LineFail::Closed),
        };
        if available.is_empty() {
            return Err(LineFail::Closed);
        }
        if let Some(pos) = available.iter().position(|&byte| byte == b'\n') {
            let chunk = &available[..=pos];
            if out.len() + chunk.len() > cap {
                return Err(LineFail::TooLong);
            }
            out.push_str(&String::from_utf8_lossy(chunk));
            reader.consume(pos + 1);
            return Ok(());
        }
        if out.len() + available.len() > cap {
            return Err(LineFail::TooLong);
        }
        out.push_str(&String::from_utf8_lossy(available));
        let filled = available.len();
        reader.consume(filled);
    }
}

fn handle_connection(
    stream: TcpStream,
    core: &ServerCore,
    route: &str,
    allow_origins: &[String],
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut stream = stream;
    loop {
        // Ligne de requête — bornée, timeout par opération.
        let mut request_line = String::new();
        match read_line_capped(&mut reader, &mut request_line, MAX_LINE) {
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
            Err(LineFail::TimedOut) | Err(LineFail::Closed) => return Ok(()), // slowloris / coupure : sans réponse
        }
        let mut parts = request_line.split_whitespace();
        let verb = parts.next().unwrap_or("");
        let path = parts.next().unwrap_or("");
        let _version = parts.next().unwrap_or("HTTP/1.1");

        let mut content_length: Option<usize> = None;
        let mut origin: Option<String> = None;
        let mut accept = String::new();
        let mut expect_continue = false;
        let mut close = false;
        let mut header_count = 0usize;
        loop {
            let mut header = String::new();
            match read_line_capped(&mut reader, &mut header, MAX_LINE) {
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
                Err(LineFail::TimedOut) | Err(LineFail::Closed) => return Ok(()),
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
                    "content-length" => content_length = value.parse().ok(),
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
                respond(
                    &mut stream,
                    "403 Forbidden",
                    "",
                    "application/json",
                    r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"origin refusée"}}"#.as_bytes(),
                    !close,
                )?;
                if close {
                    break;
                }
                continue;
            }
        }

        if path != route {
            respond(
                &mut stream,
                "404 Not Found",
                "",
                "application/json",
                br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"route inconnue"}}"#,
                !close,
            )?;
            if close {
                break;
            }
            continue;
        }

        if verb != "POST" {
            respond(
                &mut stream,
                "405 Method Not Allowed",
                "Allow: POST\r\n",
                "application/json",
                br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"POST uniquement"}}"#,
                !close,
            )?;
            if close {
                break;
            }
            continue;
        }

        let Some(len) = content_length else {
            respond(
                &mut stream,
                "411 Length Required",
                "",
                "application/json",
                br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Content-Length requis"}}"#,
                !close,
            )?;
            if close {
                break;
            }
            continue;
        };
        if len > MAX_BODY {
            respond(
                &mut stream,
                "413 Payload Too Large",
                "",
                "application/json",
                br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"corps trop volumineux"}}"#,
                !close,
            )?;
            if close {
                break;
            }
            continue;
        }

        // `Expect: 100-continue` : le client attend notre feu vert avant
        // d'envoyer le corps — on l'envoie, puis on lit le corps.
        if expect_continue && len > 0 {
            stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
            stream.flush()?;
        }

        let mut body = vec![0u8; len];
        reader.read_exact(&mut body)?;

        let Some(reply) = core.dispatch(&body) else {
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

fn respond(
    stream: &mut TcpStream,
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
