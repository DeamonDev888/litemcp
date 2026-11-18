//! Flux SSE request-scoped : événements, canal de progression et `id`
//! de requête extrait du corps.

use super::*;

/// Marqueur de recherche rapide avant de parser le corps : la quasi-totalité
/// des requêtes ne porte pas de jeton de progression, et un `windows()` sur
/// 13 octets est moins coûteux qu'un parsing JSON systématique.
const PROGRESS_TOKEN_MARKER: &[u8] = b"progressToken";

/// Jeton `params._meta.progressToken` d'une requête **actionnable**.
///
/// Une notification (sans `id`) ne reçoit aucun flux ; les méthodes non
/// actionnables n'exécutent rien qui puisse rapporter une progression. Le
/// jeton doit être une chaîne ou un nombre (forme fil de la spec).
pub(super) fn progress_token_of(body: &[u8]) -> Option<serde_json::Value> {
    if !body
        .windows(PROGRESS_TOKEN_MARKER.len())
        .any(|window| window == PROGRESS_TOKEN_MARKER)
    {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let request = value.as_object()?;
    request.get("id")?;
    let method = request.get("method")?.as_str()?;
    if !crate::protocol::is_action(method) {
        return None;
    }
    let token = request
        .get("params")?
        .get("_meta")?
        .get(crate::protocol::META_PROGRESS_TOKEN)?;
    match token {
        serde_json::Value::String(_) | serde_json::Value::Number(_) => Some(token.clone()),
        _ => None,
    }
}

/// État d'écriture du flux de progression — sérialisé entre le fil du
/// handler (`tool_timeout`) et le fil de connexion (événement final).
struct ProgressWriter {
    stream: Conn,
    token: serde_json::Value,
    closed: bool,
}

/// Canal `notifications/progress` d'une requête servie en SSE.
///
/// Le jeton vient de la requête (jamais du handler) et chaque émission
/// reprend la forme fil de la spec. `closed` est posé **avant** l'écriture
/// de l'événement final : une émission tardive d'un handler détaché est
/// ignorée, jamais intercalée dans la réponse ni dans une requête suivante.
/// Le `cancel` porte le drapeau d'annulation de la requête : toute écriture
/// qui échoue (client parti) le lève — le handler le voit via
/// [`crate::cancel::is_cancelled`].
pub(super) struct SseProgressChannel {
    writer: Mutex<ProgressWriter>,
    cancel: CancelToken,
}

impl SseProgressChannel {
    pub(super) fn new(stream: Conn, token: serde_json::Value, cancel: CancelToken) -> Self {
        Self {
            writer: Mutex::new(ProgressWriter {
                stream,
                token,
                closed: false,
            }),
            cancel,
        }
    }

    /// Ferme le canal puis écrit l'événement de réponse final (s'il y en a
    /// un). Aucune notification ne peut suivre.
    ///
    /// Un échec d'écriture ici aussi est une annulation (le client a fermé
    /// avant l'événement final) : le drapeau est levé, même si le handler a
    /// déjà rendu son résultat.
    pub(super) fn finish(&self, reply: Option<&[u8]>) -> std::io::Result<()> {
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        writer.closed = true;
        let result = match reply {
            Some(reply) => writer
                .stream
                .write_all(&format_sse_event(reply))
                .and_then(|()| writer.stream.flush()),
            None => writer.stream.flush(),
        };
        if result.is_err() {
            self.cancel.cancel();
        }
        result
    }
}

impl ProgressChannel for SseProgressChannel {
    fn progress(&self, progress: f64, total: Option<f64>, message: Option<&str>) -> bool {
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if writer.closed {
            return false;
        }
        let mut params = serde_json::Map::new();
        params.insert("progressToken".to_string(), writer.token.clone());
        params.insert("progress".to_string(), serde_json::json!(progress));
        if let Some(total) = total {
            params.insert("total".to_string(), serde_json::json!(total));
        }
        if let Some(message) = message {
            params.insert(
                "message".to_string(),
                serde_json::Value::String(message.to_string()),
            );
        }
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": crate::protocol::NOTIFICATION_PROGRESS,
            "params": params,
        });
        let Ok(payload) = serde_json::to_vec(&notification) else {
            return false;
        };
        if writer
            .stream
            .write_all(&format_sse_event(&payload))
            .and_then(|()| writer.stream.flush())
            .is_err()
        {
            // Client parti : plus aucune écriture sur ce descripteur, et
            // l'annulation de la requête est signalée au handler.
            writer.closed = true;
            self.cancel.cancel();
            return false;
        }
        true
    }
}

/// Tête d'une réponse SSE **ouverte avant le dispatch** : pas de
/// `Content-Length` (le corps est délimité par la fermeture), anti-tampon
/// pour les proxies, et `Connection: close` pour qu'aucune écriture tardive
/// ne puisse être lue comme la réponse d'une requête suivante.
pub(super) fn respond_stream_head(
    stream: &mut impl Write,
    version: &str,
    extra_headers: &str,
) -> std::io::Result<()> {
    write!(
        stream,
        "{version} 200 OK\r\n{extra_headers}{SSE_ANTI_BUFFERING}Content-Type: text/event-stream\r\nConnection: close\r\n\r\n"
    )?;
    stream.flush()
}

/// Formate une charge utile en événement SSE (`data: ...\n\n`) en
/// neutralisant tout `\r` ou `\n` éventuel (défense en profondeur contre
/// l'injection de champs SSE `event:` / `id:` / `retry:`).
pub(super) fn format_sse_event(reply: &[u8]) -> Vec<u8> {
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
pub(super) fn extract_request_id(body: &[u8]) -> serde_json::Value {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
