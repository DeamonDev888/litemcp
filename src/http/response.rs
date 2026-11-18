//! Écriture des réponses : framing exact, échéance globale d'écriture,
//! fermeture propre et drain borné.

use super::conn::EcritureBornee;
use super::parse::DeadlineRead;
use super::parse::{is_valid_field_value, is_valid_token, tighten_read_timeout};
use super::*;

/// Durée totale maximale du drain de [`shutdown_gracefully`] — échéance
/// **absolue**, calculée une seule fois à l’entrée de la fonction et jamais
/// réarmée par les lectures.
const DRAIN_DEADLINE: Duration = Duration::from_millis(50);

/// Plafond d’octets vidés par [`shutdown_gracefully`] — borne mémoire
/// indépendante de l’échéance temporelle.
const DRAIN_CAP_BYTES: usize = 64 * 1024;

/// Tranche maximale visée par **une** tentative d’écriture (voir
/// [`ecrire_tranches`]).
///
/// Un `send` sur un reliquat de plusieurs Mio ne rend la main qu’après avoir
/// **tout** accepté (mesuré : 8 388 934 octets en 1-2 ms, donc aucun point de
/// contrôle), ou reste bloqué jusqu’à son timeout en progressant au rythme du
/// lecteur. Dans les deux cas la boucle ne réévalue l’échéance qu’au retour de
/// l’appel : découper le reliquat borne la durée d’une tentative et garantit
/// un point de contrôle tous les `TRANCHE_ECRITURE` octets. Le cas nominal
/// (tête + corps dans une tranche) reste **un seul** `write_vectored`, sans
/// allocation et sans un octet de différence sur le fil.
const TRANCHE_ECRITURE: usize = 256 * 1024;

/// Budget minimal armable sur une tentative : une échéance plus courte est
/// déjà expirée (un `set_write_timeout` nul est refusé par le système, et
/// arrondi à zéro il vaudrait « infini » sur certaines plateformes).
const PAS_MIN_ECRITURE: Duration = Duration::from_millis(1);

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
/// Le vidage est **borné par une échéance absolue** de [`DRAIN_DEADLINE`]
/// (50 ms) — assez pour laisser arriver les octets déjà en vol (sans quoi
/// le `close()` émet un RST et le client perd la réponse — mesuré : 9/15
/// sans attente, 15/15 avec). Le timeout de lecture est réajusté à chaque
/// tour sur le temps **résiduel** (via [`tighten_read_timeout`]) : un client
/// qui dribble un octet toutes les 49 ms ne peut plus réarmer le délai —
/// chaque lecture repartait autrefois avec 50 ms pleins et retenait le
/// thread quasi indéfiniment. Le plafond [`DRAIN_CAP_BYTES`] (64 Kio) borne
/// la mémoire indépendamment du temps. Comme les refus sont écrits hors de
/// la boucle d'`accept`, cette attente ne retarde pas les accepts suivants.
pub(super) fn shutdown_gracefully(stream: &mut Conn) {
    stream.shutdown_write();
    drain_before_close(stream);
}

/// Fermeture propre d'une socket **nue** (refus 503 écrits hors session
/// TLS) : même drain borné que [`shutdown_gracefully`].
pub(super) fn shutdown_plain(stream: &mut TcpStream) {
    let _ = stream.shutdown(std::net::Shutdown::Write);
    drain_before_close(stream);
}

/// Vide le tampon de réception avant le `close()` — voir
/// [`shutdown_gracefully`].
fn drain_before_close<S: Read + DeadlineRead>(stream: &mut S) {
    // Échéance absolue, calculée une seule fois : les tours suivants ne
    // font que consommer le résiduel — jamais le réarmer. `checked_add` :
    // aucun débordement d'`Instant` ne peut paniquer (une durée utilisateur
    // démesurée rend `None`, donc aucune échéance — le timeout par opération
    // borne alors chaque lecture).
    let deadline = Instant::now().checked_add(DRAIN_DEADLINE);
    let mut sink = [0u8; 8192];
    let mut drained = 0usize;
    loop {
        // Timeout réajusté sur le résiduel ; `None` = échéance atteinte.
        if tighten_read_timeout(stream, DRAIN_DEADLINE, deadline).is_none() {
            break;
        }
        match stream.read(&mut sink) {
            Ok(0) => break,
            Ok(n) => {
                drained += n;
                if drained >= DRAIN_CAP_BYTES {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

/// Réponse d'erreur : la connexion est **toujours fermée** — `Connection:
/// close` explicite + `shutdown(Write)`, jamais réutilisée. On ne draine pas
/// le corps restant : le drain nourrirait un client qui inonde (le 413 ferme
/// net), au prix possible d'un RST côté client avant lecture de la réponse —
/// dans les deux cas, aucun octet résiduel ne peut être réinterprété comme
/// une requête suivante.
pub(super) fn respond_error(
    stream: &mut Conn,
    status: &str,
    message: &str,
    extra_headers: &str,
    head: bool,
    http10: bool,
) -> std::io::Result<()> {
    let payload: &[u8] = &error_payload(message);
    // La ligne de statut reflète la version de la requête, comme les
    // réponses normales : un client HTTP/1.0 strict refuse une réponse
    // annoncée `HTTP/1.1` (RFC 9112 §2.3).
    let version = if http10 { "HTTP/1.0" } else { "HTTP/1.1" };
    respond_version(
        stream,
        &ResponseHead {
            version,
            status,
            content_type: "application/json",
        },
        extra_headers,
        payload,
        false,
        head,
        // Corps d'erreur court (< 1 Kio, tient dans le tampon d'envoi du
        // noyau) : le budget minimal suffit à borner un client qui ne lit
        // rien, sans dépendre de `read_timeout` ici.
        WRITE_DEADLINE_MIN,
    )?;
    // Fermeture propre systématique : on envoie un FIN **et** on vide le
    // tampon de réception avant le close — sinon le noyau émet un RST avec
    // des octets non lus, RST qui purge chez le client la réponse qu'on
    // vient d'écrire (404/415/431 perdus quand la requête n'a pas été
    // intégralement consommée).
    shutdown_gracefully(stream);
    Ok(())
}

/// Réponse `413` **sans drain** du corps restant.
///
/// Le drain de [`respond_error`] est borné à 64 Kio / 50 ms : au-delà, le
/// tampon de réception garde des octets et le `close()` émet un RST qui
/// purge la réponse chez le client (le 413 est perdu). Ici, on écrit la
/// réponse puis on envoie **seulement** un FIN (`shutdown(Write)`) : le
/// client lit le 413, et les octets résiduels du corps sont abandonnés —
/// la connexion n'est de toute façon pas réutilisable (le cadrage de la
/// requête est rompu).
pub(super) fn respond_413_without_drain(
    stream: &mut Conn,
    status: &str,
    message: &str,
    head: bool,
    http10: bool,
) -> std::io::Result<()> {
    let payload: &[u8] = &error_payload(message);
    let version = if http10 { "HTTP/1.0" } else { "HTTP/1.1" };
    respond_version(
        stream,
        &ResponseHead {
            version,
            status,
            content_type: "application/json",
        },
        "",
        payload,
        false,
        head,
        WRITE_DEADLINE_MIN,
    )?;
    stream.shutdown_write();
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

/// Répond un rejet émis par un hook de requête — voir
/// [`crate::ServerBuilder::on_request`].
///
/// En-têtes supplémentaires **assainis au départ** : un nom hors jeton
/// RFC 9110 ou une valeur portant un contrôle (CRLF inclus) est écarté —
/// le rejet ne doit jamais devenir une injection d'en-têtes. Connexion
/// **toujours fermée** (le corps de la requête n'a pas été lu), même
/// traitement que [`respond_error`].
pub(super) fn respond_rejection(
    stream: &mut Conn,
    rejection: &RequestRejection,
    head: bool,
    http10: bool,
) -> std::io::Result<()> {
    let mut extra_headers = String::new();
    for (name, value) in rejection.headers() {
        if is_valid_token(name) && is_valid_field_value(value) {
            extra_headers.push_str(name);
            extra_headers.push_str(": ");
            extra_headers.push_str(value);
            extra_headers.push_str("\r\n");
        }
    }
    let status_line = format!(
        "{} {}",
        rejection.status(),
        reason_phrase(rejection.status())
    );
    let payload = error_payload(&rejection.message);
    let version = if http10 { "HTTP/1.0" } else { "HTTP/1.1" };
    respond_version(
        stream,
        &ResponseHead {
            version,
            status: &status_line,
            content_type: "application/json",
        },
        &extra_headers,
        &payload,
        false,
        head,
        WRITE_DEADLINE_MIN,
    )?;
    shutdown_gracefully(stream);
    Ok(())
}

/// Phrase de raison des statuts qu'un rejet de hook peut porter — les
/// autres reçoivent un libellé générique (la ligne de statut exige un
/// jeton ; le code numérique reste la référence pour le client).
fn reason_phrase(code: u16) -> &'static str {
    match code {
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        407 => "Proxy Authentication Required",
        408 => "Request Timeout",
        409 => "Conflict",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        418 => "I'm a teapot",
        421 => "Misdirected Request",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Rejected",
    }
}

/// En-tête de réponse — version HTTP, statut et type de contenu regroupés
/// pour éviter une signature à huit arguments.
pub(super) struct ResponseHead<'a> {
    /// Version annoncée dans la ligne de statut (`HTTP/1.0` ou `HTTP/1.1`).
    pub(super) version: &'a str,
    /// Ligne de statut (`"200 OK"`, `"400 Bad Request"`…).
    pub(super) status: &'a str,
    /// Type de contenu du corps.
    pub(super) content_type: &'a str,
}

/// Écrit la réponse complète : la **ligne de statut** reflète la version
/// HTTP de la requête (`HTTP/1.0` → `HTTP/1.0 200 OK`). Un client HTTP/1.0
/// strict peut refuser une réponse annoncée en `HTTP/1.1` ; la RFC 9112
/// §2.3 impose au récepteur HTTP/1.1 d'accepter `HTTP/1.0`, mais l'inverse
/// n'est pas vrai.
///
/// `write_budget` est l'échéance **globale** d'écriture (voir
/// [`write_deadline`]) : elle borne l'envoi complet, pas seulement un
/// `write_vectored`. Chaque tentative est bornée par le budget restant (voir
/// [`ecrire_borne`]) : un client qui lit au compte-gouttes est coupé, même
/// lorsque son premier `send` ne rend pas la main.
pub(super) fn respond_version<W: EcritureBornee>(
    stream: &mut W,
    head_meta: &ResponseHead<'_>,
    extra_headers: &str,
    body: &[u8],
    keep_alive: bool,
    head: bool,
    write_budget: Duration,
) -> std::io::Result<()> {
    let ResponseHead {
        version,
        status,
        content_type,
    } = *head_meta;
    let connection = if keep_alive { "keep-alive" } else { "close" };
    // Tête et corps partent dans **un seul syscall** (`write_vectored`) :
    // avec nodelay, ils partent dans le même segment TCP (un syscall et un
    // paquet, comme l'assemblage en un seul tampon) — mais le corps n'est
    // **jamais recopié** dans un tampon d'assemblage : une copie pleine
    // charge de gagnée par réponse (à 1 Mio, ~1 Mio d'allocations et de
    // memcpy en moins). Les octets fil sont identiques : même suite d'octets,
    // seul le vecteur d'écriture change.
    //
    // La tête est pré-réservée en une allocation (même convention que
    // l'assemblage historique : `96 + longueurs`) — sans elle, le `format!`
    // croîtrait par doublement et alourdirait le chemin chaud.
    let mut tete = String::with_capacity(
        96 + version.len() + status.len() + extra_headers.len() + content_type.len(),
    );
    write!(
        tete,
        "{version} {status}\r\n{extra_headers}Content-Type: {content_type}\r\nContent-Length: {}\r\nConnection: {connection}\r\n\r\n",
        body.len()
    )
    .expect("écriture de tête dans String : infaillible");
    let echeance = Instant::now().checked_add(write_budget);
    // Réponse à HEAD : en-têtes du GET équivalent, **sans corps** (RFC 9110
    // §9.3.2). Même échéance et même découpage que le corps : une tête seule
    // ne peut pas retenir le thread au-delà du budget.
    if head {
        let mut vecteurs = [IoSlice::new(tete.as_bytes())];
        ecrire_borne(stream, &mut vecteurs, echeance)?;
    } else {
        let mut vecteurs = [IoSlice::new(tete.as_bytes()), IoSlice::new(body)];
        ecrire_borne(stream, &mut vecteurs, echeance)?;
    }
    stream.flush()
}

/// Écrit **tous** les octets des iovecs en bouclant sur `write_vectored`
/// (`write_all_vectored` n'est pas encore stable — rust-lang/rust#70436).
///
/// Variante **sans borne par tentative** : réservée aux réponses dont le
/// corps tient dans le tampon d'envoi (refus, erreurs, préflight) — la boucle
/// y contrôle quand même l'échéance entre deux appels. Les réponses dont le
/// corps peut être volumineux passent par [`ecrire_borne`], qui arme en plus
/// le timeout de la socket avant **chaque** tentative.
pub(super) fn ecrire_tous_vecteurs(
    stream: &mut impl Write,
    bufs: &mut [IoSlice<'_>],
    deadline: Option<Instant>,
) -> std::io::Result<()> {
    ecrire_tranches(stream, bufs, deadline, |_, _| Ok(()))
}

/// Écriture **bornée par tentative** : le timeout de la socket est réarmé sur
/// le budget restant avant chaque `write_vectored`.
///
/// C'est ce qui rend l'échéance globale effective **dès qu'un envoi ne rend
/// pas la main** : le `send` attend la place dans la fenêtre du pair, donc la
/// boucle n'évalue l'échéance qu'à son retour. Le timeout armé ici coupe cet
/// appel ; l'erreur remonte et l'appelant ferme la connexion. Quand le noyau
/// absorbe au contraire tout le corps d'un coup (petite réponse, tampons
/// autotunés), l'écriture est terminée avant l'échéance : il n'y a plus rien à
/// couper, et c'est le découpage en tranches (voir [`TRANCHE_ECRITURE`]) qui
/// empêche un seul appel d'absorber ainsi un corps entier.
pub(super) fn ecrire_borne<W: EcritureBornee>(
    stream: &mut W,
    bufs: &mut [IoSlice<'_>],
    deadline: Option<Instant>,
) -> std::io::Result<()> {
    ecrire_tranches(stream, bufs, deadline, |flux, budget| {
        flux.borner_prochain_envoi(budget)
    })
}

/// Boucle d'écriture commune : avance les iovecs, découpe le reliquat en
/// tranches et réévalue l'échéance avant chaque tentative.
///
/// `armer` reçoit le budget de la tentative suivante (voir [`ecrire_borne`]).
/// Le cas nominal — tête et corps tenant dans [`TRANCHE_ECRITURE`] — reste
/// **un seul** `write_vectored`, sans allocation et sans un octet de
/// différence sur le fil : le découpage ne s'applique qu'au-delà.
fn ecrire_tranches<W: Write + ?Sized>(
    stream: &mut W,
    mut bufs: &mut [IoSlice<'_>],
    deadline: Option<Instant>,
    mut armer: impl FnMut(&mut W, Duration) -> std::io::Result<()>,
) -> std::io::Result<()> {
    // Les iovecs vides de tête ne comptent pas comme une tentative : sans ce
    // nettoyage, le découpage ci-dessous écrirait zéro octet et rendrait
    // `WriteZero` sur une réponse valide.
    while bufs.first().is_some_and(|premier| premier.is_empty()) {
        bufs = &mut bufs[1..];
    }
    loop {
        // Écriture terminée : succès, même si l'échéance vient de tomber —
        // tous les octets sont partis.
        if bufs.is_empty() {
            return Ok(());
        }
        // Budget de **cette** tentative : min(WRITE_TIMEOUT, temps restant).
        // Sans échéance (budget infini côté appelant), `WRITE_TIMEOUT` reste
        // la seule borne — comportement historique.
        let budget = match deadline {
            None => WRITE_TIMEOUT,
            Some(echeance) => {
                let restant = echeance.saturating_duration_since(Instant::now());
                if restant < PAS_MIN_ECRITURE {
                    return Err(echeance_depassee());
                }
                restant.min(WRITE_TIMEOUT)
            }
        };
        armer(stream, budget)?;
        // Point de contrôle : une seule tentative ne peut pas absorber plus
        // d'une tranche, donc l'échéance est réévaluée au moins tous les
        // `TRANCHE_ECRITURE` octets — indépendamment du comportement du
        // timeout de la socket (rustls, sémantique partielle d'Unix).
        let envoi = if longueur_totale(bufs) <= TRANCHE_ECRITURE {
            stream.write_vectored(bufs)
        } else {
            let n = bufs[0].len().min(TRANCHE_ECRITURE);
            let tranche = [IoSlice::new(&bufs[0][..n])];
            stream.write_vectored(&tranche)
        };
        match envoi {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "failed to write whole buffer",
                ));
            }
            Ok(n) => IoSlice::advance_slices(&mut bufs, n),
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            // Expiration d'une tentative bornée **sans qu'un octet n'ait été
            // accepté** (Unix rend `EAGAIN` dans ce cas) : rien n'est perdu,
            // le tour suivant tranche sur l'échéance — le budget restant
            // décroît, donc la boucle termine.
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock && deadline.is_some() => {}
            // `TimedOut` (Windows) est l'échéance : on ne réessaie pas, un
            // envoi expiré peut avoir transmis un nombre d'octets inconnu
            // (réessayer dupliquerait des octets du corps).
            Err(e) => return Err(e),
        }
    }
}

/// Longueur totale des iovecs, sans débordement (une somme saturante reste
/// au-dessus de [`TRANCHE_ECRITURE`] et déclenche donc le découpage).
fn longueur_totale(bufs: &[IoSlice<'_>]) -> usize {
    bufs.iter()
        .fold(0usize, |total, buf| total.saturating_add(buf.len()))
}

/// Erreur d'échéance d'écriture — `TimedOut`, que l'appelant traite comme une
/// connexion à fermer.
fn echeance_depassee() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "response write deadline exceeded",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn respond_writes_exact_framing_for_get_and_head() {
        let mut get_buf = Vec::new();
        let get_head = ResponseHead {
            version: "HTTP/1.1",
            status: "200 OK",
            content_type: "text/plain",
        };
        respond_version(
            &mut get_buf,
            &get_head,
            "",
            b"ok",
            true,
            false,
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(
            get_buf,
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nok"
        );

        let mut head_buf = Vec::new();
        let head_head = ResponseHead {
            version: "HTTP/1.1",
            status: "200 OK",
            content_type: "text/plain",
        };
        respond_version(
            &mut head_buf,
            &head_head,
            "",
            b"ok",
            false,
            true,
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(
            head_buf,
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\n"
        );
    }

    /// Écrivain qui n'accepte qu'un octet par appel, avec 1 ms d'attente —
    /// modélise un client qui lit au compte-gouttes **sans** dépendre de la
    /// taille des tampons noyaux (un 4 Mio tiendrait dans le tampon TCP de
    /// loopback et l'écriture se terminerait avant l'échéance).
    #[derive(Default)]
    struct GoutteAEcriture {
        octets: usize,
    }

    impl Write for GoutteAEcriture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            std::thread::sleep(Duration::from_millis(1));
            self.octets += 1;
            Ok(1)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl EcritureBornee for GoutteAEcriture {
        fn borner_prochain_envoi(&mut self, _budget: Duration) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl EcritureBornee for Vec<u8> {
        fn borner_prochain_envoi(&mut self, _budget: Duration) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// L'échéance globale coupe un client qui lit au compte-gouttes — sans
    /// jamais dépendre de `WRITE_TIMEOUT` (qui ne borne qu'un
    /// `write_vectored`) : aucun appel individuel n'échoue, c'est bien le
    /// budget total qui tranche.
    #[test]
    fn ecrire_tous_vecteurs_respecte_l_echeance_globale() {
        let charge = vec![b'x'; 4096];
        let mut ecrivain = GoutteAEcriture::default();
        let mut vecteurs = [IoSlice::new(&charge)];
        let debut = Instant::now();
        let resultat = ecrire_tous_vecteurs(
            &mut ecrivain,
            &mut vecteurs,
            Instant::now().checked_add(Duration::from_millis(200)),
        );
        let ecoule = debut.elapsed();
        assert!(
            matches!(&resultat, Err(error) if error.kind() == std::io::ErrorKind::TimedOut),
            "écriture non interrompue à l'échéance : {resultat:?}"
        );
        assert!(
            ecoule >= Duration::from_millis(150) && ecoule < Duration::from_secs(5),
            "échéance globale non respectée : {ecoule:?}"
        );
        assert!(
            ecrivain.octets < charge.len(),
            "toute la charge a été écrite : {}",
            ecrivain.octets
        );
    }

    /// Un plafond **relevé** laisse passer une écriture lente que le défaut
    /// coupe : même écrivain au compte-gouttes (1 octet / ms), même charge.
    #[test]
    fn ecrire_tous_vecteurs_plafond_releve_laisse_passer_une_ecriture_lente() {
        let charge = vec![b'x'; 1536];

        // Défaut (borne basse 1 s) : coupé au tiers de la charge.
        let mut coupe = GoutteAEcriture::default();
        let mut vecteurs = [IoSlice::new(&charge)];
        let resultat = ecrire_tous_vecteurs(
            &mut coupe,
            &mut vecteurs,
            Instant::now().checked_add(WRITE_DEADLINE_MIN),
        );
        assert!(
            matches!(&resultat, Err(error) if error.kind() == std::io::ErrorKind::TimedOut),
            "échéance par défaut non appliquée : {resultat:?}"
        );
        assert!(
            coupe.octets < charge.len(),
            "toute la charge a été écrite malgré l'échéance : {}",
            coupe.octets
        );

        // Plafond relevé (celui de `.write_deadline(10 s)`) : tout passe.
        let mut lent = GoutteAEcriture::default();
        let mut vecteurs = [IoSlice::new(&charge)];
        ecrire_tous_vecteurs(
            &mut lent,
            &mut vecteurs,
            Instant::now().checked_add(Duration::from_secs(10)),
        )
        .expect("plafond relevé : écriture complète attendue");
        assert_eq!(lent.octets, charge.len());
    }

    /// Échéance absente (`None`) : le comportement historique est conservé.
    #[test]
    fn ecrire_tous_vecteurs_sans_echeance_ecrit_tout() {
        let mut tampon = Vec::new();
        let mut vecteurs = [IoSlice::new(b"tete"), IoSlice::new(b"corps")];
        ecrire_tous_vecteurs(&mut tampon, &mut vecteurs, None).unwrap();
        assert_eq!(tampon, b"tetecorps");
    }

    /// Écrivain qui se comporte comme une **socket bloquante** : un appel
    /// `write_vectored` dure exactement la borne qu'on lui a armée et rend
    /// `TimedOut` (`SO_SNDTIMEO` de Windows) ; sans borne armée, il bloque
    /// `sans_borne` puis accepte un octet — c'est le comportement qui faisait
    /// passer l'ancien test : l'appel ne rendait la main qu'après son propre
    /// timeout, jamais à l'échéance globale.
    struct EnvoiQuiExpire {
        borne: Option<Duration>,
        sans_borne: Duration,
        appels: usize,
        armes: Vec<Duration>,
    }

    impl EnvoiQuiExpire {
        fn nouveau(sans_borne: Duration) -> Self {
            Self {
                borne: None,
                sans_borne,
                appels: 0,
                armes: Vec::new(),
            }
        }
    }

    impl Write for EnvoiQuiExpire {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.write_vectored(&[IoSlice::new(buf)])
        }

        fn write_vectored(&mut self, _bufs: &[IoSlice<'_>]) -> std::io::Result<usize> {
            self.appels += 1;
            match self.borne {
                Some(borne) => {
                    std::thread::sleep(borne);
                    Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "borne d'envoi atteinte",
                    ))
                }
                None => {
                    std::thread::sleep(self.sans_borne);
                    Ok(1)
                }
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl EcritureBornee for EnvoiQuiExpire {
        fn borner_prochain_envoi(&mut self, budget: Duration) -> std::io::Result<()> {
            self.armes.push(budget);
            self.borne = Some(budget);
            Ok(())
        }
    }

    /// Écrivain qui accepte tout, en mémoire : compte les appels, la taille
    /// de chacun et conserve les octets reçus (pour vérifier qu'aucun ne
    /// change).
    #[derive(Default)]
    struct CompteurEnvois {
        appels: usize,
        par_appel: Vec<usize>,
        armes: Vec<Duration>,
        recu: Vec<u8>,
    }

    impl Write for CompteurEnvois {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.appels += 1;
            self.par_appel.push(buf.len());
            self.recu.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> std::io::Result<usize> {
            let mut total = 0usize;
            for buf in bufs {
                self.recu.extend_from_slice(buf);
                total += buf.len();
            }
            self.appels += 1;
            self.par_appel.push(total);
            Ok(total)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl EcritureBornee for CompteurEnvois {
        fn borner_prochain_envoi(&mut self, budget: Duration) -> std::io::Result<()> {
            self.armes.push(budget);
            Ok(())
        }
    }

    /// Cas qui manquait : **un seul** `write_vectored` bloque plus longtemps
    /// que le budget global. L'échéance n'était testée qu'**entre** deux
    /// appels : si le premier appel ne rend pas la main (client dont la
    /// fenêtre TCP est fermée, corps de plusieurs Mio), l'échéance n'était
    /// jamais évaluée. Ici, sans borne par tentative, l'écriture dure
    /// `sans_borne` (5 s) pour un budget de 300 ms.
    #[test]
    fn un_seul_envoi_bloque_est_coupe_a_l_echeance_globale() {
        let charge = vec![b'x'; 4096];
        let mut ecrivain = EnvoiQuiExpire::nouveau(Duration::from_secs(5));
        let mut vecteurs = [IoSlice::new(&charge)];
        let budget = Duration::from_millis(300);
        let debut = Instant::now();
        let resultat = ecrire_borne(
            &mut ecrivain,
            &mut vecteurs,
            Instant::now().checked_add(budget),
        );
        let ecoule = debut.elapsed();
        assert!(
            matches!(&resultat, Err(error) if error.kind() == std::io::ErrorKind::TimedOut),
            "un envoi bloqué doit rendre l'échéance : {resultat:?}"
        );
        assert!(
            ecoule < Duration::from_secs(2),
            "la tentative a bloqué {ecoule:?} : elle n'a pas été bornée par le budget ({budget:?})"
        );
        assert_eq!(ecrivain.appels, 1, "une seule tentative doit avoir eu lieu");
        assert!(
            !ecrivain.armes.is_empty(),
            "aucune borne armée avant l'envoi"
        );
        assert!(
            ecrivain.armes.iter().all(|borne| *borne <= budget),
            "borne armée au-delà du budget restant : {:?}",
            ecrivain.armes
        );
    }

    /// Cas nominal : tête + corps tenant dans une tranche → **un seul**
    /// `write_vectored`, une seule borne armée, et les octets du fil sont
    /// exactement ceux de l'assemblage tête + corps.
    #[test]
    fn cas_nominal_tete_et_corps_en_un_seul_envoi() {
        let corps = vec![b'y'; 200 * 1024];
        let mut ecrivain = CompteurEnvois::default();
        respond_version(
            &mut ecrivain,
            &ResponseHead {
                version: "HTTP/1.1",
                status: "200 OK",
                content_type: "application/json",
            },
            "",
            &corps,
            true,
            false,
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(
            ecrivain.appels, 1,
            "le cas nominal doit rester un seul appel"
        );
        assert_eq!(ecrivain.armes.len(), 1, "une borne par tentative");
        assert!(ecrivain.armes[0] <= Duration::from_secs(1));
        let tete = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 204800\r\nConnection: keep-alive\r\n\r\n";
        assert_eq!(ecrivain.recu.len(), tete.len() + corps.len());
        assert_eq!(&ecrivain.recu[..tete.len()], tete.as_bytes());
        assert_eq!(&ecrivain.recu[tete.len()..], &corps[..]);
    }

    /// Reliquat plus gros qu'une tranche : plusieurs tentatives, **aucune**
    /// ne dépasse [`TRANCHE_ECRITURE`], et les octets reçus sont identiques à
    /// la charge (le découpage est invisible sur le fil).
    #[test]
    fn reliquat_decoupe_en_tranches_bornees() {
        let charge = vec![b'z'; TRANCHE_ECRITURE * 3 + 1024];
        let mut ecrivain = CompteurEnvois::default();
        let mut vecteurs = [IoSlice::new(&charge)];
        ecrire_borne(
            &mut ecrivain,
            &mut vecteurs,
            Instant::now().checked_add(Duration::from_secs(5)),
        )
        .unwrap();
        assert_eq!(
            ecrivain.appels, 4,
            "3 tranches pleines + le reliquat : {:?}",
            ecrivain.par_appel
        );
        assert!(
            ecrivain
                .par_appel
                .iter()
                .all(|taille| *taille <= TRANCHE_ECRITURE),
            "tentative au-delà d'une tranche : {:?}",
            ecrivain.par_appel
        );
        assert_eq!(ecrivain.recu.len(), charge.len());
        assert_eq!(ecrivain.recu, charge, "un octet a changé sur le fil");
    }
}
