//! Lecture et validation de la syntaxe HTTP/1.1 : lignes d'en-tête
//! bornées, corps par tranches, jetons, cibles de requête, valeurs de champ.

use super::conn::Conn;
use super::*;

/// Échec de lecture d'une ligne : trop longue, client trop lent, ligne
/// lisible mais sans terminaison CRLF (LF seul), ou connexion fermée.
pub(super) enum LineFail {
    TooLong,
    /// Ligne UTF-8 valide mais terminée par un LF seul (RFC 9112 §2.2) —
    /// lisible, donc répondue en `400` (contrairement à `Slow`/`Closed`,
    /// qui ferment sans réponse : un client lent ou parti n'a pas à être
    /// nourri).
    MalformedEnding,
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
/// Délai de lecture resserrable — `Conn` comme `TcpStream` (les refus 503
/// partent sur la socket nue, avant toute session TLS).
pub(super) trait DeadlineRead {
    fn set_read_deadline(&self, timeout: Duration);
}

impl DeadlineRead for Conn {
    fn set_read_deadline(&self, timeout: Duration) {
        let _ = self.set_read_timeout(Some(timeout));
    }
}

impl DeadlineRead for TcpStream {
    fn set_read_deadline(&self, timeout: Duration) {
        let _ = self.set_read_timeout(Some(timeout));
    }
}

pub(super) fn tighten_read_timeout<S: DeadlineRead + ?Sized>(
    stream: &S,
    read_timeout: Duration,
    deadline: Option<Instant>,
) -> Option<()> {
    let now = Instant::now();
    // `None` = aucune échéance absolue (`Duration::MAX` utilisateur : la
    // source ne borne rien) — le timeout par opération suffit, aucun
    // resserrage n'est nécessaire.
    if let Some(deadline) = deadline {
        if now >= deadline {
            return None;
        }
        let remaining = deadline.saturating_duration_since(now);
        if remaining < read_timeout {
            stream.set_read_deadline(remaining.max(Duration::from_millis(1)));
        }
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
pub(super) fn read_line_capped(
    reader: &mut BufReader<Conn>,
    out: &mut String,
    scratch: &mut Vec<u8>,
    cap: usize,
    read_timeout: Duration,
    deadline: Option<Instant>,
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
                    // CRLF). Un LF seul (« httpoxy ») est une ligne lisible
                    // mais malformée : répondue en `400` par l'appelant
                    // ([`LineFail::MalformedEnding`]) — un client qui
                    // parle du HTTP doit savoir pourquoi il est refusé.
                    if !text.ends_with("\r\n") {
                        return Err(LineFail::MalformedEnding);
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

/// Lit un corps de `len` octets **par tranches** dans `body`.
///
/// La capacité initiale est bornée à [`BODY_CHUNK`] (64 Kio) et `body` est
/// réutilisé sur la connexion keep-alive : la mémoire engagée par requête
/// reste proportionnelle aux octets **effectivement reçus**, pas au
/// `Content-Length` annoncé. Un client qui annonce 1 Mio et n'envoie rien ne
/// consomme donc plus 1 Mio de commit (amplification ×9 800 → ×1).
pub(super) fn read_body(
    reader: &mut BufReader<Conn>,
    body: &mut Vec<u8>,
    len: usize,
    read_timeout: Duration,
    deadline: Option<Instant>,
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

/// Le tampon résiduel commence-t-il par une **ligne de requête plausible** ?
///
/// Une ligne de requête commence par un jeton de méthode (`tchar`, RFC 9110
/// §5.6.2) suivi d'un espace. Le test est volontairement tolérant :
/// - pas encore d'espace dans le tampon → le jeton est encore en cours
///   (pipelining partiel) : accepté si tous les caractères vus sont des
///   `tchar` ;
/// - espace présent → le jeton de méthode doit être complet et valide.
///
/// Un résidu qui ne ressemble à rien (binaire, JSON, texte arbitraire)
/// n'est pas une requête : le cadrage est ambigu et la requête courante est
/// refusée plutôt que de laisser un résidu devenir une requête fantôme.
///
/// Une **ligne vide** (résidu `\r\n\r\n` — un client qui termine son message
/// par une ligne vide en trop) n'est pas non plus une ligne de requête :
/// un jeton vide n'est pas un jeton (`1*tchar`). La refuser ici la fait
/// attribuer à la requête courante (refus 400, connexion fermée) au lieu de
/// la laisser atteindre la boucle keep-alive, qui la relirait comme une
/// ligne de requête et rendrait une seconde réponse 400 parasite après la
/// réponse attendue (audit 2026-11-13, § 2.2).
pub(super) fn starts_a_request_line(buffer: &[u8]) -> bool {
    // On ne regarde que la première ligne (jusqu'au premier LF).
    let line = match buffer.iter().position(|&b| b == b'\n') {
        Some(position) => &buffer[..position],
        None => buffer,
    };
    // La ligne peut finir par un CR (CRLF) — on l'ignore.
    let line = match line.last() {
        Some(b'\r') => &line[..line.len() - 1],
        _ => line,
    };
    match line.iter().position(|&b| b == b' ') {
        // Espace trouvé : le jeton de méthode doit être complet et valide.
        // Le verdict porte sur des **octets** : un résidu partiel n'est pas
        // forcément de l'UTF-8 valide, et `String::from_utf8_lossy` allouait
        // puis remplaçait les octets invalides — un verdict biaisé pour un
        // jeton qui, de toute façon, ne peut être qu'ASCII (`tchar`).
        Some(position) => is_valid_token_bytes(&line[..position]),
        // Pas encore d'espace : jeton partiel — tous les caractères vus
        // doivent être des `tchar` (un préfixe de méthode valide). La ligne
        // vide (`\r\n\r\n`) n'est pas un préfixe : `1*tchar` exige au moins
        // un `tchar` — une ligne vide n'est jamais une requête (voir le
        // commentaire de la fonction).
        None => !line.is_empty() && line.iter().all(|&b| is_tchar(b)),
    }
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
pub(super) fn is_valid_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(is_tchar)
}

/// [`is_valid_token`] sur des octets — pour les chemins où la chaîne n'existe
/// pas encore : aucune conversion UTF-8 (donc aucune allocation) n'est faite
/// pour un simple verdict.
fn is_valid_token_bytes(octets: &[u8]) -> bool {
    !octets.is_empty() && octets.iter().copied().all(is_tchar)
}

/// Vrai si `target` est une cible de requête `origin-form` valide
/// (`absolute-path [ "?" query ]`, RFC 9112 §3.2.1), en ASCII visible et
/// sans fragment `#`.
pub(super) fn is_valid_request_target(target: &str) -> bool {
    target.starts_with('/')
        && target
            .bytes()
            .all(|b| (0x21..=0x7e).contains(&b) && b != b'#')
}

/// Retire uniquement l'OWS HTTP (`SP` 0x20 et `HTAB` 0x09 — RFC 9110 §5.6.3).
///
/// Contrairement à `str::trim()`, ne retire ni `\x0b` (VT), ni `\x0c` (FF),
/// ni `\r`/`\n`, ni les espaces Unicode (`\u{00a0}`, etc.).
pub(super) fn trim_ows(s: &str) -> &str {
    s.trim_matches([' ', '\t'])
}

/// Vrai si la valeur d'en-tête respecte RFC 9110 §5.5 (aucun caractère de
/// contrôle C0/C1 ni DEL `0x7F`, seul `HTAB` est admis avec les octets
/// visibles).
pub(super) fn is_valid_field_value(s: &str) -> bool {
    !s.bytes().any(|b| (b < 0x20 && b != b'\t') || b == 0x7f)
        && !s.chars().any(|c| c != '\t' && c.is_control())
}

/// Vrai si `Content-Type` désigne `application/json` (sans second type
/// séparé par une virgule, et avec `charset=utf-8` si le paramètre `charset`
/// est présent).
pub(super) fn is_json_content_type(value: &str) -> bool {
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
pub(super) fn parse_expect_100_continue(value: &str) -> bool {
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
pub(super) fn header_has_media_type(value: &str, media_type: &str) -> bool {
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

/// Vrai si la valeur de poids `q` désigne **zéro**.
///
/// RFC 9110 §12.5.1 : `qvalue = "0" [ "." 1*3DIGIT ]`. On reconnaît comme
/// nul toute valeur dont les décimales sont **toutes** des zéros — y compris
/// au-delà des trois chiffres valides (`0.0000`, syntaxiquement malformé) :
/// l'intention du client y est non ambiguë, et la traiter comme non nulle
/// ferait diverger le verdict d'un proxy strict qui lirait zéro. Les autres
/// formes malformées (`00`, `0.5.0`…) restent **non nulles** : ce ne sont
/// pas des `qvalue` valides, et les traiter comme zéro serait trop permissif.
fn is_zero_qvalue(v: &str) -> bool {
    match v.strip_prefix('0') {
        Some("") => true,
        Some(rest) => rest
            .strip_prefix('.')
            .is_some_and(|digits| digits.bytes().all(|b| b == b'0')),
        None => false,
    }
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

    /// [`is_valid_token_bytes`] rend le **même verdict** que
    /// [`is_valid_token`] sur toute entrée ASCII, et refuse — sans allouer ni
    /// remplacer d'octet — un jeton non ASCII ou non UTF-8, qu'un
    /// `from_utf8_lossy` aurait transformé en `U+FFFD` avant de le refuser.
    #[test]
    fn validite_de_jeton_sur_octets_sans_allocation() {
        // Cas vide, cas nominal, cas limite (tchar exact / hors tchar).
        assert!(!is_valid_token_bytes(b""));
        assert!(is_valid_token_bytes(b"GET"));
        assert!(is_valid_token_bytes(b"X-Custom_Header!#$%&'*+-.^`|~123"));
        assert!(!is_valid_token_bytes(b"GE T"));
        assert!(!is_valid_token_bytes(b"Host\t"));
        assert!(!is_valid_token_bytes(b"Host\x00Name"));
        assert!(!is_valid_token_bytes(b"Host\x7f"));
        // Non ASCII et séquences invalides : refusés **octet par octet**.
        assert!(!is_valid_token_bytes("Été".as_bytes()));
        assert!(!is_valid_token_bytes(&[0x47, 0x45, 0xff, 0xfe]));
        // Équivalence avec la version `&str` sur les entrées ASCII valides.
        for jeton in ["GET", "POST", "DELETE", "M-SEARCH", "PRI"] {
            assert_eq!(
                is_valid_token(jeton),
                is_valid_token_bytes(jeton.as_bytes()),
                "verdict divergent pour `{jeton}`"
            );
        }
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
    fn zero_qvalue_accepts_any_run_of_zero_decimals() {
        // Toutes les décimales nulles = poids nul, y compris au-delà des 3
        // chiffres valides de la RFC (0.0000 est malformé mais sans ambiguïté).
        for zero in ["0", "0.", "0.0", "0.00", "0.000", "0.0000", "0.000000"] {
            assert!(is_zero_qvalue(zero), "{zero} doit être un poids nul");
        }
        for nonzero in ["1", "0.1", "0.01", "0.0001", "00", "0.5", "0.250"] {
            assert!(!is_zero_qvalue(nonzero), "{nonzero} n'est pas un poids nul");
        }
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
        let mut reader = BufReader::new(Conn::Plain(server));
        let mut out = String::new();
        let mut scratch = Vec::new();
        let res = read_line_capped(
            &mut reader,
            &mut out,
            &mut scratch,
            64,
            Duration::from_millis(300),
            Some(Instant::now() + Duration::from_secs(2)),
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
            Some(Instant::now() + Duration::from_secs(2)),
        );
        assert!(matches!(res, Err(LineFail::TooLong)));

        // 3. LF nu sans CR -> MalformedEnding (lisible, donc répondue 400).
        let (mut client, server) = connected_pair();
        client.write_all(b"GET /health HTTP/1.1\n").unwrap();
        let mut reader = BufReader::new(Conn::Plain(server));
        let res = read_line_capped(
            &mut reader,
            &mut out,
            &mut scratch,
            64,
            Duration::from_millis(300),
            Some(Instant::now() + Duration::from_secs(2)),
        );
        assert!(matches!(res, Err(LineFail::MalformedEnding)));

        // 4. UTF-8 invalide -> Closed.
        let (mut client, server) = connected_pair();
        client.write_all(b"GET /\xff\xfe HTTP/1.1\r\n").unwrap();
        let mut reader = BufReader::new(Conn::Plain(server));
        let res = read_line_capped(
            &mut reader,
            &mut out,
            &mut scratch,
            64,
            Duration::from_millis(300),
            Some(Instant::now() + Duration::from_secs(2)),
        );
        assert!(matches!(res, Err(LineFail::Closed)));

        // 5. Deadline dépassée -> Slow.
        let (_client, server) = connected_pair();
        let mut reader = BufReader::new(Conn::Plain(server));
        let res = read_line_capped(
            &mut reader,
            &mut out,
            &mut scratch,
            64,
            Duration::from_secs(10),
            Some(Instant::now() + Duration::from_millis(40)),
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

        let mut reader = BufReader::new(Conn::Plain(server));
        let mut body = Vec::new();
        read_body(
            &mut reader,
            &mut body,
            BODY_CHUNK + 1024,
            Duration::from_millis(300),
            Some(Instant::now() + Duration::from_secs(2)),
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
            Some(Instant::now() + Duration::from_secs(2)),
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
            Some(Instant::now() + Duration::from_secs(2)),
        )
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }
}
