//! Boucle d'une connexion : une requête à la fois, keep-alive borné,
//! routage, verdicts et réponse.

use super::parse::{LineFail, read_body, read_line_capped, starts_a_request_line};
use super::parse::{
    header_has_media_type, is_json_content_type, is_valid_field_value, is_valid_request_target,
    is_valid_token, parse_expect_100_continue, trim_ows,
};
use super::response::{
    ResponseHead, respond_413_without_drain, respond_error, respond_rejection, respond_version,
    shutdown_gracefully,
};
use super::security::{host_autorise, is_valid_host, is_valid_origin_syntax, respond_preflight};
use super::sse::{
    SseProgressChannel, extract_request_id, format_sse_event, progress_token_of,
    respond_stream_head,
};
use super::*;

/// État partagé d'une connexion — tout ce qui vit au-delà d'une requête,
/// référencé depuis le [`BoundServer`] (cloné en `Arc` par la boucle
/// d'accept).
pub(super) struct ConnectionEnv<'a> {
    pub(super) core: &'a ServerCore,
    pub(super) route: &'a str,
    pub(super) allow_origins: &'a [String],
    /// Hôtes acceptés dans `Host` (vide : règle locale par défaut).
    pub(super) allowed_hosts: &'a [String],
    pub(super) request_hooks: &'a [Arc<RequestHook>],
    /// En-tête d'identité d'instance (`X-Instance-Id: …`) émis par la sonde
    /// `/health` — pré-formaté une fois par serveur lié (chemin froid).
    pub(super) instance_headers: &'a str,
    pub(super) read_timeout: Duration,
    /// Échéance globale d'écriture déjà résolue (plafond explicite ou défaut
    /// dérivé de `read_timeout`) — calculée une fois par serveur lié.
    pub(super) write_budget: Duration,
    pub(super) connection_ttl: Duration,
    pub(super) shutdown: &'a std::sync::atomic::AtomicBool,
    pub(super) is_loopback_listener: bool,
    /// Schéma externe (`http` ou `https` selon TLS) : annonce du document
    /// RFC 9728 **et** préfixe attendu de l'`Origin` dans l'exception
    /// loopback même-origine.
    pub(super) scheme: &'static str,
}

pub(super) fn handle_connection(stream: Conn, env: ConnectionEnv<'_>) -> std::io::Result<()> {
    // Déstructuration : le corps de la fonction garde ses références
    // directes (`core`, `route`…) sans un seul renommage interne.
    let ConnectionEnv {
        core,
        route,
        allow_origins,
        allowed_hosts,
        request_hooks,
        instance_headers,
        read_timeout,
        write_budget,
        connection_ttl,
        shutdown,
        is_loopback_listener,
        scheme,
    } = env;
    // Un hook de requête est enregistré ? Sans lui, la rétention des en-têtes
    // bruts n'existe pas : le chemin chaud ne réserve rien, ne copie rien.
    let hook_present = !request_hooks.is_empty();
    // L'autorisation lit les en-têtes bruts (signature RFC 9421) : un
    // vérificateur configuré les retient donc exactement comme un hook —
    // sans hook **ni** vérificateur, le chemin chaud reste sans allocation.
    let auth_active = core.authorization_active();
    let retain_headers = hook_present || auth_active;
    // Entrepôt des en-têtes bruts — alloué une fois par connexion et
    // réinitialisé (sans désallocation) à chaque requête de la keep-alive.
    // Borné par construction : MAX_HEADERS (64) lignes de MAX_LINE (16 Kio)
    // chacune.
    let mut header_store = String::new();
    let mut header_spans: Vec<(u32, u32, u32, u32)> = Vec::new();
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
        // TTL au maximum (ou proche) : l'addition déborde d'Instant —
        // `checked_add` renvoie None, la connexion n'a alors **aucune**
        // échéance de durée de vie (jamais de panic).
        connection_started.checked_add(connection_ttl)
    };
    // Budget total d'une requête : proportionnel à `read_timeout` (×2, borné
    // à `REQUEST_DEADLINE` = 60 s). Un serveur configuré avec un
    // `read_timeout` court (ex. 400 ms) coupe aussi un client qui dribble
    // sous `read_timeout` en 800 ms au lieu d'attendre 60 s.
    let request_budget = read_timeout
        .saturating_mul(2)
        .min(REQUEST_DEADLINE)
        .max(read_timeout);
    // Échéance **globale** d'écriture (résolue par le serveur lié — voir
    // [`write_deadline`] et `BoundServer::write_budget`) : indépendante de
    // `WRITE_TIMEOUT`, qui ne borne qu'un `write_vectored`.
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
        // Échéance absolue de la requête : budget courant OU TTL de
        // connexion, le premier qui arrive. Un `Duration::MAX` utilisateur
        // déborde d'Instant : `checked_add` renvoie None — cette source
        // n'impose alors aucune échéance (jamais de panic).
        let mut deadline = now.checked_add(request_budget);
        if let Some(conn_end) = connection_deadline {
            if deadline.is_none_or(|budget_end| conn_end < budget_end) {
                deadline = Some(conn_end);
            }
        }
        // **Restauration du timeout de lecture** : `tighten_read_timeout`
        // ne fait que RESSERRER le timeout du socket (budget résiduel
        // < read_timeout). Sans restauration explicite, une requête lente
        // laisse le socket avec un timeout résiduel minuscule — la requête
        // keep-alive SUIVANTE est alors coupée sans réponse alors que son
        // gap est légal (< read_timeout). On repose le timeout nominal à
        // chaque tour ; `tighten_read_timeout` le resserrera si le budget
        // de cette requête l'exige.
        let _ = reader.get_ref().set_read_timeout(Some(read_timeout));

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
                    false,
                )?;
                return Ok(());
            }
            // LF seul : ligne lisible mais malformée → `400` (le verdict
            // du README : « fin de ligne LF seul | 400 »).
            Err(LineFail::MalformedEnding) => {
                respond_error(
                    &mut stream,
                    "400 Bad Request",
                    "invalid line ending in the request line",
                    "",
                    false,
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
        // Version du client, connue dès l'extraction : les rejets 400/505 qui
        // suivent répondent dans la version annoncée — un client HTTP/1.0
        // strict refuse une réponse `HTTP/1.1` (RFC 9112 §2.3). Une version
        // absente ou malformée reste servie en HTTP/1.1.
        let http10 = version == "HTTP/1.0";
        let response_version = if http10 { "HTTP/1.0" } else { "HTTP/1.1" };
        // Ligne de requête stricte : exactement trois jetons (méthode, cible,
        // version). RFC 9112 §3.
        if verb.is_empty() || full_path.is_empty() || version.is_empty() || parts.next().is_some() {
            respond_error(
                &mut stream,
                "400 Bad Request",
                "malformed request line",
                "",
                verb == "HEAD",
                http10,
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
                http10,
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
                http10,
            )?;
            return Ok(());
        }
        // Version sensible à la casse (RFC 9112 §2.3 : le protocole est un
        // jeton exact) — `http/1.1` n'est pas une version, c'est du garbage.
        if !http10 && version != "HTTP/1.1" {
            respond_error(
                &mut stream,
                "505 HTTP Version Not Supported",
                "unsupported HTTP version",
                "",
                verb == "HEAD",
                http10,
            )?;
            return Ok(());
        }
        // La query string est ignorée : POST /mcp?x=1 == POST /mcp.
        // Verdicts calculés maintenant : `line` est réemprunté (mutable)
        // par la lecture des en-têtes, on ne garde que les booléens.
        let path = full_path.split('?').next().unwrap_or(full_path);
        let is_post = verb == "POST";
        // Copies possédées pour l'autorisation uniquement : `line` est
        // réemprunté (mutable) par la lecture des en-têtes, le vérificateur
        // reçoit donc méthode et chemin possédés. Sans vérificateur, aucune
        // copie — le chemin chaud reste inchangé.
        let auth_verb = if auth_active {
            verb.to_string()
        } else {
            String::new()
        };
        let auth_path = if auth_active {
            path.to_string()
        } else {
            String::new()
        };
        let is_get = verb == "GET";
        let is_head = verb == "HEAD";
        let is_options = verb == "OPTIONS";
        let route_matched = path == route;
        let health_probe = path == HEALTH_ROUTE;
        // Route RFC 9728 : verdict calculé ici, `path` emprunte `line` que la
        // lecture des en-têtes va réemprunter (mutable).
        let metadata_probe = path == PROTECTED_RESOURCE_METADATA_ROUTE;
        // Copies possédées pour le hook de requête : `verb` et `full_path`
        // dérivent de `line`, réemprunté en mutable par la lecture des
        // en-têtes. Sans hook, aucune copie (chemin chaud inchangé).
        let hook_method = if hook_present {
            verb.to_string()
        } else {
            String::new()
        };
        let hook_full_path = if hook_present {
            full_path.to_string()
        } else {
            String::new()
        };

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
        let mut origin_header_value: Option<String> = None;
        let mut sec_fetch_site_seen = false;
        let mut sec_fetch_site_duplicate = false;
        let mut sec_fetch_site_cross = false;
        let mut host_seen = false;
        let mut host_duplicate = false;
        let mut host_invalid = false;
        let mut host_header_value: Option<String> = None;
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
        let mut mcp_protocol_version: Option<String> = None;
        let mut mcp_protocol_version_duplicate = false;
        let mut mcp_method: Option<String> = None;
        let mut mcp_name: Option<String> = None;
        let mut mcp_params: Vec<(String, String)> = Vec::new();
        let mut mcp_method_duplicate = false;
        let mut mcp_name_duplicate = false;
        let mut mcp_param_duplicate = false;
        // En-têtes d'identité (auth / cookies / proxy) : un doublon rend le
        // verdict d'authentification dépendant de l'ordre de lecture (la
        // première occurrence ici, une combinaison ou la dernière occurrence
        // chez un reverse proxy) — refusé comme les autres en-têtes à valeur
        // unique.
        let mut authorization_seen = false;
        let mut authorization_duplicate = false;
        let mut cookie_seen = false;
        let mut cookie_duplicate = false;
        let mut x_forwarded_for_seen = false;
        let mut x_forwarded_for_duplicate = false;
        // `Connection: close` a-t-il été vu ? (HTTP/1.0 : la connexion est
        // fermée par défaut, `keep-alive` doit pouvoir la maintenir ouverte,
        // mais `close` reste ABSORBANT s'il apparaît).
        let mut connection_close_seen = false;
        let mut close = http10; // HTTP/1.0 : fermeture par défaut
        let mut header_count = 0usize;
        // Rétention des en-têtes de CETTE requête — l'entrepôt est vidé
        // (capacité conservée) à chaque itération keep-alive : le hook ne
        // doit jamais voir un en-tête de la requête précédente.
        if retain_headers {
            header_store.clear();
            header_spans.clear();
        }
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
                        http10,
                    )?;
                    return Ok(());
                }
                // LF seul dans une ligne d'en-tête : idem ligne de requête,
                // `400` (le client saura pourquoi la connexion est fermée).
                Err(LineFail::MalformedEnding) => {
                    respond_error(
                        &mut stream,
                        "400 Bad Request",
                        "invalid line ending in header",
                        "",
                        is_head,
                        http10,
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
                    http10,
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
                    http10,
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
                    http10,
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
                        http10,
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
                        http10,
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
                        http10,
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
                        http10,
                    )?;
                    return Ok(());
                }
                let name = name_raw;
                let value = trim_ows(value_raw);
                if retain_headers {
                    // Rétention brute (nom + valeur) pour le hook et pour le
                    // vérificateur : copie dans l'entrepôt réutilisé, la
                    // ligne courante va être écrasée par la lecture suivante.
                    let name_start = header_store.len();
                    header_store.push_str(name);
                    let value_start = header_store.len();
                    header_store.push_str(value);
                    header_spans.push((
                        name_start as u32,
                        value_start as u32,
                        value_start as u32,
                        header_store.len() as u32,
                    ));
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
                        origin_header_value = Some(value.to_string());
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
                        host_header_value = Some(value.to_string());
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
                } else if name.eq_ignore_ascii_case("mcp-protocol-version") {
                    if mcp_protocol_version.is_some() {
                        mcp_protocol_version_duplicate = true;
                    } else {
                        mcp_protocol_version = Some(value.to_string());
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
                } else if name.len() > 10 && name[..10].eq_ignore_ascii_case("mcp-param-") {
                    let suffix = name[10..].to_ascii_lowercase();
                    if mcp_params.iter().any(|(k, _)| k == &suffix) {
                        mcp_param_duplicate = true;
                    } else {
                        mcp_params.push((suffix, value.to_string()));
                    }
                } else if name.eq_ignore_ascii_case("authorization") {
                    if authorization_seen {
                        authorization_duplicate = true;
                    } else {
                        authorization_seen = true;
                    }
                } else if name.eq_ignore_ascii_case("cookie") {
                    if cookie_seen {
                        cookie_duplicate = true;
                    } else {
                        cookie_seen = true;
                    }
                } else if name.eq_ignore_ascii_case("x-forwarded-for") {
                    if x_forwarded_for_seen {
                        x_forwarded_for_duplicate = true;
                    } else {
                        x_forwarded_for_seen = true;
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
                    http10,
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
                http10,
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
                http10,
            )?;
            return Ok(());
        }

        // En-têtes à valeur unique dupliqués (`Content-Type`,
        // `Content-Encoding`, `Sec-Fetch-Site`, `Mcp-Method`, `Mcp-Name`) :
        // requête ambiguë — refus net 400.
        if content_type_duplicate
            || content_encoding_duplicate
            || sec_fetch_site_duplicate
            || mcp_protocol_version_duplicate
            || mcp_method_duplicate
            || mcp_name_duplicate
            || mcp_param_duplicate
            || authorization_duplicate
            || cookie_duplicate
            || x_forwarded_for_duplicate
        {
            respond_error(
                &mut stream,
                "400 Bad Request",
                "duplicated single-value header",
                "",
                is_head,
                http10,
            )?;
            return Ok(());
        }

        // Exception loopback stricte (garde DNS-rebinding) : autorise par défaut une origine
        // loopback strictement cohérente avec le `Host` et le listener local,
        // à condition que la requête ne soit pas cross-site et que le schéma
        // de l'origine soit celui du listener (`http` ou `https` selon TLS).
        let local_addr = stream.local_addr().ok();
        if is_loopback_listener
            && !origin_allowed
            && origin_seen
            && !origin_malformed
            && !sec_fetch_site_cross
        {
            if let (Some(origin_str), Some(host_str), Some(laddr)) = (
                origin_header_value.as_deref(),
                host_header_value.as_deref(),
                local_addr,
            ) {
                if !origin_str.eq_ignore_ascii_case("null") {
                    // Préfixe attendu : le schéma **réel** du listener, pas
                    // `http://` en dur — un navigateur même-origine sur TLS
                    // porte `https://127.0.0.1:PORT` et était sinon rejeté.
                    // La longueur suit le schéma (7 ou 8), sans troncature.
                    let prefixe = format!("{scheme}://");
                    let scheme_auth = if origin_str.len() >= prefixe.len()
                        && origin_str[..prefixe.len()].eq_ignore_ascii_case(&prefixe)
                    {
                        Some(&origin_str[prefixe.len()..])
                    } else {
                        None
                    };
                    if let Some(auth) = scheme_auth {
                        if auth.eq_ignore_ascii_case(host_str) {
                            let expected_auth = match laddr {
                                SocketAddr::V4(v4) => format!("{}:{}", v4.ip(), v4.port()),
                                SocketAddr::V6(v6) => format!("[{}]:{}", v6.ip(), v6.port()),
                            };
                            if auth.eq_ignore_ascii_case(&expected_auth) {
                                origin_allowed = true;
                            }
                        }
                    }
                }
            }
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
                http10,
            )?;
            return Ok(());
        }

        // En-têtes CORS de **cette** requête : émis seulement quand l'origine
        // est explicitement autorisée (`allow_origins` ou exception loopback).
        // Aucune allocation quand la requête n'a pas d'`Origin` autorisé — le
        // chemin chaud (CLI) reste identique octet pour octet.
        let cors_headers = if origin_allowed {
            match origin_header_value.as_deref() {
                Some(origin) => {
                    format!("Access-Control-Allow-Origin: {origin}\r\nVary: Origin\r\n")
                }
                None => String::new(),
            }
        } else {
            String::new()
        };

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
                http10,
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
                http10,
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
                http10,
            )?;
            return Ok(());
        }

        // **`Host` confronté au listener** (garde DNS-rebinding, `421`) : la
        // syntaxe ne dit pas si le nom désigne *ce* serveur. Un nom
        // pleinement qualifié (`attaquant.example`) ou un littéral d'adresse
        // étranger (`10.1.2.3`) est refusé même **sans `Origin`** — le cas
        // qu'un client non-navigateur (ou une page qui omet l'en-tête)
        // exploitait pour atteindre le loopback par un nom qu'il contrôle.
        // Règle par défaut et liste explicite : voir [`host_autorise`] et
        // [`crate::ServerBuilder::allowed_hosts`].
        if host_seen
            && !host_autorise(
                host_header_value.as_deref().unwrap_or_default(),
                allowed_hosts,
                local_addr,
            )
        {
            respond_error(
                &mut stream,
                "421 Misdirected Request",
                "Host not allowed",
                "",
                is_head,
                http10,
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
                http10,
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
                http10,
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
                    http10,
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
                        http10,
                    )?;
                    return Ok(());
                }
            }
        };
        if len > MAX_BODY {
            // Le client peut déjà streamer le corps : on répond puis on
            // ferme — drainer 1 Mio+ nourrirait l'attaque qu'on refuse.
            //
            // Le drain de `respond_error` est borné à 64 Kio : au-delà, le
            // tampon de réception n'est pas vidé et le `close()` émet un
            // **RST** qui purge chez le client la réponse qu'on vient
            // d'écrire (le 413 était perdu de façon non déterministe —
            // mesuré : 4/8 à 8/8 selon la taille du corps et le délai de
            // lecture). On passe donc par une réponse 413 **sans drain** :
            // le FIN part, le client lit le 413, et le corps restant est
            // abandonné (il sera jeté par le RST de fin de connexion, après
            // que le client a lu la réponse).
            respond_413_without_drain(
                &mut stream,
                "413 Payload Too Large",
                "body too large",
                is_head,
                http10,
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
                        http10,
                    )?;
                    return Ok(());
                }
                respond_version(
                    &mut stream,
                    &ResponseHead {
                        version: response_version,
                        status: "200 OK",
                        content_type: "text/plain",
                    },
                    instance_headers,
                    b"ok",
                    !close,
                    is_head,
                    write_budget,
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
                http10,
            )?;
            return Ok(());
        }

        // Document **RFC 9728** (métadonnées de ressource protégée) : servi
        // seulement quand l'autorisation est active — sans elle, la route
        // n'existe pas (`404`). Il est volontairement **honnête** : aucun
        // serveur d'autorisation, aucun jeton porteur, donc pas de
        // `bearer_methods_supported` — le profil est une signature de
        // requête, et le client l'apprend par `www_authenticate`.
        if core.authorization_active() && metadata_probe {
            if !is_get && !is_head {
                respond_error(
                    &mut stream,
                    "405 Method Not Allowed",
                    "GET or HEAD only",
                    "Allow: GET, HEAD\r\n",
                    is_head,
                    http10,
                )?;
                return Ok(());
            }
            if len != 0 || content_encoding_seen {
                respond_error(
                    &mut stream,
                    "400 Bad Request",
                    "body not allowed on the metadata probe",
                    "",
                    is_head,
                    http10,
                )?;
                return Ok(());
            }
            let authority = host_header_value.as_deref().unwrap_or_default();
            let document = serde_json::json!({
                "resource": format!("{scheme}://{authority}{route}"),
                "methods": ["POST"],
                "www_authenticate": crate::auth::WWW_AUTHENTICATE,
                "authorization_servers": [],
                "litemcp_signature_profile": "litemcp-auth-v1",
            });
            let document = document.to_string();
            respond_version(
                &mut stream,
                &ResponseHead {
                    version: response_version,
                    status: "200 OK",
                    content_type: "application/json",
                },
                "Cache-Control: no-store\r\n",
                document.as_bytes(),
                !close,
                is_head,
                write_budget,
            )?;
            if close {
                break;
            }
            continue;
        }

        if !route_matched {
            respond_error(
                &mut stream,
                "404 Not Found",
                "unknown route",
                "",
                is_head,
                http10,
            )?;
            return Ok(());
        }

        // Verrou utilisateur (auth / rate-limit) — voir
        // [`crate::ServerBuilder::on_request`]. Placé **après** tous les
        // verdicts de protocole (400/431/403 Origin/411/413/404) : le
        // durcissement intégré reste arbitre, le hook ne fait qu'autoriser
        // l'accès à la route MCP. Placé **avant** la méthode, le
        // `Expect: 100-continue` et la lecture du corps : un rejet ne paie
        // ni le feu vert ni le transport du payload — la connexion ferme
        // (corps non lu, impossible à réinterpréter). `/health` (sondes sans
        // authentification) et les routes inconnues n'y passent jamais.
        if hook_present {
            let peer = stream
                .peer_addr()
                .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)));
            let (hook_path, hook_query) = match hook_full_path.split_once('?') {
                Some((only_path, query)) => (only_path, Some(query)),
                None => (hook_full_path.as_str(), None),
            };
            let context = RequestContext {
                method: &hook_method,
                path: hook_path,
                query: hook_query,
                peer,
                store: &header_store,
                spans: &header_spans,
            };
            for hook in request_hooks {
                // Panique du hook contenue : verdict `500`, connexion fermée,
                // le serveur continue — comme une panique de handler.
                let verdict = std::panic::catch_unwind(AssertUnwindSafe(|| hook(&context)));
                match verdict {
                    Ok(Ok(())) => {}
                    Ok(Err(rejection)) => {
                        respond_rejection(&mut stream, &rejection, is_head, http10)?;
                        return Ok(());
                    }
                    Err(_) => {
                        let rejection =
                            RequestRejection::new(500, "internal error (request hook panicked)");
                        respond_rejection(&mut stream, &rejection, is_head, http10)?;
                        return Ok(());
                    }
                }
            }
        }

        // Verdict de **méthode** : APRÈS le verrou utilisateur (l'authentification
        // gagne sur le `405` — un client non autorisé ne doit pas apprendre que la
        // route existe autrement que par un refus d'authentification ; voir
        // `tests/request_hook.rs::gate_runs_before_method_check`). `OPTIONS` est
        // admis : c'est le préflight CORS, jugé plus bas (après la lecture du
        // corps, pour qu'une connexion keep-alive ne se désynchronise pas).
        if !is_post && !is_options {
            respond_error(
                &mut stream,
                "405 Method Not Allowed",
                "POST only",
                "Allow: POST, OPTIONS\r\n",
                is_head,
                http10,
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
                http10,
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
                http10,
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
                http10,
            );
            return Ok(());
        }

        // **Cadrage de la requête** : le corps a été lu en entier ; s'il
        // reste des octets DÉJÀ reçus dans le tampon, le client a envoyé plus
        // que son `Content-Length`. Ces octets sont le début de la requête
        // SUIVANTE (c'est ainsi que fonctionne le pipelining HTTP, supporté
        // ici) — mais s'ils ne commencent pas par une ligne de requête
        // plausible, le cadrage est ambigu : le résidu est alors attribué à
        // CETTE requête (qui a mal annoncé sa longueur) au lieu de devenir
        // une requête fantôme. Sans ce contrôle, un résidu non conforme
        // produisait une réponse `400` parasite intercalée entre la réponse
        // attendue et la requête suivante, et la fermeture de la connexion
        // faisait perdre la réponse du client.
        if !reader.buffer().is_empty() && !starts_a_request_line(reader.buffer()) {
            let _ = respond_error(
                &mut stream,
                "400 Bad Request",
                "request body larger than Content-Length",
                "",
                is_head,
                http10,
            );
            return Ok(());
        }

        // **Préflight CORS** (`OPTIONS` sur la route MCP) : la seule méthode
        // supplémentaire admise. Il exige une origine **explicitement
        // autorisée** — la garde `Origin` ci-dessus a déjà refusé les autres
        // en `403`, et sans `Origin` (`cors_headers` vide) rien n'est
        // autorisé : un préflight sans origine n'a pas de sens. Le corps de
        // la requête est déjà consommé (cadrage ci-dessus), donc la connexion
        // peut rester keep-alive.
        if is_options {
            let Some(origin) = origin_header_value.as_deref().filter(|_| origin_allowed) else {
                respond_error(
                    &mut stream,
                    "403 Forbidden",
                    "CORS preflight requires an allowed Origin",
                    "",
                    false,
                    http10,
                )?;
                return Ok(());
            };
            respond_preflight(&mut stream, response_version, origin, !close, write_budget)?;
            if close {
                break;
            }
            continue;
        }

        // En-têtes de routage MCP (SEP-2243) — construits une seule fois :
        // le chemin `notifications/progress` et le chemin nominal partagent
        // le même verdict de cohérence.
        let headers = crate::server::McpHeaders {
            protocol_version: mcp_protocol_version,
            method: mcp_method,
            name: mcp_name,
            params: mcp_params,
        };

        // ------------------------------------------------------------------
        // **Autorisation** — placée ici, c'est-à-dire APRÈS la lecture du
        // corps et tous les contrôles de transport (ligne de requête,
        // cadrage, `Host`, `Origin`, `Content-Type`, `Content-Encoding`,
        // méthode, préflight) : un refus de transport (`400`, `403`, `404`,
        // `405`, `415`, `421`) n'est jamais transformé en refus d'identité,
        // et un `401` ne précède jamais un contrôle moins cher.
        //
        // Refus → `401` + `WWW-Authenticate` + corps **constant** (celui des
        // rejets de transport) ; le motif précis ne part que vers
        // `on_auth_decision`. Succès → dispatch avec l'identité établie.
        // Sans vérificateur, ce bloc n'existe pas : aucune allocation, aucun
        // en-tête retenu, chemin historique inchangé.
        // ------------------------------------------------------------------
        let auth_context = if auth_active {
            let received: Vec<(&str, &str)> = header_spans
                .iter()
                .filter_map(|&(name_start, name_end, value_start, value_end)| {
                    Some((
                        header_store.get(name_start as usize..name_end as usize)?,
                        header_store.get(value_start as usize..value_end as usize)?,
                    ))
                })
                .collect();
            let request = crate::auth::VerifiedRequest::new(
                &auth_verb,
                &auth_path,
                host_header_value.as_deref().unwrap_or_default(),
                &received,
                &body,
            );
            match core.authenticate(&request) {
                Some(Ok(context)) => Some(context),
                Some(Err(error)) => {
                    let (method, name) = crate::server::audit_target(&body, &headers);
                    core.report_authentication_refusal(&method, name.as_deref(), error.reason());
                    let extra_headers =
                        format!("WWW-Authenticate: {}\r\n", crate::auth::WWW_AUTHENTICATE);
                    respond_error(
                        &mut stream,
                        "401 Unauthorized",
                        "unauthorized",
                        &extra_headers,
                        is_head,
                        http10,
                    )?;
                    return Ok(());
                }
                // Aucun vérificateur : `auth_active` était faux.
                None => None,
            }
        } else {
            None
        };

        // **Progression request-scoped** : le client a fourni un jeton ET
        // accepte `text/event-stream` → la réponse est un flux SSE ouvert
        // AVANT le dispatch, alimenté par `progress::report` pendant
        // l'exécution et refermé par l'événement de réponse. C'est le seul
        // chemin où le handler écrit avant la réponse finale ; il reste
        // borné à une requête (pas de flux longue durée, pas de session) et
        // la connexion est fermée à la fin pour qu'aucune écriture tardive
        // d'un handler détaché ne puisse atteindre une requête suivante.
        // Sans jeton, le transport garde sa réponse en un seul write.
        //
        // Conséquence du streaming : la tête part en `200 OK` et le verdict
        // JSON-RPC (succès **ou** erreur) voyage dans l'événement final — un
        // statut 4xx ne peut plus être annoncé une fois le flux ouvert.
        if accept_sse {
            if let Some(token) = progress_token_of(&body) {
                respond_stream_head(&mut stream, response_version, &cors_headers)?;
                // **Annulation coopérative** : le jeton du flux est lié au fil
                // du dispatch (le fil du handler le réinstalle à son tour) ;
                // la première écriture SSE qui échoue le lève, et le handler
                // sort à son prochain point sûr (voir [`crate::cancel`]).
                let cancellation = CancelToken::new();
                // **Budget d'écriture du flux** : `write_budget`, le même que
                // pour la réponse finale, mais compté en temps passé à
                // écrire (voir [`SseProgressChannel`]) — pas en durée de vie,
                // sinon un handler long qui progresse se ferait couper. Sous
                // `no_tool_timeout()`, c'est la seule borne des émissions.
                let channel = Arc::new(SseProgressChannel::new(
                    stream.try_clone()?,
                    token,
                    cancellation.clone(),
                    write_budget,
                ));
                let dispatch_result = {
                    let _cancel = crate::cancel::bind(Some(cancellation));
                    std::panic::catch_unwind(AssertUnwindSafe(|| match &auth_context {
                        Some(context) => core.dispatch_authenticated_with_progress(
                            &body,
                            Some(&headers),
                            context,
                            channel.clone(),
                        ),
                        None => core.dispatch_request_with_progress(
                            &body,
                            Some(&headers),
                            channel.clone(),
                        ),
                    }))
                };
                let reply = match dispatch_result {
                    Ok(Some(resp)) => Some(resp.body),
                    Ok(None) => None,
                    Err(_) => Some(
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": extract_request_id(&body),
                            "error": { "code": -32603, "message": "internal error" }
                        })
                        .to_string()
                        .into_bytes(),
                    ),
                };
                channel.finish(reply.as_deref())?;
                shutdown_gracefully(&mut stream);
                break;
            }
        }

        let dispatch_result = std::panic::catch_unwind(AssertUnwindSafe(|| match &auth_context {
            Some(context) => core.dispatch_authenticated(&body, Some(&headers), context),
            None => core.dispatch_request(&body, Some(&headers)),
        }));

        let (reply, http_status) = match dispatch_result {
            Ok(Some(resp)) => (resp.body, resp.status.as_str()),
            Ok(None) => {
                // Notification : aucune réponse JSON-RPC attendue.
                respond_version(
                    &mut stream,
                    &ResponseHead {
                        version: response_version,
                        status: "202 Accepted",
                        content_type: "application/json",
                    },
                    &cors_headers,
                    b"",
                    !close,
                    false,
                    write_budget,
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
                respond_version(
                    &mut stream,
                    &ResponseHead {
                        version: response_version,
                        status: "200 OK",
                        content_type: "application/json",
                    },
                    &cors_headers,
                    payload.to_string().as_bytes(),
                    false,
                    false,
                    write_budget,
                )?;
                shutdown_gracefully(&mut stream);
                return Ok(());
            }
        };

        // SSE request-scoped (sans jeton de progression) : le client ne veut
        // qu'un flux d'événements, et la réponse tient en un seul événement.
        //
        // **Pas de SSE longue durée** : aucun flux n'est tenu entre deux
        // requêtes — un POST, un événement, clôture. La seule exception est
        // le flux de progression ouvert plus haut, borné à la durée du
        // handler et refermé par l'événement de réponse. C'est ce qui rend le
        // transport immunisé contre les proxies de VPS qui tamponnent les
        // flux en cours (empilement jusqu'au seuil de leur buffer) : une
        // réponse terminée porte un `Content-Length` explicite et un
        // `Connection: close` qui la font toujours pousser par le proxy.
        //
        // Contrat de transport pour tout écrivain SSE : flusher après
        // CHAQUE événement (`respond` fait `write_all` + `flush`) et porter
        // [`SSE_ANTI_BUFFERING`].
        //
        // **Annulation coopérative (spec `2026-07-28`, MUST)** : sur un flux
        // de progression, une déconnexion client n'est détectable qu'à la
        // première écriture qui échoue — le canal est alors fermé **et** le
        // drapeau `crate::cancel` de la requête est levé. Le handler le
        // consulte à ses points sûrs (`cancel::is_cancelled()`) et sort de
        // lui-même ; le transport n'interrompt jamais un fil (impossible en
        // Rust), donc le contrat est explicitement **coopératif** : un
        // handler qui ne consulte pas le drapeau s'exécute jusqu'au bout, et
        // seule la borne `tool_timeout` (30 s par défaut) protège le
        // serveur. `notifications/cancelled` n'est pas implémentée (profil
        // stateless, aucun flux à annuler hors requête).
        if accept_sse && !accept_json {
            let event = format_sse_event(&reply);
            // En-têtes CORS (origine autorisée) concaténés aux anti-tampon —
            // allocation seulement quand une origine est effectivement
            // autorisée, la constante reste le cas nominal.
            let sse_headers = if cors_headers.is_empty() {
                SSE_ANTI_BUFFERING.to_string()
            } else {
                format!("{cors_headers}{SSE_ANTI_BUFFERING}")
            };
            respond_version(
                &mut stream,
                &ResponseHead {
                    version: response_version,
                    status: http_status,
                    content_type: "text/event-stream",
                },
                &sse_headers,
                &event,
                false,
                false,
                write_budget,
            )?;
            shutdown_gracefully(&mut stream);
            break; // un seul événement de réponse, puis clôture
        }

        respond_version(
            &mut stream,
            &ResponseHead {
                version: response_version,
                status: http_status,
                content_type: "application/json",
            },
            &cors_headers,
            &reply,
            !close,
            false,
            write_budget,
        )?;
        if close {
            stream.shutdown_write();
            break;
        }
    }
    Ok(())
}
