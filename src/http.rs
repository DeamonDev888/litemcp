//! Transport Streamable HTTP **stateless** — HTTP/1.1 minimal sur `std::net`.
//!
//! Une seule route (`POST /mcp` par défaut) : chaque message JSON-RPC est un
//! POST, la réponse arrive en `application/json` ou en `text/event-stream`
//! (SSE request-scoped, un seul événement de réponse) selon l'en-tête
//! `Accept`. Pas de session, pas de listener `GET` (405), garde
//! DNS-rebinding sur `Origin`.

use crate::server::ServerCore;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;

/// Taille maximale d'un corps de requête (1 Mio par défaut).
pub const MAX_BODY: usize = 1 << 20;

/// Serveur HTTP attaché à un socket bindé
/// (résultat de [`crate::ServerBuilder::bind`]).
pub struct BoundServer {
    pub(crate) listener: TcpListener,
    pub(crate) core: Arc<ServerCore>,
    pub(crate) route: String,
    pub(crate) allow_origins: Vec<String>,
}

impl BoundServer {
    /// Adresse locale effective — utile après un bind sur le port 0.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Bloque et sert les connexions — un thread par connexion.
    pub fn serve(self) -> std::io::Result<()> {
        for stream in self.listener.incoming() {
            let Ok(stream) = stream else { continue };
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

fn handle_connection(
    stream: TcpStream,
    core: &ServerCore,
    route: &str,
    allow_origins: &[String],
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut stream = stream;
    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line)? == 0 {
            return Ok(()); // connexion fermée par le client
        }
        let mut parts = request_line.split_whitespace();
        let verb = parts.next().unwrap_or("");
        let path = parts.next().unwrap_or("");
        let _version = parts.next().unwrap_or("HTTP/1.1");

        let mut content_length: Option<usize> = None;
        let mut origin: Option<String> = None;
        let mut accept = String::new();
        let mut close = false;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header)? == 0 {
                return Ok(());
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':') {
                let name = name.trim().to_ascii_lowercase();
                let value = value.trim();
                match name.as_str() {
                    "content-length" => content_length = value.parse().ok(),
                    "origin" => origin = Some(value.to_string()),
                    "accept" => accept = value.to_ascii_lowercase(),
                    "connection" if value.eq_ignore_ascii_case("close") => close = true,
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
