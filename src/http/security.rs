//! Gardes d'en-têtes : `Host` confronté au listener (anti-DNS-rebinding),
//! syntaxe `Origin`, et préflight CORS.

use super::response::ecrire_tous_vecteurs;
use super::*;

/// Réponse de **préflight CORS** (`OPTIONS` sur la route MCP) — `204 No
/// Content` sans corps ni `Content-Length` (RFC 9110 §8.6 interdit ce champ
/// sur un `204`), l'origine autorisée **echoée** (jamais `*` : la liste
/// `allow_origins` est un vote par origine, pas un wildcard) et `Vary:
/// Origin` pour que les caches ne servent pas la réponse d'une autre origine.
pub(super) fn respond_preflight(
    stream: &mut Conn,
    version: &str,
    origin: &str,
    keep_alive: bool,
    write_budget: Duration,
) -> std::io::Result<()> {
    let mut tete = String::with_capacity(256 + origin.len());
    let connection = if keep_alive { "keep-alive" } else { "close" };
    write!(
        tete,
        "{version} 204 No Content\r\n\
         Access-Control-Allow-Origin: {origin}\r\n\
         Access-Control-Allow-Methods: POST, OPTIONS\r\n\
         Access-Control-Allow-Headers: Content-Type, Accept, MCP-Protocol-Version, Mcp-Method, Mcp-Name\r\n\
         Access-Control-Max-Age: 600\r\n\
         Vary: Origin\r\n\
         Connection: {connection}\r\n\r\n"
    )
    .expect("écriture de tête dans String : infaillible");
    let mut vecteurs = [IoSlice::new(tete.as_bytes())];
    ecrire_tous_vecteurs(
        stream,
        &mut vecteurs,
        Instant::now().checked_add(write_budget),
    )?;
    stream.flush()
}

/// Vrai si l'en-tête `Host` est syntaxiquement valide (`uri-host [ ":" port ]`,
/// RFC 9112 §3.2 / RFC 3986 §3.2.2) : non vide, sans userinfo (`@`), sans
/// chemin/requête/fragment (`/`, `\`, `?`, `#`), sans espace, et avec un
/// port `u16` valide s'il est présent.
pub(super) fn is_valid_host(value: &str) -> bool {
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

/// Sépare la partie **hôte** d'une valeur `Host` déjà validée par
/// [`is_valid_host`] de son port éventuel — les crochets d'un littéral IPv6
/// sont retirés (`[::1]:8080` → `("::1", Some("8080"))`).
fn host_authority_parts(value: &str) -> (&str, Option<&str>) {
    if let Some(rest) = value.strip_prefix('[') {
        return match rest.split_once(']') {
            Some((ipv6, after)) => (ipv6, after.strip_prefix(':')),
            None => (value, None),
        };
    }
    match value.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (value, None),
    }
}

/// Forme canonique d'une IP : une adresse IPv4 **mappée** en IPv6
/// (`::ffff:a.b.c.d`) redevient l'IPv4 qu'elle désigne.
fn ip_canonique(ip: std::net::IpAddr) -> std::net::IpAddr {
    match ip {
        std::net::IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map_or(std::net::IpAddr::V6(v6), std::net::IpAddr::V4),
        v4 => v4,
    }
}

/// Deux adresses désignent-elles la même interface, une fois la forme
/// **v4-mappée** normalisée ([`ip_canonique`]) ?
///
/// Sur un listener double pile (`[::]:port`), un client IPv4 fait rendre
/// `::ffff:a.b.c.d` à `stream.local_addr()` (la valeur `local` de
/// [`host_autorise`]) alors que `Host` porte le littéral `a.b.c.d` — ou
/// l'inverse. Sans normalisation, `Host: <ip de l'interface>:port` rendait
/// `421 Misdirected Request` sur une connexion parfaitement légitime.
fn meme_ip(gauche: std::net::IpAddr, droite: std::net::IpAddr) -> bool {
    ip_canonique(gauche) == ip_canonique(droite)
}

/// Un `Host` désigne-t-il **ce** serveur ? — garde anti-DNS-rebinding.
///
/// `autorises` ([`crate::ServerBuilder::allowed_hosts`]) **s'ajoute** aux
/// autorités locales : comparaison sans tenir compte de la casse sur
/// l'autorité complète ou sur la seule partie hôte (le port de l'autorité est
/// ignoré : c'est le nom qui décide de la résolution, donc de l'attaque).
///
/// Règle locale (toujours active, liste vide ou non) : seules les
/// **autorités locales** passent —
/// - littéral d'adresse de bouclage (`127.0.0.0/8`, `::1`), avec ou sans port ;
/// - adresse IP effective du listener (`local`), comparée après normalisation
///   des formes IPv4 mappées en IPv6 ([`meme_ip`]) ;
/// - nom **court non qualifié** (aucun point : `localhost`, `mon-service`).
///
/// Est refusé tout nom pleinement qualifié (`mcp.exemple.com`) et tout
/// littéral d'adresse étranger (`10.1.2.3`) → `421 Misdirected Request`. La
/// règle s'appuie sur ce que le DNS rebinding exige : un nom résolvable
/// depuis l'internet public, donc toujours qualifié (au moins un point) — un
/// nom court ne peut pas être détourné par un enregistrement DNS public.
pub(super) fn host_autorise(value: &str, autorises: &[String], local: Option<SocketAddr>) -> bool {
    let (hote, _port) = host_authority_parts(value);
    // Hôtes déclarés (reverse proxy, nom public) : additifs.
    if autorises.iter().any(|autorise| {
        value.eq_ignore_ascii_case(autorise)
            || hote.eq_ignore_ascii_case(autorise)
            || host_authority_parts(autorise).0.eq_ignore_ascii_case(hote)
    }) {
        return true;
    }
    if let Ok(ip) = hote.parse::<std::net::IpAddr>() {
        // Normalisation avant **tout** verdict : `[::ffff:127.0.0.1]` est le
        // littéral de bouclage, et `::ffff:a.b.c.d` l'interface `a.b.c.d`.
        let ip = ip_canonique(ip);
        return ip.is_loopback() || local.is_some_and(|addr| meme_ip(addr.ip(), ip));
    }
    !hote.contains('.')
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
pub(super) fn is_valid_origin_syntax(value: &str) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Matrice de la règle par défaut : autorités **locales** acceptées, nom
    /// pleinement qualifié et littéral d'adresse étranger refusés.
    #[test]
    fn host_autorise_par_defaut_ne_garde_que_les_autorites_locales() {
        let bind = SocketAddr::from(([127, 0, 0, 1], 23080));
        for accepte in [
            "127.0.0.1:23080",
            "localhost:23080",
            "localhost",
            "LOCALHOST:8080",
            "127.0.0.1",
            "127.0.0.1:1",
            "[::1]:23080",
            "::1",
            "mon-service",
            "test",
        ] {
            assert!(
                host_autorise(accepte, &[], Some(bind)),
                "{accepte:?} doit être accepté par défaut"
            );
        }
        for refuse in [
            "attaquant.example",
            "attaquant.example:443",
            "mcp.exemple.com",
            "10.1.2.3",
            "10.1.2.3:23080",
            "[2001:db8::1]:443",
        ] {
            assert!(
                !host_autorise(refuse, &[], Some(bind)),
                "{refuse:?} doit être refusé (421) par défaut"
            );
        }
        // L'adresse effective du listener est acceptée même non bouclée.
        let public = SocketAddr::from(([192, 0, 2, 10], 23080));
        assert!(host_autorise("192.0.2.10:23080", &[], Some(public)));
        assert!(!host_autorise("192.0.2.11", &[], Some(public)));
    }

    /// Liste explicite : **additive**, insensible à la casse, port de
    /// l'autorité ignoré côté `Host` comme côté entrée.
    #[test]
    fn host_autorise_liste_explicite_s_ajoute_aux_autorites_locales() {
        let liste = vec![
            "mcp.exemple.com".to_string(),
            "interne.exemple.com:8443".to_string(),
        ];
        let bind = SocketAddr::from(([127, 0, 0, 1], 23080));
        for accepte in [
            "mcp.exemple.com",
            "MCP.Exemple.com:443",
            "interne.exemple.com:8443",
            "localhost:23080",
            "127.0.0.1",
            "mon-service",
        ] {
            assert!(
                host_autorise(accepte, &liste, Some(bind)),
                "{accepte:?} doit être accepté"
            );
        }
        for refuse in ["autre.exemple.com", "evil.example", "10.1.2.3"] {
            assert!(
                !host_autorise(refuse, &liste, Some(bind)),
                "{refuse:?} n'est ni local ni déclaré"
            );
        }
    }

    /// A5 (contre-revue) : sur un listener double pile, un client IPv4 fait
    /// rendre `::ffff:a.b.c.d` à `stream.local_addr()` (la valeur passée en
    /// `local`), alors que `Host` porte le littéral IPv4 nu — ou l'inverse.
    /// Sans normalisation, `Host: <ip de l'interface>` rendait `421`.
    #[test]
    fn host_autorise_normalise_les_adresses_ipv4_mappees() {
        let mappe: SocketAddr = "[::ffff:192.0.2.10]:23080".parse().unwrap();
        assert!(
            host_autorise("192.0.2.10:23080", &[], Some(mappe)),
            "Host IPv4 nu face à un listener v4-mappé"
        );
        assert!(
            host_autorise("192.0.2.10", &[], Some(mappe)),
            "Host IPv4 nu sans port face à un listener v4-mappé"
        );
        // Sens inverse : listener IPv4, `Host` portant la forme mappée.
        let v4 = SocketAddr::from(([192, 0, 2, 10], 23080));
        assert!(
            host_autorise("[::ffff:192.0.2.10]:23080", &[], Some(v4)),
            "Host v4-mappé face à un listener IPv4"
        );
        // Une IP étrangère, mappée ou non, reste refusée.
        assert!(!host_autorise("10.1.2.3:23080", &[], Some(mappe)));
        assert!(!host_autorise("[::ffff:10.1.2.3]:23080", &[], Some(mappe)));
        // `[::ffff:127.0.0.1]` **est** le littéral de bouclage une fois
        // normalisé : il doit être accepté comme `127.0.0.1`.
        assert!(host_autorise(
            "[::ffff:127.0.0.1]:23080",
            &[],
            Some(SocketAddr::from(([127, 0, 0, 1], 23080)))
        ));
    }
}
