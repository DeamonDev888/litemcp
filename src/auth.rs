//! Autorisation — **qui** appelle, et **ce qu'il a le droit de faire**.
//!
//! Le noyau ne sait pas *comment* une identité a été prouvée : le transport
//! (ou l'application hôte qui possède déjà son socket) établit un
//! [`AuthContext`](crate::auth::AuthContext) puis le remet au dispatch. Le
//! noyau, lui, décide des droits déclarés par l'application
//! ([`crate::server::ServerBuilder::require_right`])
//! et, si elle l'a demandé, interroge une
//! [`ApprovalPolicy`](crate::auth::ApprovalPolicy) avant d'entrer dans un handler.
//!
//! Rien de tout cela n'existe sans [`crate::server::ServerBuilder::verifier`] :
//! sans vérificateur, le comportement du crate est **strictement** celui
//! d'avant l'autorisation (aucun filtrage, aucun refus, aucune allocation).
//!
//! # Le contrat, en une page
//!
//! * **Identité** — un [`Verifier`](crate::auth::Verifier) rend un
//!   [`AuthContext`](crate::auth::AuthContext) (client, libellé, droits,
//!   méthode de preuve, instant de vérification) ou un
//!   [`AuthError`](crate::auth::AuthError).
//!   L'identité n'est **jamais** déduite du corps JSON-RPC : elle vient du
//!   transport, qui est la frontière de confiance.
//! * **Droits** — un droit est une chaîne opaque (`^[a-z0-9][a-z0-9._:-]{0,63}$`,
//!   par convention) ; la lib ne teste que l'appartenance à un ensemble.
//!   Plusieurs droits sur un même élément sont **tous** exigés (ET).
//! * **Échec fermé** — un élément (outil, ressource, prompt, gabarit) sans
//!   droit déclaré est **refusé** dès que l'autorisation est active, et
//!   n'apparaît dans aucune liste.
//! * **Avant le handler** — droits puis confirmation sont décidés avant
//!   l'exécution : un refus ne produit ni contenu ni effet de bord. Le code
//!   JSON-RPC est `-32023` (`ErrorObject::FORBIDDEN`, HTTP `403`) et
//!   `data.reason` porte la distinction (`right_missing`, `approval_denied`,
//!   `approval_pending`). L'existence d'un nom n'est pas un secret : un
//!   élément **inexistant** reste `-32602` (« Unknown tool »).
//! * **Listes filtrées** — `tools/list`, `resources/list`,
//!   `resources/templates/list` et `prompts/list` ne rendent que ce que
//!   l'appelant peut utiliser. Le cache précalculé
//!   ([`crate::server::ServerCore`]) n'est servi que sans autorisation.
//! * **Dispatch imbriqué** — l'[`AuthContext`](crate::auth::AuthContext) est
//!   lié au fil pour la durée du dispatch ([`current`](crate::auth::current)) :
//!   un handler qui re-dispatche (par exemple via
//!   un [`crate::Gateway`]) hérite du contexte, comme [`crate::progress`] et
//!   [`crate::cancel`] le font pour leurs canaux. Un snapshot échangé pendant
//!   l'appel ne vaut que pour le dispatch suivant.
//! * **Observation** — chaque décision d'élément produit un
//!   [`AuthDecisionEvent`](crate::auth::AuthDecisionEvent) pour le journal de
//!   l'application
//!   ([`crate::server::ServerBuilder::on_auth_decision`]) : verdict et motif
//!   **précis** côté application, motif **générique** côté réseau.
//!
//! # Exemple
//!
//! ```
//! use litemcp::auth::{AuthContext, AuthError, ProofMethod, VerifiedRequest, Verifier};
//! use litemcp::server::ServerBuilder;
//! use std::sync::Arc;
//!
//! // Un transport applicatif (MCP Control) a déjà prouvé l'identité : il la
//! // remet au noyau telle quelle, sans que le corps JSON-RPC y participe.
//! struct Loopback;
//!
//! impl Verifier for Loopback {
//!     fn verify(&self, _request: &VerifiedRequest<'_>) -> Result<AuthContext, AuthError> {
//!         Ok(AuthContext::new("client-demo", ProofMethod::Application)
//!             .with_label("Démonstration")
//!             .with_right("db.read"))
//!     }
//! }
//!
//! let core = ServerBuilder::new("demo", "1.0.0")
//!     .tool("db.query", "Lit la base", |_args| {
//!         Ok(serde_json::json!({ "rows": [] }))
//!     })
//!     .require_right("db.query", "db.read")
//!     .verifier(Arc::new(Loopback))
//!     .core();
//!
//! let context = AuthContext::new("client-demo", ProofMethod::Application).with_right("db.read");
//! let body = serde_json::json!({
//!     "jsonrpc": "2.0",
//!     "id": 1,
//!     "method": "tools/call",
//!     "params": {
//!         "name": "db.query",
//!         "arguments": {},
//!         "_meta": {
//!             "io.modelcontextprotocol/protocolVersion": "2026-07-28",
//!             "io.modelcontextprotocol/clientCapabilities": {}
//!         }
//!     }
//! })
//! .to_string();
//!
//! let reply = core
//!     .dispatch_authenticated(body.as_bytes(), None, &context)
//!     .expect("une requête reçoit toujours une réponse");
//! assert_eq!(reply.status, litemcp::server::HttpStatus::Ok);
//! # Ok::<(), ()>(())
//! ```
//!
//! Sans le droit, le même appel rend `-32023` / `right_missing` (HTTP 403) et
//! le handler n'est jamais appelé.
//!
//! # Ce que ce module ne fait pas
//!
//! Aucune cryptographie, aucune signature, aucun TLS, aucun état : le noyau
//! ne fait que comparer des chaînes et appeler des traits fournis par
//! l'application. Le vérificateur de signature est livré dans le
//! sous-module `sign` (feature `auth-sign`) ; les jetons (`auth-oauth`)
//! restent à l'application, qui fournit aussi le magasin de clés, l'écran
//! de confirmation et le journal d'audit.

use serde_json::Value;
use std::cell::RefCell;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

#[cfg(feature = "auth-sign")]
pub mod sign;

/// Longueur maximale d'un motif d'erreur, en caractères — voir
/// [`AuthError::new`].
const MAX_REASON_CHARS: usize = 128;

/// Valeur exacte de l'en-tête `WWW-Authenticate` de **tout** refus
/// d'authentification (401), identique à celle du profil de signature
/// (`docs/auth.md` §2.7). Définie ici, sans feature : le transport la sert
/// dès qu'un vérificateur est configuré, quel qu'il soit.
pub const WWW_AUTHENTICATE: &str = "Signature realm=\"litemcp\", tag=\"litemcp-auth-v1\"";

/// Statut HTTP de tout refus d'authentification du profil.
pub const UNAUTHORIZED_STATUS: u16 = 401;

/// Statut HTTP d'un refus d'autorisation (droit manquant, confirmation
/// refusée) — le corps porte `-32023`.
pub const FORBIDDEN_STATUS: u16 = 403;

/// Corps **constant** de tout `401` : celui des rejets de transport
/// existants (`code -32600`, `id: null`), avec un message fixe qui ne dit
/// jamais *pourquoi* la requête a été refusée. Le motif précis ne sort que
/// par [`AuthDecisionEvent`] (côté application).
#[must_use]
pub fn unauthorized_body() -> Vec<u8> {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": null,
        "error": { "code": -32600, "message": "unauthorized" }
    })
    .to_string()
    .into_bytes()
}

/// Réponse complète d'une requête vérifiée puis dispatchée — voir
/// [`crate::server::ServerCore::dispatch_verified`] (contrat C8).
///
/// C'est la forme dont a besoin une application qui possède **son propre**
/// socket HTTP (MCP Control) : elle écrit `status`, `headers` et `body`
/// tels quels. Le type vit dans le noyau (aucune feature) : un vérificateur
/// applicatif peut produire la même forme sans `auth-sign`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AuthResponse {
    /// Statut HTTP (401, 403, 200, 400, 404, 202…).
    pub status: u16,
    /// En-têtes supplémentaires (`WWW-Authenticate` sur un 401), déjà
    /// assainis et prêts à l'envoi.
    pub headers: Vec<(String, String)>,
    /// Corps de la réponse, octet pour octet.
    pub body: Vec<u8>,
}

impl AuthResponse {
    /// Assemble une réponse.
    #[must_use]
    pub fn new(status: u16, headers: Vec<(String, String)>, body: Vec<u8>) -> Self {
        Self {
            status,
            headers,
            body,
        }
    }

    /// `401` générique : `WWW-Authenticate` du profil et corps constant
    /// ([`unauthorized_body`]). Aucun motif n'y figure.
    #[must_use]
    pub fn unauthorized() -> Self {
        Self {
            status: UNAUTHORIZED_STATUS,
            headers: vec![("WWW-Authenticate".to_string(), WWW_AUTHENTICATE.to_string())],
            body: unauthorized_body(),
        }
    }
}

/// Refus explicite d'épinglage : l'identité observée n'est pas celle qui a
/// été épinglée. Le message ne porte **aucune** empreinte (ni attendue, ni
/// observée) : un journal d'erreur ne doit pas publier l'identité du serveur
/// qu'on refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PinnedServerError;

impl std::fmt::Display for PinnedServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("server identity changed: certificate fingerprint mismatch")
    }
}

impl std::error::Error for PinnedServerError {}

/// Épinglage d'identité serveur (contrat C9, réduit) — l'empreinte TLS
/// attendue, comparée en temps constant.
///
/// 0.11.0/0.12.0 est un serveur seul : il n'existe **aucun** client HTTP
/// dans le crate, donc C9 ne peut pas être un client. Ce qui manquait à une
/// application qui a déjà son socket (MCP Control) est la primitive de
/// vérification d'identité du serveur : le signeur `auth::sign::Signer`
/// signe, `PinnedServer` refuse un serveur dont le certificat a changé. Aucun
/// contournement n'existe (pas de `danger_accept_invalid_certs` ici) :
/// l'appelant compare l'empreinte qu'il a observée et **doit** refuser sur
/// `Err`.
///
/// L'empreinte est celle de `docs/auth.md` §8 : `sha256:` suivi de 64
/// caractères hexadécimaux **minuscules** (SHA-256 du DER du certificat).
/// Sans la feature `tls`, la primitive reste utilisable : l'application
/// calcule l'empreinte avec l'outil de son choix.
///
/// ```
/// use litemcp::auth::PinnedServer;
///
/// let pin = PinnedServer::new("sha256:0000000000000000000000000000000000000000000000000000000000000000")
///     .expect("empreinte canonique");
/// assert!(pin.matches("sha256:0000000000000000000000000000000000000000000000000000000000000000"));
/// let refus = pin.verify("sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff");
/// assert!(refus.is_err());
/// ```
#[derive(Clone)]
pub struct PinnedServer {
    fingerprint: [u8; 32],
}

impl PinnedServer {
    /// Épingle l'empreinte fournie (`sha256:` + 64 hexadécimaux minuscules).
    ///
    /// # Erreurs
    ///
    /// [`AuthError`] (`fingerprint_malformed`) si la forme n'est pas
    /// canonique — un épinglage qu'on ne sait pas lire n'est pas un
    /// épinglage.
    pub fn new(fingerprint: &str) -> Result<Self, AuthError> {
        let fingerprint = parse_fingerprint(fingerprint)
            .ok_or_else(|| AuthError::new("fingerprint_malformed"))?;
        Ok(Self { fingerprint })
    }

    /// L'empreinte épinglée, sous sa forme canonique (`sha256:` + hex).
    #[must_use]
    pub fn fingerprint(&self) -> String {
        let mut texte = String::with_capacity(71);
        texte.push_str("sha256:");
        for byte in self.fingerprint {
            texte.push(hex_digit(byte >> 4));
            texte.push(hex_digit(byte & 0x0f));
        }
        texte
    }

    /// L'empreinte observée est-elle celle qui a été épinglée ?
    ///
    /// Comparaison en **temps constant** ; une empreinte illisible rend
    /// `false` (échec fermé).
    #[must_use]
    pub fn matches(&self, observed: &str) -> bool {
        match parse_fingerprint(observed) {
            Some(observed) => {
                let mut difference = 0u8;
                for (expected, seen) in self.fingerprint.iter().zip(observed.iter()) {
                    difference |= expected ^ seen;
                }
                difference == 0
            }
            None => false,
        }
    }

    /// Vérifie l'identité observée et **refuse explicitement** si elle a
    /// changé.
    ///
    /// # Erreurs
    ///
    /// [`PinnedServerError`] — message constant, sans empreinte : l'appelant
    /// doit fermer la connexion, sans option de contournement.
    pub fn verify(&self, observed: &str) -> Result<(), PinnedServerError> {
        if self.matches(observed) {
            Ok(())
        } else {
            Err(PinnedServerError)
        }
    }
}

impl std::fmt::Debug for PinnedServer {
    // L'empreinte est publique par nature (elle identifie le serveur) : elle
    // est affichable, mais jamais mêlée à un secret (il n'y en a aucun ici).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedServer")
            .field("fingerprint", &self.fingerprint())
            .finish()
    }
}

/// Décode `sha256:` + 64 hexadécimaux **minuscules** ; `None` sinon.
fn parse_fingerprint(value: &str) -> Option<[u8; 32]> {
    let hex = value.strip_prefix("sha256:")?;
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (index, slot) in out.iter_mut().enumerate() {
        let high = hex.as_bytes().get(index * 2).copied()?;
        let low = hex.as_bytes().get(index * 2 + 1).copied()?;
        *slot = (hex_value(high)? << 4) | hex_value(low)?;
    }
    Some(out)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn hex_digit(value: u8) -> char {
    match value & 0x0f {
        0..=9 => char::from(b'0' + (value & 0x0f)),
        other => char::from(b'a' + other - 10),
    }
}

/// Méthode de preuve d'une identité — **information**, jamais une décision.
///
/// Le noyau ne l'interprète pas : il la rend telle quelle à l'application
/// (journal, écran, politique interne). Aucune variante ne porte de secret :
/// `keyid` et `issuer` sont des identifiants publics, choisis par
/// l'application.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProofMethod {
    /// Signature de requête (RFC 9421) vérifiée sous ce `keyid`.
    Signature {
        /// Identifiant public de la clé (`^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$`).
        keyid: String,
    },
    /// Jeton porteur validé, émis par cet émetteur (`iss`).
    Token {
        /// Émetteur du jeton, tel que validé.
        issuer: String,
    },
    /// Identité établie par l'application elle-même (transport local, socket
    /// de confiance, passerelle déjà authentifiée) : aucun artefact
    /// cryptographique n'est attaché.
    Application,
}

/// Identité d'un appelant, **établie par le transport**.
///
/// Construite par le transport (ou par un [`Verifier`]) puis remise au
/// dispatch ([`crate::server::ServerCore::dispatch_authenticated`]) : elle
/// n'est jamais déduite du corps JSON-RPC. Les droits qu'elle porte sont
/// ceux que l'application a déclarés (voir
/// [`crate::server::ServerBuilder::require_right`]) ; la lib ne fait que
/// tester leur appartenance.
///
/// Le contenu n'est pas validé ici : c'est le [`Verifier`] qui est la
/// frontière de confiance, et c'est à lui de borner ce qu'il accepte (le
/// profil de signature limite par exemple `keyid` à 128 octets ASCII).
///
/// ```
/// use litemcp::auth::{AuthContext, ProofMethod};
///
/// let contexte = AuthContext::new("client-demo", ProofMethod::Application)
///     .with_label("Poste de démonstration")
///     .with_right("db.read")
///     .with_rights(["db.write", "db.read"]);
///
/// assert_eq!(contexte.client(), "client-demo");
/// assert_eq!(contexte.label(), Some("Poste de démonstration"));
/// assert_eq!(contexte.rights(), ["db.read", "db.write"]);
/// assert!(contexte.has_right("db.write"));
/// assert!(!contexte.has_right("db.admin"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AuthContext {
    client: String,
    label: Option<String>,
    /// Droits triés et dédoublonnés : `has_right` est une recherche binaire,
    /// et [`AuthContext::rights`] rend un ordre stable (journaux, tests).
    rights: Vec<String>,
    proof: ProofMethod,
    verified_at: SystemTime,
}

impl AuthContext {
    /// Nouvelle identité — **le** constructeur des transports applicatifs et
    /// des tests.
    ///
    /// `verified_at` vaut l'instant de l'appel ; un vérificateur qui connaît
    /// l'instant réel de sa vérification le remplace par
    /// [`Self::with_verified_at`].
    #[must_use]
    pub fn new(client: impl Into<String>, proof: ProofMethod) -> Self {
        Self {
            client: client.into(),
            label: None,
            rights: Vec::new(),
            proof,
            verified_at: SystemTime::now(),
        }
    }

    /// Libellé lisible (nom d'écran, nom d'appareil) — jamais utilisé pour
    /// décider.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Ajoute un droit (idempotent : l'ensemble est dédoublonné).
    #[must_use]
    pub fn with_right(mut self, right: impl Into<String>) -> Self {
        self.insert_right(right.into());
        self
    }

    /// Ajoute plusieurs droits (idempotent, ordre indifférent).
    #[must_use]
    pub fn with_rights<I, S>(mut self, rights: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for right in rights {
            self.insert_right(right.into());
        }
        self
    }

    /// Instant de la vérification d'identité, tel que le transport l'a
    /// observé (horloge murale — sert au journal, jamais à une décision).
    #[must_use]
    pub fn with_verified_at(mut self, verified_at: SystemTime) -> Self {
        self.verified_at = verified_at;
        self
    }

    /// Insère en ordre trié, sans doublon.
    fn insert_right(&mut self, right: String) {
        if let Err(at) = self.rights.binary_search(&right) {
            self.rights.insert(at, right);
        }
    }

    /// Identifiant du client, tel que le vérificateur l'a établi.
    #[must_use]
    pub fn client(&self) -> &str {
        &self.client
    }

    /// Libellé lisible de l'identité, s'il a été fourni.
    #[must_use]
    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }

    /// Droits détenus, triés et dédoublonnés.
    #[must_use]
    pub fn rights(&self) -> &[String] {
        &self.rights
    }

    /// L'appelant détient-il ce droit ? (comparaison exacte, sans joker ni
    /// hiérarchie)
    #[must_use]
    pub fn has_right(&self, right: &str) -> bool {
        self.rights
            .binary_search_by(|probe| probe.as_str().cmp(right))
            .is_ok()
    }

    /// Méthode de preuve attachée à cette identité.
    #[must_use]
    pub fn proof(&self) -> &ProofMethod {
        &self.proof
    }

    /// Instant de la vérification d'identité.
    #[must_use]
    pub fn verified_at(&self) -> SystemTime {
        self.verified_at
    }
}

/// Requête brute remise à un [`Verifier`] par le transport.
///
/// C'est l'entrée de la vérification d'identité : méthode, chemin,
/// autorité, en-têtes **tels que reçus** et corps **exactement** tel que
/// reçu (après décadrage, avant toute analyse JSON). La vérification est
/// entièrement à la charge du vérificateur : ce type ne normalise rien et ne
/// valide rien.
#[derive(Clone, Copy)]
pub struct VerifiedRequest<'a> {
    /// Méthode HTTP reçue (`POST`).
    pub method: &'a str,
    /// Chemin absolu reçu, query exclue, octets `%XX` non décodés.
    pub path: &'a str,
    /// Autorité reçue (`Host`).
    pub authority: &'a str,
    /// En-têtes reçus, dans l'ordre de réception, sans normalisation.
    pub headers: &'a [(&'a str, &'a str)],
    /// Corps reçu, octet pour octet.
    pub body: &'a [u8],
}

impl<'a> VerifiedRequest<'a> {
    /// Assemble une requête à vérifier.
    #[must_use]
    pub fn new(
        method: &'a str,
        path: &'a str,
        authority: &'a str,
        headers: &'a [(&'a str, &'a str)],
        body: &'a [u8],
    ) -> Self {
        Self {
            method,
            path,
            authority,
            headers,
            body,
        }
    }

    /// Première valeur de l'en-tête `name` (comparaison ASCII insensible à la
    /// casse), ou `None`.
    ///
    /// Un en-tête **dupliqué** n'est pas une erreur ici : le vérificateur qui
    /// exige « exactement une occurrence » compte les valeurs avec
    /// [`Self::header_values`].
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&'a str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| *value)
    }

    /// Toutes les valeurs de l'en-tête `name`, dans l'ordre de réception.
    pub fn header_values<'b>(&'b self, name: &'b str) -> impl Iterator<Item = &'a str> + 'b {
        self.headers
            .iter()
            .filter(move |(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| *value)
    }
}

/// `Debug` **sans le corps** : un journal ne doit pas recopier une requête
/// entière (le corps peut porter des données applicatives sensibles).
impl std::fmt::Debug for VerifiedRequest<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifiedRequest")
            .field("method", &self.method)
            .field("path", &self.path)
            .field("authority", &self.authority)
            .field("headers", &self.headers.len())
            .field("body_len", &self.body.len())
            .finish()
    }
}

/// Refus d'authentification — motif **précis**, destiné à l'application.
///
/// Jamais rendu tel quel au réseau : le transport en fait une réponse
/// générique (`401` + `WWW-Authenticate`, corps constant). Le motif est
/// tronqué à 128 caractères pour qu'un champ venu du réseau ne puisse pas
/// gonfler une ligne de journal ; il ne doit contenir **aucun secret**
/// (clé, nonce, signature).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AuthError {
    reason: String,
}

impl AuthError {
    /// Refus motivé (`signature_invalid`, `nonce_replay`, `token_expired`…).
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        let mut reason = reason.into();
        if let Some((cut, _)) = reason.char_indices().nth(MAX_REASON_CHARS) {
            reason.truncate(cut);
        }
        Self { reason }
    }

    /// Motif du refus, pour le journal ou l'écran de l'application.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "authentication refused: {}", self.reason)
    }
}

impl std::error::Error for AuthError {}

/// Établit l'identité d'une requête — implémenté par les lots `auth-sign`
/// (signature RFC 9421) et `auth-oauth` (jetons), et par les transports
/// applicatifs qui ont déjà prouvé l'identité autrement.
///
/// # Contrat
///
/// * `Ok(context)` : identité **prouvée** ; le noyau décide ensuite des
///   droits et de la confirmation.
/// * `Err(error)` : refus. Le transport rend un `401` générique et le motif
///   n'est publié qu'à l'application.
///
/// # Panique
///
/// Un vérificateur **ne doit pas** paniquer (une entrée réseau ne doit
/// jamais faire tomber le service). Le noyau s'en protège quand même : voir
/// [`verify_or_refuse`], que tout transport doit employer.
///
/// ```no_run
/// use litemcp::auth::{AuthContext, AuthError, ProofMethod, VerifiedRequest, Verifier};
///
/// struct Application;
///
/// impl Verifier for Application {
///     fn verify(&self, request: &VerifiedRequest<'_>) -> Result<AuthContext, AuthError> {
///         // Ici : lire `request.headers` / `request.body`, puis décider.
///         match request.header("x-client") {
///             Some(client) => Ok(AuthContext::new(client, ProofMethod::Application)),
///             None => Err(AuthError::new("client_header_missing")),
///         }
///     }
/// }
/// ```
pub trait Verifier: Send + Sync {
    /// Vérifie la requête et rend l'identité établie, ou un refus motivé.
    fn verify(&self, request: &VerifiedRequest<'_>) -> Result<AuthContext, AuthError>;
}

/// Appelle un [`Verifier`] en **contenant ses paniques** : une panique est un
/// refus (`verifier_panicked`), jamais une interruption du service.
///
/// C'est le seul point d'entrée recommandé pour un transport : il garantit
/// que l'échec fermé tient même face à un vérificateur défaillant.
///
/// # Exemple
///
/// ```
/// use litemcp::auth::{verify_or_refuse, Verifier};
///
/// struct Defaillant;
///
/// impl Verifier for Defaillant {
///     fn verify(&self, _request: &litemcp::auth::VerifiedRequest<'_>)
///         -> Result<litemcp::auth::AuthContext, litemcp::auth::AuthError> {
///         panic!("bug du vérificateur");
///     }
/// }
///
/// let requete = litemcp::auth::VerifiedRequest::new("POST", "/mcp", "127.0.0.1:23080", &[], b"{}");
/// let refus = verify_or_refuse(&Defaillant, &requete).expect_err("une panique est un refus");
/// assert_eq!(refus.reason(), "verifier_panicked");
/// ```
pub fn verify_or_refuse(
    verifier: &dyn Verifier,
    request: &VerifiedRequest<'_>,
) -> Result<AuthContext, AuthError> {
    match std::panic::catch_unwind(AssertUnwindSafe(|| verifier.verify(request))) {
        Ok(Ok(context)) => Ok(context),
        Ok(Err(error)) => Err(error),
        // Échec fermé : la panique est un refus, sans détail interne.
        Err(_) => Err(AuthError::new("verifier_panicked")),
    }
}

/// Verdict d'une décision d'autorisation — voir [`AuthDecisionEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthVerdict {
    /// L'appelant peut utiliser l'élément (droits détenus).
    Allowed,
    /// Un droit exigé manque (ou aucun droit n'est déclaré sur l'élément).
    RightMissing,
    /// La confirmation externe a refusé.
    ApprovalDenied,
    /// La confirmation externe n'a pas encore tranché — rien n'est exécuté.
    ApprovalPending,
    /// Aucune identité vérifiée n'accompagnait la requête.
    Unauthenticated,
}

/// Décision d'autorisation, publiée au journal de l'application par
/// [`crate::server::ServerBuilder::on_auth_decision`].
///
/// Le **motif précis** vit ici (côté application) ; le réseau ne reçoit que
/// `data.reason`, limité aux trois valeurs de la spécification.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct AuthDecisionEvent<'a> {
    /// Méthode JSON-RPC concernée (`tools/call`, `tools/list`…).
    pub method: &'a str,
    /// Élément nommé (outil, prompt, `uri` de ressource), si la méthode en
    /// porte un.
    pub name: Option<&'a str>,
    /// Identifiant du client, ou `None` quand aucune identité n'a été
    /// établie (verdict [`AuthVerdict::Unauthenticated`]).
    pub client: Option<&'a str>,
    /// Verdict rendu.
    pub verdict: AuthVerdict,
    /// Motif **précis** (`right_missing`, `approval_denied`,
    /// `approval_denied` d'une politique en panne, `unauthenticated`…), ou
    /// `None` pour un accès accordé.
    pub reason: Option<&'a str>,
}

/// Demande soumise à une [`ApprovalPolicy`] — exactement ce qui va
/// s'exécuter, jamais une copie reformatée.
#[non_exhaustive]
pub struct ApprovalRequest<'a> {
    /// Identifiant de l'appelant (voir [`AuthContext::client`]).
    pub client: &'a str,
    /// Nom de l'outil appelé.
    pub tool: &'a str,
    /// Arguments **tels que le handler les recevra** (après validation et
    /// injection des valeurs par défaut) : ce que l'application approuve est
    /// ce qui s'exécute.
    pub arguments: &'a Value,
    /// `id` JSON-RPC de la requête (nombre ou chaîne ; `null` si la requête
    /// n'en portait pas d'exploitable).
    pub request_id: &'a Value,
}

impl ApprovalRequest<'_> {
    /// `Debug` des arguments **tronqué** : un journal d'audit ne recopie pas
    /// des arguments arbitrairement gros venus du réseau.
    fn arguments_debug(&self) -> String {
        const MAX: usize = 2048;
        match serde_json::to_string(self.arguments) {
            Ok(mut texte) => {
                if texte.len() > MAX {
                    if let Some((cut, _)) = texte.char_indices().nth(MAX) {
                        texte.truncate(cut);
                        texte.push('…');
                    }
                }
                texte
            }
            Err(_) => "<arguments non sérialisables>".to_string(),
        }
    }
}

impl std::fmt::Debug for ApprovalRequest<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApprovalRequest")
            .field("client", &self.client)
            .field("tool", &self.tool)
            .field("arguments", &self.arguments_debug())
            .field("request_id", &self.request_id)
            .finish()
    }
}

/// Verdict d'une confirmation externe.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApprovalVerdict {
    /// Le handler peut s'exécuter — réponse normale.
    Approved,
    /// Rien ne s'exécute : `403`, `-32023`, `data.reason = "approval_denied"`.
    Denied,
    /// Rien n'est exécuté **ni mis en file** : `403`, `-32023`,
    /// `data.reason = "approval_pending"`, `data.retryable = true` et
    /// `data.retryAfterMs` quand un délai est fourni. Le client réessaie
    /// (même corps, nouvel `id`).
    Pending {
        /// Délai suggéré avant de réessayer.
        retry_after: Option<Duration>,
    },
}

/// Confirmation **externe** d'un appel d'outil — fournie par l'application
/// (écran de confirmation, politique interne, file d'attente humaine).
///
/// Interrogée **après** les droits et **avant** le handler, avec les
/// arguments exacts de l'appel. Le noyau ne conserve aucun état : un
/// `Pending` n'est ni mémorisé ni mis en file, c'est au client de réessayer.
///
/// # Panique
///
/// Une panique de [`ApprovalPolicy::decide`] est contenue et traitée comme
/// un refus (`Denied`) : le service continue, l'appel est refusé.
pub trait ApprovalPolicy: Send + Sync {
    /// Décide du sort de cet appel.
    fn decide(&self, request: &ApprovalRequest<'_>) -> ApprovalVerdict;
}

thread_local! {
    /// Contexte du dispatch en cours sur ce fil — `None` hors requête
    /// authentifiée. `RefCell` : le dispatch est synchrone, aucune
    /// réentrance concurrente sur un même fil.
    static CURRENT: RefCell<Option<AuthContext>> = const { RefCell::new(None) };
}

/// Identité du dispatch en cours sur ce fil, si elle existe.
///
/// C'est le chemin par lequel un handler (ou le hook
/// [`crate::server::ServerBuilder::on_call`]) lit l'appelant sans changer de
/// signature, et par lequel un dispatch imbriqué (handler appelant
/// [`crate::Gateway::dispatch`]) hérite du contexte. Rend une copie : la
/// valeur reste locale au tour courant, rien n'est partagé entre requêtes.
#[must_use]
pub fn current() -> Option<AuthContext> {
    CURRENT.with(|slot| slot.borrow().clone())
}

/// Garde de liaison : restaure le contexte précédent au `Drop`, y compris sur
/// un retour anticipé du dispatch.
pub(crate) struct Bound(Option<AuthContext>);

impl Drop for Bound {
    fn drop(&mut self) {
        let previous = self.0.take();
        CURRENT.with(|slot| *slot.borrow_mut() = previous);
    }
}

/// Lie (ou délie avec `None`) le contexte de ce fil pour la durée du
/// dispatch.
pub(crate) fn bind(context: Option<AuthContext>) -> Bound {
    Bound(CURRENT.with(|slot| slot.replace(context)))
}

/// Décision d'un [`TestVerifier`] — voir [`TestVerifier::new`].
type TestDecision =
    dyn for<'a> Fn(&VerifiedRequest<'a>) -> Result<AuthContext, AuthError> + Send + Sync;

/// Vérificateur de **test** — `#[doc(hidden)]`, réservé aux tests
/// d'intégration et aux harnais.
///
/// Il n'est pas une implémentation de référence : il ne prouve rien. Il
/// existe pour que les tests d'intégration n'aient pas à réécrire un
/// vérificateur à chaque fichier, et pour compter les appels.
#[doc(hidden)]
pub struct TestVerifier {
    decide: Arc<TestDecision>,
    calls: std::sync::atomic::AtomicUsize,
}

impl TestVerifier {
    /// Vérificateur dont la décision est la fonction fournie (elle peut
    /// paniquer : [`verify_or_refuse`] le contient).
    #[must_use]
    pub fn new<F>(decide: F) -> Self
    where
        F: for<'a> Fn(&VerifiedRequest<'a>) -> Result<AuthContext, AuthError>
            + Send
            + Sync
            + 'static,
    {
        Self {
            decide: Arc::new(decide),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Vérificateur qui accepte toute requête en rendant une copie de
    /// `context`.
    #[must_use]
    pub fn accepting(context: AuthContext) -> Self {
        Self::new(move |_request| Ok(context.clone()))
    }

    /// Nombre d'appels reçus — pour prouver qu'un refus n'a pas consulté le
    /// vérificateur, ou l'inverse.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Verifier for TestVerifier {
    fn verify(&self, request: &VerifiedRequest<'_>) -> Result<AuthContext, AuthError> {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        (self.decide)(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn requete() -> VerifiedRequest<'static> {
        static HEADERS: [(&str, &str); 3] = [
            ("Host", "127.0.0.1:23080"),
            ("Content-Digest", "sha-256=:AAAA:"),
            ("content-digest", "sha-256=:BBBB:"),
        ];
        VerifiedRequest::new(
            "POST",
            "/mcp",
            "127.0.0.1:23080",
            &HEADERS,
            b"{\"jsonrpc\":\"2.0\"}",
        )
    }

    #[test]
    fn les_droits_sont_tries_et_dedoublonnes() {
        let contexte = AuthContext::new("c", ProofMethod::Application)
            .with_right("db.write")
            .with_rights(["db.read", "db.write", "db.read"]);
        assert_eq!(contexte.rights(), ["db.read", "db.write"]);
        assert!(contexte.has_right("db.read"));
        assert!(contexte.has_right("db.write"));
        assert!(!contexte.has_right("db.admin"));
        // La comparaison est exacte : pas de préfixe, pas de casse ignorée.
        assert!(!contexte.has_right("db.rea"));
        assert!(!contexte.has_right("DB.READ"));
        assert!(!contexte.has_right(""));
    }

    #[test]
    fn un_contexte_neuf_est_vide_et_horodate() {
        let avant = SystemTime::now();
        let contexte = AuthContext::new("client", ProofMethod::Application);
        assert_eq!(contexte.client(), "client");
        assert_eq!(contexte.label(), None);
        assert!(contexte.rights().is_empty());
        assert_eq!(contexte.proof(), &ProofMethod::Application);
        assert!(contexte.verified_at() >= avant);
        // `with_verified_at` fixe l'instant : un vérificateur qui connaît
        // l'heure de sa vérification le remplace.
        let fixe = SystemTime::UNIX_EPOCH;
        assert_eq!(contexte.with_verified_at(fixe).verified_at(), fixe);
    }

    #[test]
    fn la_preuve_ne_porte_que_des_identifiants() {
        let signature = AuthContext::new(
            "c",
            ProofMethod::Signature {
                keyid: "client-demo".to_string(),
            },
        );
        let jeton = AuthContext::new(
            "c",
            ProofMethod::Token {
                issuer: "https://issuer.example".to_string(),
            },
        );
        assert!(format!("{:?}", signature.proof()).contains("client-demo"));
        assert!(format!("{:?}", jeton.proof()).contains("issuer.example"));
    }

    #[test]
    fn un_motif_d_erreur_est_borne_sans_couper_un_caractere() {
        let long = "é".repeat(MAX_REASON_CHARS + 40);
        let refus = AuthError::new(long);
        assert_eq!(refus.reason().chars().count(), MAX_REASON_CHARS);
        assert_eq!(AuthError::new("court").reason(), "court");
        assert_eq!(AuthError::new("").reason(), "");
        assert!(refus.to_string().starts_with("authentication refused:"));
    }

    #[test]
    fn un_en_tete_se_lit_sans_casse_et_toutes_les_occurrences_restent_visibles() {
        let requete = requete();
        assert_eq!(requete.header("content-digest"), Some("sha-256=:AAAA:"));
        assert_eq!(requete.header("CONTENT-DIGEST"), Some("sha-256=:AAAA:"));
        assert_eq!(requete.header("absent"), None);
        // Deux occurrences : c'est au vérificateur d'exiger l'unicité.
        assert_eq!(requete.header_values("Content-Digest").count(), 2);
    }

    #[test]
    fn le_debug_d_une_requete_ne_recopie_pas_le_corps() {
        let requete = requete();
        let texte = format!("{requete:?}");
        assert!(texte.contains("body_len: 17"), "{texte}");
        assert!(
            !texte.contains("jsonrpc"),
            "le corps ne doit pas fuir : {texte}"
        );
    }

    #[test]
    fn une_panique_de_verificateur_est_un_refus() {
        struct Defaillant;
        impl Verifier for Defaillant {
            fn verify(&self, _request: &VerifiedRequest<'_>) -> Result<AuthContext, AuthError> {
                panic!("bug");
            }
        }
        let refus = verify_or_refuse(&Defaillant, &requete()).expect_err("refus");
        assert_eq!(refus.reason(), "verifier_panicked");
    }

    #[test]
    fn un_verificateur_qui_refuse_rend_son_motif() {
        struct Refus;
        impl Verifier for Refus {
            fn verify(&self, _request: &VerifiedRequest<'_>) -> Result<AuthContext, AuthError> {
                Err(AuthError::new("signature_invalid"))
            }
        }
        let refus = verify_or_refuse(&Refus, &requete()).expect_err("refus");
        assert_eq!(refus.reason(), "signature_invalid");
    }

    #[test]
    fn le_contexte_courant_se_lie_et_se_restaure() {
        assert!(current().is_none(), "hors dispatch : aucun contexte");
        let externe = AuthContext::new("externe", ProofMethod::Application);
        {
            let _garde = bind(Some(externe.clone()));
            assert_eq!(current().map(|c| c.client), Some("externe".to_string()));
            {
                let interne = AuthContext::new("interne", ProofMethod::Application);
                let _garde = bind(Some(interne));
                assert_eq!(current().map(|c| c.client), Some("interne".to_string()));
            }
            // Le garde imbriqué a restauré le contexte extérieur.
            assert_eq!(current().map(|c| c.client), Some("externe".to_string()));
        }
        assert!(current().is_none(), "le garde a délié le fil");
    }

    #[test]
    fn le_verificateur_de_test_compte_ses_appels() {
        let verificateur = TestVerifier::new(|request: &VerifiedRequest<'_>| {
            request
                .header("x-client")
                .map(|client| AuthContext::new(client, ProofMethod::Application))
                .ok_or_else(|| AuthError::new("client_header_missing"))
        });
        let sans = VerifiedRequest::new("POST", "/mcp", "h", &[], b"");
        assert!(verify_or_refuse(&verificateur, &sans).is_err());
        let entetes: [(&str, &str); 1] = [("X-Client", "client-demo")];
        let avec = VerifiedRequest::new("POST", "/mcp", "h", &entetes, b"");
        let contexte = verify_or_refuse(&verificateur, &avec).expect("accepté");
        assert_eq!(contexte.client(), "client-demo");
        assert_eq!(verificateur.calls(), 2);
    }

    #[test]
    fn le_debug_d_une_demande_d_approbation_tronque_les_arguments() {
        let gros = Value::String("x".repeat(8192));
        let id = Value::from(7);
        let demande = ApprovalRequest {
            client: "c",
            tool: "t",
            arguments: &gros,
            request_id: &id,
        };
        let texte = format!("{demande:?}");
        assert!(
            texte.len() < 4096,
            "arguments non bornés : {} octets",
            texte.len()
        );
        assert!(texte.contains("client: \"c\""));
    }
}
