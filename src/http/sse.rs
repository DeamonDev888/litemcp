//! Flux SSE request-scoped : événements, canal de progression et `id`
//! de requête extrait du corps.
//!
//! Le canal de progression est borné par le **budget d'écriture** du serveur
//! lié : le temps passé à écrire est décompté, et un client qui ne lit plus
//! épuise ce budget au lieu de retenir le fil du handler indéfiniment
//! (`WRITE_TIMEOUT` ne borne qu'un appel système). Ce n'est **pas** une
//! échéance de vie : un handler long qui progresse vers un client qui lit
//! garde son flux, sans quoi `no_tool_timeout()` deviendrait inutilisable.

use super::conn::EcritureBornee;
use super::response::{PAS_MIN_ECRITURE, echeance_depassee};
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
    /// Budget d'écriture **restant** pour ce flux : le budget global du
    /// serveur lié (`write_budget`), consommé par le **temps réellement
    /// passé à écrire**. Un client qui lit ne consomme presque rien (un
    /// événement part en un ou deux appels système), un client figé épuise le
    /// budget au rythme de ses `WRITE_TIMEOUT`. Le flux lui-même n'a donc
    /// **pas** d'échéance de vie : un handler long qui progresse garde son
    /// flux, ce qui est l'usage même de `notifications/progress`.
    budget: Duration,
    /// Borne actuellement armée sur la socket (`SO_SNDTIMEO`). Tant que le
    /// budget restant lui est supérieur (cas nominal : `WRITE_TIMEOUT`
    /// plafonne la borne), rien n'est réarmé ; dès qu'il passe en dessous,
    /// **chaque** tentative réarme — un `setsockopt` par écriture partielle,
    /// pas « aucun par événement ».
    arme: Option<Duration>,
}

impl ProgressWriter {
    /// Écrit `evenement` puis vide le tampon, borne réarmée avant **chaque**
    /// tentative (voir [`ecrire_budget`]). Le temps réellement passé dans ces
    /// appels est décompté du budget, même en cas d'échec. `closed` est posé
    /// dès qu'une étape échoue : aucune écriture ne suivra.
    fn ecrire(&mut self, evenement: &[u8]) -> std::io::Result<()> {
        let resultat = ecrire_budget(
            &mut self.stream,
            &mut self.budget,
            &mut self.arme,
            evenement,
        );
        if resultat.is_err() {
            self.closed = true;
        }
        resultat
    }

    /// Vide le tampon sans nouvel événement (fin de flux) : zéro octet à
    /// écrire, donc seule la borne puis le `flush` ont lieu — un tampon TLS
    /// bloquant reste borné par le budget restant, comme une écriture.
    fn vider(&mut self) -> std::io::Result<()> {
        self.ecrire(&[])
    }
}

/// Écrit `evenement` puis vide le tampon, en débitant le budget du temps
/// écoulé — y compris quand une tentative échoue.
///
/// `write_all` ne suffit pas : il boucle sur des écritures partielles
/// **réussies** en réutilisant la borne armée une seule fois, donc un client
/// lent peut cumuler N × `WRITE_TIMEOUT` alors que le budget restant est bien
/// plus court. Ici la borne est réarmée avant chaque `write` sur ce qui reste
/// (voir [`armer_borne`]) : le budget est **strict** par appel système, et le
/// commentaire « un envoi bloquant ne peut pas dépasser ce qui reste » devient
/// vrai.
fn ecrire_budget<W: Write + EcritureBornee>(
    flux: &mut W,
    budget: &mut Duration,
    arme: &mut Option<Duration>,
    evenement: &[u8],
) -> std::io::Result<()> {
    let debut = Instant::now();
    let resultat = ecrire_et_vider(flux, evenement, *budget, debut, arme);
    *budget = (*budget).saturating_sub(debut.elapsed());
    resultat
}

/// Boucle d'écriture explicite (remplace `write_all`), puis `flush` borné :
/// avant **chaque** appel système, la borne est armée sur le budget restant.
fn ecrire_et_vider<W: Write + EcritureBornee>(
    flux: &mut W,
    mut reste: &[u8],
    budget: Duration,
    debut: Instant,
    arme: &mut Option<Duration>,
) -> std::io::Result<()> {
    while !reste.is_empty() {
        armer_borne(flux, budget, debut, arme)?;
        match flux.write(reste) {
            // Même contrat que `write_all` : un `write` nul est une erreur.
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "failed to write whole buffer",
                ));
            }
            Ok(ecrits) => reste = &reste[ecrits..],
            // `write_all` réessayait `EINTR` : même contrat ici.
            Err(ref error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    armer_borne(flux, budget, debut, arme)?;
    flux.flush()
}

/// Arme `SO_SNDTIMEO` sur `min(budget restant, WRITE_TIMEOUT)` ; budget
/// épuisé → `TimedOut` sans toucher à la socket.
///
/// Le réarmement n'a lieu que si la borne doit **décroître** : tant que le
/// budget restant dépasse celle déjà posée (cas nominal, `WRITE_TIMEOUT`
/// plafonne), aucun `setsockopt` n'est émis. Dès qu'il passe en dessous,
/// chaque tentative réarme — c'est ce qui rend le budget strict.
fn armer_borne<W: EcritureBornee>(
    flux: &mut W,
    budget: Duration,
    debut: Instant,
    arme: &mut Option<Duration>,
) -> std::io::Result<()> {
    let Some(borne) = budget_restant(budget.saturating_sub(debut.elapsed())) else {
        return Err(echeance_depassee());
    };
    if arme.is_some_and(|posee| posee <= borne) {
        return Ok(());
    }
    flux.borner_prochain_envoi(borne)?;
    *arme = Some(borne);
    Ok(())
}

/// Budget d'une tentative d'écriture : le budget restant du flux, plafonné
/// par `WRITE_TIMEOUT` (borne historique d'un appel système), ou `None` quand
/// il est épuisé.
///
/// Même seuil et même plafond que la boucle d'écriture des réponses (voir
/// [`super::response::ecrire_tranches`]) : sous [`PAS_MIN_ECRITURE`]
/// restant, un `set_write_timeout` serait nul — refusé par le système, ou
/// arrondi à « infini » sur certaines plateformes — donc le budget est
/// déjà épuisé.
fn budget_restant(budget: Duration) -> Option<Duration> {
    let borne = budget.min(WRITE_TIMEOUT);
    (borne >= PAS_MIN_ECRITURE).then_some(borne)
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
///
/// Toute émission est en outre bornée par le **budget d'écriture** du flux
/// ([`Self::new`]) : ce budget ne compte que le temps passé à écrire, donc
/// un client figé l'épuise et se fait couper (canal fermé, jeton levé,
/// exactement comme un échec d'écriture), tandis qu'un handler légitimement
/// long qui progresse garde son flux. `WRITE_TIMEOUT` ne borne qu'un appel
/// système ; sans ce budget, un handler sous `no_tool_timeout()` pourrait
/// passer un temps arbitraire à écrire vers un client qui ne lit plus.
pub(super) struct SseProgressChannel {
    writer: Mutex<ProgressWriter>,
    cancel: CancelToken,
}

impl SseProgressChannel {
    /// Ouvre le canal sur une copie de la connexion. `budget` est le budget
    /// d'écriture du serveur lié (`write_budget`) : il n'est **pas** une
    /// échéance de vie, seulement le temps d'écriture cumulé que le flux peut
    /// consommer (voir [`ProgressWriter::budget`]).
    pub(super) fn new(
        stream: Conn,
        token: serde_json::Value,
        cancel: CancelToken,
        budget: Duration,
    ) -> Self {
        Self {
            writer: Mutex::new(ProgressWriter {
                stream,
                token,
                closed: false,
                budget,
                arme: None,
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
            Some(reply) => writer.ecrire(&format_sse_event(reply)),
            None => writer.vider(),
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
        if writer.ecrire(&format_sse_event(&payload)).is_err() {
            // Client parti **ou** budget d'écriture épuisé : plus aucune
            // écriture sur ce descripteur, et l'annulation de la requête est
            // signalée au handler.
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

    /// Paire de sockets loopback : le `Conn` du serveur et son pair client.
    /// `Conn` n'a pas de variante en mémoire (le canal écrit réellement sur
    /// une socket), donc les tests d'échéance en fabriquent une.
    fn paire_loopback() -> (Conn, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener loopback");
        let addr = listener.local_addr().expect("adresse locale");
        let client = TcpStream::connect(addr).expect("connexion loopback");
        let (serveur, _) = listener.accept().expect("acceptation loopback");
        (Conn::Plain(serveur), client)
    }

    /// Budget déjà épuisé : aucune émission n'est tentée, le canal est fermé
    /// et le jeton d'annulation levé — le handler sort à son prochain point
    /// sûr au lieu de retenir son fil.
    #[test]
    fn progress_refuse_le_budget_epuise() {
        let (conn, client) = paire_loopback();
        let cancel = CancelToken::new();
        let channel = SseProgressChannel::new(
            conn,
            serde_json::json!("jeton"),
            cancel.clone(),
            Duration::ZERO,
        );

        assert!(
            !channel.progress(1.0, Some(2.0), None),
            "budget épuisé : émission refusée"
        );
        assert!(
            cancel.is_cancelled(),
            "un refus pour budget épuisé est une annulation"
        );
        // Canal fermé : plus aucune tentative d'écriture.
        assert!(
            !channel.progress(2.0, Some(2.0), None),
            "le canal doit rester fermé après épuisement du budget"
        );

        // Aucun octet n'a été écrit sur la socket.
        client
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("timeout de lecture");
        let mut tampon = [0u8; 64];
        let lu = (&client).read(&mut tampon);
        assert!(
            matches!(&lu, Err(error) if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            )),
            "des octets ont été écrits malgré l'échéance : {lu:?}"
        );
    }

    /// Événement final hors budget : `finish` rend l'erreur d'échéance et
    /// lève le jeton, au lieu d'écrire en retard.
    #[test]
    fn finish_refuse_le_budget_epuise() {
        let (conn, _client) = paire_loopback();
        let cancel = CancelToken::new();
        let channel =
            SseProgressChannel::new(conn, serde_json::json!(1), cancel.clone(), Duration::ZERO);

        let resultat = channel.finish(Some(br#"{"jsonrpc":"2.0","id":1}"#));
        assert!(
            matches!(&resultat, Err(error) if error.kind() == std::io::ErrorKind::TimedOut),
            "l'échéance doit être rendue, pas ignorée : {resultat:?}"
        );
        assert!(
            cancel.is_cancelled(),
            "l'échec d'écriture final annule la requête"
        );
    }

    /// Chemin nominal : budget intact, l'événement part et arrive intact
    /// (framing et anti-injection inchangés).
    #[test]
    fn progress_ecrit_l_evenement_dans_le_budget() {
        let (conn, mut client) = paire_loopback();
        let cancel = CancelToken::new();
        let channel =
            SseProgressChannel::new(conn, serde_json::json!(7), cancel.clone(), WRITE_TIMEOUT);

        assert!(channel.progress(1.0, Some(3.0), Some("départ")));
        assert!(
            !cancel.is_cancelled(),
            "aucune annulation sur le chemin nominal"
        );

        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout de lecture");
        let mut recu = Vec::new();
        let mut tampon = [0u8; 256];
        while !recu.windows(2).any(|fenetre| fenetre == b"\n\n") {
            let lu = client.read(&mut tampon).expect("événement attendu");
            assert!(lu > 0, "flux fermé avant la fin de l'événement");
            recu.extend_from_slice(&tampon[..lu]);
        }
        let evenement = String::from_utf8(recu).expect("UTF-8");
        assert!(evenement.starts_with("data: "), "{evenement}");
        assert!(
            evenement.contains(crate::protocol::NOTIFICATION_PROGRESS),
            "{evenement}"
        );
        assert!(evenement.contains("\"progressToken\":7"), "{evenement}");
        assert!(evenement.contains("\"message\":\"départ\""), "{evenement}");
    }

    /// Le budget ne mesure **que** le temps passé à écrire : une attente
    /// longue entre deux émissions (handler qui travaille) ne le consomme
    /// pas. Sans cette distinction, `no_tool_timeout()` deviendrait
    /// inutilisable dès qu'un handler dépasse le budget d'écriture.
    #[test]
    fn une_attente_du_handler_ne_consomme_pas_le_budget() {
        let (conn, _client) = paire_loopback();
        let cancel = CancelToken::new();
        let channel = SseProgressChannel::new(
            conn,
            serde_json::json!(1),
            cancel.clone(),
            Duration::from_millis(50),
        );
        std::thread::sleep(Duration::from_millis(120));
        assert!(
            channel.progress(1.0, Some(2.0), None),
            "le silence du handler ne doit pas consommer le budget d'écriture"
        );
    }

    /// Décision d'échéance isolée : plafond `WRITE_TIMEOUT` et épuisement sous
    /// le pas minimal.
    #[test]
    fn budget_restant_plafonne_et_expire() {
        assert_eq!(
            budget_restant(Duration::from_secs(600)),
            Some(WRITE_TIMEOUT)
        );
        assert_eq!(
            budget_restant(Duration::from_millis(500)),
            Some(Duration::from_millis(500)),
            "un budget plus court que WRITE_TIMEOUT est respecté tel quel"
        );
        assert_eq!(budget_restant(Duration::ZERO), None, "budget épuisé");
        assert_eq!(
            budget_restant(Duration::from_micros(1)),
            None,
            "sous le pas minimal, aucune borne armable : épuisé"
        );
    }

    /// Écrivain qui n'accepte **qu'un octet par appel**, après 1 ms
    /// d'attente : modélise un client qui lit au compte-gouttes **sans**
    /// dépendre de la taille des tampons noyau (un événement de quelques
    /// centaines de Ko tiendrait dans le tampon loopback et l'écriture se
    /// terminerait avant l'échéance). Il enregistre les bornes armées.
    #[derive(Default)]
    struct GoutteAEcriture {
        octets: usize,
        bornes: Vec<Duration>,
    }

    impl Write for GoutteAEcriture {
        fn write(&mut self, tampon: &[u8]) -> std::io::Result<usize> {
            if tampon.is_empty() {
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
        fn borner_prochain_envoi(&mut self, budget: Duration) -> std::io::Result<()> {
            self.bornes.push(budget);
            Ok(())
        }
    }

    /// Le budget est **strict par appel système** : un client lent ne peut pas
    /// cumuler N écritures partielles **réussies**, chacune bornée par
    /// `WRITE_TIMEOUT` (l'ancien `write_all` réutilisait une borne armée une
    /// seule fois). Sans le réarmement avant chaque `write`, les 4 Kio
    /// prendraient ~4 s et l'écriture rendrait `Ok` ; ici elle rend `TimedOut`
    /// au budget (50 ms).
    #[test]
    fn des_ecritures_partielles_ne_depassent_pas_le_budget() {
        let mut flux = GoutteAEcriture::default();
        let mut budget = Duration::from_millis(50);
        let mut arme = None;
        let evenement = vec![b'x'; 4096];
        let debut = Instant::now();
        let resultat = ecrire_budget(&mut flux, &mut budget, &mut arme, &evenement);
        let ecoule = debut.elapsed();
        assert!(
            matches!(&resultat, Err(error) if error.kind() == std::io::ErrorKind::TimedOut),
            "le budget doit couper l'écriture : {resultat:?}"
        );
        assert!(
            ecoule < Duration::from_secs(2),
            "le budget n'est pas strict : {ecoule:?} pour 50 ms"
        );
        assert!(
            flux.octets < evenement.len(),
            "toute la charge a été écrite : {} octets",
            flux.octets
        );
        assert!(
            budget < PAS_MIN_ECRITURE,
            "le budget doit être débité sous le pas minimal armable : {budget:?}"
        );
        assert!(
            flux.bornes.len() > 1,
            "la borne doit être réarmée avant chaque tentative : {:?}",
            flux.bornes
        );
        assert!(
            flux.bornes.windows(2).all(|paire| paire[0] >= paire[1]),
            "les bornes armées doivent décroître : {:?}",
            flux.bornes
        );
        assert!(
            flux.bornes
                .iter()
                .all(|borne| *borne <= Duration::from_millis(50)),
            "une borne dépasse le budget initial : {:?}",
            flux.bornes
        );
    }

    /// Budget consommé par des écritures **successives** : le premier
    /// événement passe, mais il a débité le budget ; le second est refusé sur
    /// ce qui reste — sans attendre un `WRITE_TIMEOUT` plein.
    #[test]
    fn un_budget_consomme_refuse_l_evenement_suivant() {
        let mut flux = GoutteAEcriture::default();
        let mut budget = Duration::from_millis(200);
        let mut arme = None;
        ecrire_budget(&mut flux, &mut budget, &mut arme, b"abc")
            .expect("3 ms d'écriture dans un budget de 200 ms");
        assert_eq!(flux.octets, 3, "un octet accepté par appel");
        assert!(
            budget < Duration::from_millis(200),
            "le budget doit être débité des écritures réussies : {budget:?}"
        );

        let resultat = ecrire_budget(&mut flux, &mut budget, &mut arme, &vec![b'y'; 4096]);
        assert!(
            matches!(&resultat, Err(error) if error.kind() == std::io::ErrorKind::TimedOut),
            "le second événement doit être refusé : {resultat:?}"
        );
        assert!(
            flux.octets < 3 + 4096,
            "l'écriture a continué au-delà du budget : {} octets",
            flux.octets
        );
    }

    /// Client réel qui **ne lit pas** : le tampon d'envoi se remplit,
    /// l'écriture bloquée est coupée par le budget — `progress` rend `false`,
    /// le jeton d'annulation est levé, et l'appel ne dure pas
    /// N × `WRITE_TIMEOUT`.
    ///
    /// `TcpStream` std n'expose pas `SO_SNDBUF` : l'événement doit dépasser ce
    /// que le noyau absorbe (tampon d'envoi + fenêtre de réception du pair),
    /// d'où la taille volontairement large.
    ///
    /// Unix seulement : sur Windows, les tampons AFD du loopback absorbent
    /// plusieurs Mio et l'écriture se termine sans jamais bloquer (mesuré :
    /// 8 Mio acceptés d'un trait) — la propriété de budget strict reste
    /// couverte partout par les tests à écrivain au compte-gouttes ci-dessus.
    #[cfg(unix)]
    #[test]
    fn un_client_qui_ne_lit_pas_epuise_le_budget() {
        let (conn, client) = paire_loopback();
        let cancel = CancelToken::new();
        let budget = Duration::from_millis(200);
        let channel =
            SseProgressChannel::new(conn, serde_json::json!("jeton"), cancel.clone(), budget);

        // Le pair ne lit jamais : seuls les tampons noyau absorbent.
        let gros = "x".repeat(8 * 1024 * 1024);
        let debut = Instant::now();
        assert!(
            !channel.progress(1.0, Some(2.0), Some(&gros)),
            "un client figé doit épuiser le budget d'écriture"
        );
        let ecoule = debut.elapsed();
        assert!(
            cancel.is_cancelled(),
            "un échec d'écriture annule la requête (le handler le voit)"
        );
        assert!(
            ecoule < Duration::from_secs(3),
            "l'écriture a bloqué {ecoule:?} malgré un budget de {budget:?}"
        );

        // Budget épuisé : l'événement suivant est refusé sans écriture.
        assert!(!channel.progress(2.0, Some(2.0), None));
        drop(client);
    }
}
