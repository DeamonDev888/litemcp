//! Transport Streamable HTTP **stateless** — HTTP/1.1 minimal sur `std::net`.
//!
//! Une seule route (`POST /mcp` par défaut) : chaque message JSON-RPC est un
//! POST, la réponse arrive en `application/json` ou en `text/event-stream`
//! selon l'en-tête `Accept`. Un flux SSE reste **request-scoped** : sans jeton
//! de progression, un seul événement de réponse ; avec
//! `params._meta.progressToken`, les `notifications/progress` du handler
//! ([`crate::progress`]) précèdent l'événement de réponse et la connexion est
//! fermée à la fin. Pas de session, pas de listener `GET` (405), garde
//! DNS-rebinding sur `Origin`. **Annulation coopérative** : sur un flux SSE,
//! une écriture qui échoue (client déconnecté) lève le drapeau de la requête
//! (`crate::cancel`), que le handler consulte à ses points sûrs ; rien n'est
//! interrompu de force, et `tool_timeout` (30 s par défaut) borne la réponse.
//!
//! Durcissement :
//! - timeout de **lecture** par opération (30 s par défaut, `.read_timeout()`)
//!   **et deadline totale par requête** (60 s) : un client qui dribble des
//!   octets est coupé lui aussi ;
//! - **TTL de connexion** (5 min par défaut) : une connexion keep-alive qui
//!   envoie une requête toutes les 29 s ne peut pas retenir son slot
//!   indéfiniment (le deadline par requête est réinitialisé à chaque
//!   requête — sans TTL, le plafond était saturable durablement par 256
//!   clients lents) ;
//! - plafond de connexions simultanées (`MAX_CONNECTIONS`, défaut 256, par
//!   serveur) : au-delà, la connexion est refusée immédiatement (`503`) —
//!   pas de spawn illimité ;
//! - **le corps est lu par tranches** (buffer 64 Ko réutilisé) : un
//!   `Content-Length` de 1 Mio n'est plus pré-alloué, un client qui
//!   annonce 1 Mio et n'envoie rien ne coûte plus 1 Mio de commit ;
//! - limites `MAX_HEADERS` (64) / `MAX_LINE` (16 Kio) → `431`, `MAX_BODY`
//!   (1 Mio) → `413` ;
//! - `Transfer-Encoding` présent ou `Content-Length` dupliqué → `400`
//!   (pas de désynchronisation type request smuggling derrière un proxy) —
//!   **contrôle appliqué AVANT le routage**, donc sur toutes les routes
//!   y compris `/health` ;
//! - `Content-Length` malformé → `400` (RFC 9112 §6.3 ; le `411` est réservé
//!   à l'absence du champ) ;
//! - en-tête `Host` requis et unique en HTTP/1.1 (`400` sinon, RFC 9112
//!   §3.2), lignes obs-fold rejetées (`400`), ligne d'en-tête sans
//!   `:` ou avec une espace avant `:` rejetée (`400`, RFC 9112 §5.1) ;
//! - **`Host` confronté au listener** (`.allowed_hosts()`) : par défaut seules
//!   les autorités locales sont acceptées (adresse effective du listener,
//!   littéral de bouclage, nom court non qualifié) — tout nom pleinement
//!   qualifié ou littéral d'adresse étranger rend `421 Misdirected Request`,
//!   même sans `Origin` : la garde DNS-rebinding ne dépend plus du navigateur ;
//! - **CORS pour les origines listées** (`.allow_origins()`) : préflight
//!   `OPTIONS` de la route MCP → `204` avec `Access-Control-Allow-Origin`,
//!   `-Methods` et `-Headers`, `Vary: Origin` ; les réponses `POST` d'une
//!   origine autorisée — `200` JSON, flux SSE **et** `202 Accepted` d'une
//!   notification — portent `Access-Control-Allow-Origin` + `Vary: Origin`.
//!   Une origine absente de la liste reste refusée (`403`) et ne reçoit aucun
//!   en-tête CORS ;
//! - **échéance globale d'écriture** (`WRITE_DEADLINE_MIN..=WRITE_DEADLINE_MAX`,
//!   dérivée de `read_timeout` ; `.write_deadline()` la remplace par un plafond
//!   explicite, relevable) : `WRITE_TIMEOUT` ne borne qu'un `write_vectored`,
//!   pas l'envoi complet — un client qui lit un octet de temps en temps est
//!   coupé à l'échéance, la connexion est fermée ;
//! - en-têtes à valeur unique **dupliqués** (`Origin`, `Content-Type`,
//!   `Mcp-Method`, `Mcp-Name`) → `400` : retenir la dernière valeur rendait
//!   le verdict dépendant de l'ordre, divergent d'un proxy qui combine les
//!   valeurs (RFC 9110 §5.3) ou retient la première ;
//! - ligne de requête stricte : trois jetons séparés par des **espaces
//!   ASCII** seulement (RFC 9112 §3), version exacte `HTTP/1.0`/`HTTP/1.1`
//!   sensible à la casse → `505` sinon ; ligne malformée → `400` ;
//! - **corps consommé sur TOUTES les routes** (`/health` compris) : un
//!   `Content-Length` non consommé ferait de la requête suivante le corps
//!   résiduel → désynchronisation de la file de réponses ;
//! - `HEAD` : toute réponse à un `HEAD` est **sans corps** (RFC 9110 §9.3.2)
//!   — `HEAD /health` donne les en-têtes du `GET` (sondes qui parlent HEAD) ;
//! - échec d'`accept()` : pause brève (pas de boucle de spins si les
//!   descripteurs s'épuisent) ; échec de création de thread → `503` sur
//!   **cette** connexion, le serveur continue (jamais de panique du serveur) ;
//! - `Expect: 100-continue` honoré, et toute réponse d'erreur **ferme la
//!   connexion** (`Connection: close`) : aucun octet résiduel ne peut être
//!   réinterprété comme une requête suivante — on ne draine pas le corps
//!   restant (le drain nourrirait un client qui inonde), au prix possible
//!   d'un RST côté client avant lecture de la réponse (compromis assumé) ;
//! - `POST /mcp?query=…` accepté (la query est ignorée) ;
//! - `HTTP/1.0` sans `Connection: keep-alive` → connexion fermée ;
//! - réponses SSE avec **en-têtes anti-tampon** (`X-Accel-Buffering: no`,
//!   `Cache-Control: no-cache, no-transform`) pour les hébergements derrière
//!   un proxy qui tamponne les flux ; choix architectural : **pas de SSE
//!   longue durée** — request-scoped, un événement puis clôture ;
//! - panique d'un handler capturée (`catch_unwind`) → `-32603` JSON-RPC
//!   **avec l'`id` de la requête** (la réponse reste corrélable), la
//!   connexion du client reste propre. NB : compiler avec
//!   `panic = "abort"` neutralise `catch_unwind` — une panique de handler
//!   arrête alors le processus entier.

use crate::cancel::CancelToken;
use crate::progress::ProgressChannel;
use crate::server::ServerCore;
use std::fmt;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, IoSlice, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

mod conn;
mod connection;
mod parse;
mod response;
mod security;
mod serve;
mod sse;

pub(crate) use conn::Conn;
pub use serve::{BoundServer, ServerHandle};

/// Taille maximale d'un corps de requête (1 Mio par défaut).
pub const MAX_BODY: usize = 1 << 20;

/// Taille des chunks de lecture du corps (borne la mémoire allouée par
/// une requête : un `Content-Length` de 1 Mio ne pré-alloue plus 1 Mio).
pub const BODY_CHUNK: usize = 64 * 1024;

/// Nombre maximal d'en-têtes par requête (au-delà → `431`).
pub const MAX_HEADERS: usize = 64;

/// Longueur maximale d'une ligne (requête ou en-tête) — au-delà → `431`.
pub const MAX_LINE: usize = 16 * 1024;

/// Timeout de lecture par opération (anti-slowloris) — défaut 30 s.
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Timeout d'écriture par opération — défaut 10 s.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Borne **basse** de l'échéance globale d'écriture d'une réponse.
pub const WRITE_DEADLINE_MIN: Duration = Duration::from_secs(1);

/// Borne **haute** de l'échéance globale d'écriture d'une réponse — 60 s.
pub const WRITE_DEADLINE_MAX: Duration = Duration::from_secs(60);

/// Échéance **globale** d'écriture d'une réponse, dérivée du timeout de
/// lecture : deux fois `read_timeout`, bornée à
/// [`WRITE_DEADLINE_MIN`]..=[`WRITE_DEADLINE_MAX`].
///
/// C'est la valeur **par défaut** :
/// [`crate::ServerBuilder::write_deadline`] la remplace par un plafond
/// explicite (relevable au-delà de `read_timeout × 2`).
///
/// `WRITE_TIMEOUT` ne borne qu'un `write_vectored` : un client qui lit un
/// octet tous les 9 s ne déclenche jamais ce timeout et retient son slot
/// jusqu'à la fin d'une réponse de 16 Mio. Cette échéance-ci borde l'envoi
/// **complet**, dans la même unité que le budget de lecture déjà calculé
/// pour la requête (`read_timeout × 2`, borné à [`REQUEST_DEADLINE`]) : un
/// serveur qui tolère un client lent en lecture tolère un client lent en
/// écriture dans la même proportion.
fn write_deadline(read_timeout: Duration) -> Duration {
    read_timeout
        .saturating_mul(2)
        .clamp(WRITE_DEADLINE_MIN, WRITE_DEADLINE_MAX)
}

/// Deadline totale par requête (lecture en-têtes + corps) — défaut 60 s.
/// Coupe aussi les clients qui driblent des octets sous le timeout par
/// opération.
pub const REQUEST_DEADLINE: Duration = Duration::from_secs(60);

/// Durée de vie maximale d'une connexion keep-alive — défaut 5 min.
///
/// Borne le temps d'occupation d'un slot du plafond : sans elle, un client
/// qui envoie une requête valide toutes les 29 s (sous le `READ_TIMEOUT`)
/// retient son slot **indéfiniment** et 256 clients lents suffisent à
/// saturer le serveur pour tous les autres.
pub const CONNECTION_TTL: Duration = Duration::from_secs(300);

/// Nombre maximal de refus (503) dont l'écriture est en cours **en
/// parallèle**. Au-delà, la connexion est fermée sans réponse : mieux vaut
/// un RST qu'une boucle d'`accept` (ou une avalanche de threads) bloquée.
pub const MAX_REFUSALS_IN_FLIGHT: usize = 64;

/// Plafond de connexions simultanées **par serveur** — au-delà, refus
/// immédiat (pas de spawn). Défaut 256.
pub const MAX_CONNECTIONS: usize = 256;

/// Chemin de la sonde de vie : `GET {HEALTH_ROUTE}` → `200` « ok » sans
/// JSON-RPC (probes k8s/systemd/équilibreurs — point fort repris de
/// ViteMCP). `HEAD` donne les mêmes en-têtes sans le corps ; toute autre
/// méthode → `405`.
pub const HEALTH_ROUTE: &str = "/health";

/// Document de métadonnées de ressource protégée (RFC 9728), servi
/// **uniquement** quand l'autorisation est active. Il ne décrit aucun
/// serveur d'autorisation et aucun jeton porteur : le profil est une
/// signature de requête (`Signature realm="litemcp"`), et un document qui
/// annoncerait `bearer_methods_supported` serait mensonger.
pub const PROTECTED_RESOURCE_METADATA_ROUTE: &str = "/.well-known/oauth-protected-resource";

/// En-têtes anti-tampon émis sur toute réponse SSE. `X-Accel-Buffering:
/// no` est la directive **par réponse** que nginx honore même quand
/// `proxy_buffering` est actif globalement (le cas des VPS qu'on ne
/// contrôle pas) ; `no-transform` interdit aux intermédiaires de
/// retoucher le flux. Le JSON ne les porte pas : rien à tamponner.
pub const SSE_ANTI_BUFFERING: &str =
    "Cache-Control: no-cache, no-transform\r\nX-Accel-Buffering: no\r\n";

/// Contexte d'une requête HTTP, vu par un hook
/// [`ServerBuilder::on_request`](crate::server::ServerBuilder::on_request).
///
/// Le hook reçoit ce contexte **avant toute lecture du corps** : un rejet
/// ne paie même pas le transport du payload. Les en-têtes sont conservés
/// par la lib **seulement** quand un hook est enregistré — sans hook, le
/// chemin chaud reste sans allocation et sans rétention (l'entrepôt des
/// en-têtes est réutilisé sur la connexion keep-alive).
pub struct RequestContext<'a> {
    /// Méthode HTTP (`POST`, `GET`…), telle que reçue sur la ligne de
    /// requête.
    pub method: &'a str,
    /// Chemin de la requête sans la query (`/mcp` par défaut). Une route
    /// inconnue (404) et la sonde `/health` ne passent **pas** par le hook.
    pub path: &'a str,
    /// Query string sans le `?` (`x=1&y=2`), si présente.
    pub query: Option<&'a str>,
    /// Adresse du pair TCP. **Derrière un reverse proxy, c'est l'adresse du
    /// proxy**, pas du client final — l'identité du client voyage alors dans
    /// `X-Forwarded-For` (à ne croire que si la requête vient de votre
    /// proxy : vérifiez `peer` avant de faire confiance à cet en-tête).
    pub peer: SocketAddr,
    /// Entrepôt concaténé des en-têtes bruts (spans de [`Self::spans`]).
    store: &'a str,
    /// Spans `(nom_début, nom_fin, valeur_début, valeur_fin)` dans
    /// [`Self::store`], dans l'ordre d'arrivée.
    spans: &'a [(u32, u32, u32, u32)],
}

impl<'a> RequestContext<'a> {
    /// Valeur d'un en-tête, par nom **insensible à la casse** — la première
    /// occurrence reçue gagne (les en-têtes à valeur unique que la lib
    /// refuse d'elle-même sont déjà partis en `400` avant le hook ; les
    /// autres peuvent légitimement se répéter, et le verdict ne doit pas
    /// dépendre de l'ordre de lecture d'un proxy amont).
    ///
    /// La valeur est renvoyée telle que reçue : les espaces OWS de bord sont
    /// déjà retirés au parsing (RFC 9110 §5.5) — `"Bearer abc"` arrive donc
    /// sans espace parasite.
    pub fn header(&self, name: &str) -> Option<&'a str> {
        self.spans
            .iter()
            .find(|(name_start, name_end, _, _)| {
                self.store
                    .get(*name_start as usize..*name_end as usize)
                    .is_some_and(|found| found.eq_ignore_ascii_case(name))
            })
            .and_then(|(_, _, value_start, value_end)| {
                self.store.get(*value_start as usize..*value_end as usize)
            })
    }
}

/// Debug **sans les valeurs d'en-têtes** : un `Authorization` complet dans
/// un log de diagnostic, c'est un jeton qui fuit. Les noms seuls suffisent
/// à comprendre quelles en-têtes sont arrivées.
impl fmt::Debug for RequestContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self
            .spans
            .iter()
            .filter_map(|(name_start, name_end, _, _)| {
                self.store.get(*name_start as usize..*name_end as usize)
            })
            .collect();
        f.debug_struct("RequestContext")
            .field("method", &self.method)
            .field("path", &self.path)
            .field("query", &self.query)
            .field("peer", &self.peer)
            .field("headers", &names)
            .finish()
    }
}

/// Rejet d'une requête par un hook
/// [`ServerBuilder::on_request`](crate::server::ServerBuilder::on_request).
///
/// Le transport répond `4xx`/`5xx` avec le message en corps JSON (même
/// enveloppe que les autres erreurs de transport) et **ferme la connexion**
/// — le corps de la requête n'a pas été lu, il ne peut pas être réinterprété
/// comme une requête suivante. Un statut hors `400..=599` est ramené à
/// `400` : un rejet qui répond `200` n'est pas un rejet.
///
/// Les en-têtes supplémentaires sont **assainis au départ de la réponse** :
/// un nom hors jeton RFC 9110 ou une valeur portant un caractère de
/// contrôle (CRLF inclus) est silencieusement écarté — un rejet ne doit
/// jamais devenir une injection d'en-têtes, même si votre code y écho la
/// valeur d'un client.
#[derive(Debug, Clone)]
pub struct RequestRejection {
    status: u16,
    message: String,
    headers: Vec<(String, String)>,
}

impl RequestRejection {
    /// Rejet avec un statut arbitraire (`400..=599` — toute autre valeur
    /// est ramenée à `400`).
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status: if (400..=599).contains(&status) {
                status
            } else {
                400
            },
            message: message.into(),
            headers: Vec::new(),
        }
    }

    /// `401 Unauthorized` — identité absente ou invalide.
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(401, message)
    }

    /// `403 Forbidden` — identité connue, accès refusé.
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(403, message)
    }

    /// `429 Too Many Requests` — quota dépassé. Un `retry_after` non nul
    /// ajoute l'en-tête `Retry-After` en secondes entières (arrondi au
    /// dessus, minimum 1).
    pub fn rate_limited(retry_after: Option<Duration>) -> Self {
        let mut rejection = Self::new(429, "rate limit exceeded");
        if let Some(delay) = retry_after {
            let seconds = (delay.as_secs_f64().ceil() as u64).max(1);
            rejection = rejection.with_header("Retry-After", seconds.to_string());
        }
        rejection
    }

    /// Ajoute un en-tête à la réponse de rejet (`WWW-Authenticate`,
    /// `Retry-After`…). Les valeurs invalides (contrôles, CRLF) sont
    /// écartées **au départ de la réponse**, jamais ici — le constructeur
    /// reste infaillible.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Statut HTTP de la rejet (`400..=599` garanti).
    pub fn status(&self) -> u16 {
        self.status
    }

    /// Message du corps de la réponse (enveloppe JSON-RPC de transport).
    pub fn message(&self) -> &str {
        &self.message
    }

    /// En-têtes supplémentaires déclarés (pas encore assainis — l'assainissement
    /// a lieu au départ de la réponse).
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }
}

/// Signature d'un hook de requête — voir
/// [`ServerBuilder::on_request`](crate::server::ServerBuilder::on_request).
pub type RequestHook = dyn Fn(&RequestContext<'_>) -> Result<(), RequestRejection> + Send + Sync;
