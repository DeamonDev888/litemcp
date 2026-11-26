//! Signature et vérification de requêtes — profil **strict** RFC 9421 +
//! RFC 9530, Ed25519 pur (feature `auth-sign`).
//!
//! Ce module est le côté « qui prouve quoi » du contrat de
//! [`crate::auth`] : [`Signer`] produit les trois en-têtes d'une requête
//! (client), [`SignatureVerifier`] les vérifie et rend un
//! [`AuthContext`] (serveur). L'application fournit
//! le magasin de clés ([`KeyStore`]) ; le crate ne stocke aucune clé.
//!
//! # Ce qui est signé
//!
//! Liste **fermée** et ordre **imposé** (`docs/auth.md` §2.2) :
//!
//! ```text
//! ("@method" "@path" "@authority" "content-digest" "content-type" "mcp-method" "mcp-name")
//! ```
//!
//! ou la même **moins** `"mcp-name"` (six composants) quand la requête n'en
//! porte pas. Aucun paramètre de composant, aucune répétition, aucun autre
//! ordre. Les cinq paramètres de signature sont `created`, `nonce`, `keyid`,
//! `alg="ed25519"`, `tag="litemcp-auth-v1"`, dans cet ordre ; pas d'`expires`.
//!
//! # Ordre des contrôles (du moins cher au plus cher)
//!
//! 1. en-têtes présents, uniques, non vides, `Mcp-Param-*` absent ;
//! 2. composants et paramètres (`Signature-Input`), étiquette ;
//! 3. `Content-Digest` comparé au corps reçu ;
//! 4. cohérence `Mcp-Method`/`Mcp-Name` avec le corps (sentinelle
//!    `=?base64?…?=` de `Mcp-Name` décodée avant la comparaison) ;
//! 5. fenêtre d'horloge ±60 s ;
//! 6. nonce déjà vu ;
//! 7. `keyid` → [`KeyStore`], statut ;
//! 8. **Ed25519 en dernier** ;
//! 9. réservation du nonce, seulement après une vérification réussie : une clé
//!    publique ne suffit donc pas à gonfler le cache de nonces.
//!
//! # Uniformité du refus
//!
//! « clé inconnue », « clé révoquée » et « signature fausse » rendent le même
//! [`AuthError`] au réseau : le transport (lot L3) en fait un `401` de corps
//! constant et `WWW-Authenticate: ` [`WWW_AUTHENTICATE`]. Les motifs précis
//! (`key_unknown`, `key_revoked`, `signature_invalid`) ne servent qu'au
//! journal de l'application.
//!
//! # Aléa
//!
//! Les nonces viennent du **CSPRNG du système d'exploitation** (`getrandom`),
//! comme les clés Ed25519 et la clé HMAC de `crate::request_state`. Aucun
//! générateur ensemencé, aucune horloge comme graine : `RandomState` (SipHash)
//! n'est pas un générateur de clés — la std n'en tire les graines qu'une fois
//! par fil, et l'algorithme n'est pas spécifié.
//!
//! # Exemple
//!
//! ```
//! use litemcp::auth::sign::{
//!     ClientKey, KeyStore, NonceCache, RequestToSign, SignatureVerifier, Signer, SigningKey,
//!     TestKeyStore,
//! };
//! use litemcp::auth::{ProofMethod, VerifiedRequest, Verifier};
//! use std::sync::Arc;
//!
//! // Graine du vecteur 1 de la RFC 8032 §7.1 (publique : c'est un vecteur).
//! let seed = [
//!     0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c, 0xc4,
//!     0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae, 0x7f, 0x60,
//! ];
//! let key = SigningKey::from_seed(&seed);
//! let signer = Signer::new(key).with_keyid("client-demo");
//!
//! let body = br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"db.query"}}"#;
//! let headers = [
//!     ("Host", "127.0.0.1:23080"),
//!     ("Content-Type", "application/json"),
//!     ("Mcp-Method", "tools/call"),
//!     ("Mcp-Name", "db.query"),
//! ];
//! let request = RequestToSign::new("POST", "/mcp", "127.0.0.1:23080", &headers, body);
//! // Instant et nonce imposés : reproductible, donc lisible par un tiers.
//! let nonce = [0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
//! let signed = signer.headers_at(&request, 1_794_700_800, &nonce);
//! assert!(signed.is_complete());
//!
//! let store = TestKeyStore::new();
//! store.insert(
//!     "client-demo",
//!     ClientKey::new("client-demo", signer.verifying_key().to_bytes()).with_right("db.read"),
//! );
//! let keys: Arc<dyn KeyStore> = Arc::new(store);
//! let verifier = SignatureVerifier::new(keys, NonceCache::with_default_policy())
//!     .with_clock(Arc::new(|| 1_794_700_800));
//!
//! let mut received = headers.to_vec();
//! received.extend(signed.iter());
//! let verified = verifier
//!     .verify(&VerifiedRequest::new(
//!         "POST",
//!         "/mcp",
//!         "127.0.0.1:23080",
//!         &received,
//!         body,
//!     ))
//!     .expect("signature valide");
//! assert_eq!(verified.client(), "client-demo");
//! assert!(verified.has_right("db.read"));
//! assert!(matches!(
//!     verified.proof(),
//!     ProofMethod::Signature { keyid } if keyid == "client-demo"
//! ));
//! ```

mod base64;
mod headers;
mod key;
mod nonce;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::auth::{AuthContext, AuthError, ProofMethod, VerifiedRequest, Verifier};

pub use key::{Fingerprint, SigningKey, VerifyingKey};
use nonce::NonceInsert;
pub use nonce::{DEFAULT_PER_KEY, DEFAULT_TTL, Nonce, NonceCache};

use headers::{Component, ComponentValues, WITH_NAME, WITHOUT_NAME};

/// Étiquette de signature fixée par le profil.
pub const TAG: &str = "litemcp-auth-v1";
/// Algorithme admis, et lui seul.
pub const ALG: &str = "ed25519";
/// Étiquette du membre : un seul membre, toujours celui-ci.
const LABEL: &str = "sig1";
/// Demi-fenêtre d'horloge admise, en secondes (`docs/auth.md` §2.3).
const CLOCK_WINDOW_SECONDS: u64 = 60;
/// Statut HTTP de tout refus d'authentification du profil.
pub use crate::auth::UNAUTHORIZED_STATUS;
/// Valeur exacte de l'en-tête des `401` (`docs/auth.md` §2.7) — définie par
/// le noyau ([`crate::auth::WWW_AUTHENTICATE`]) pour que le transport et le
/// profil de signature ne puissent pas diverger.
pub use crate::auth::WWW_AUTHENTICATE;

/// État d'une clé inscrite au magasin de l'application.
///
/// La révocation prend effet **à la requête suivante** : [`KeyStore::lookup`]
/// est consulté à chaque vérification, aucun résultat n'est mis en cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeyStatus {
    /// La clé peut signer.
    Active,
    /// La clé est refusée (`key_revoked`), sans délai.
    Revoked,
}

/// Clé publique d'un client, telle que l'application la déclare.
///
/// Le `keyid` n'est **pas** recopié ici : c'est la clé du magasin, et la lib ne
/// le dérive jamais de la clé publique (`docs/auth.md` §3).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ClientKey {
    /// Identifiant applicatif rendu dans l'[`AuthContext`].
    pub client: String,
    /// Libellé lisible (nom d'écran), jamais utilisé pour décider.
    pub label: Option<String>,
    /// Clé publique Ed25519 (32 octets comprimés).
    pub public_key: [u8; 32],
    /// Droits détenus, chaînes opaques déclarées par l'application.
    pub rights: Vec<String>,
    /// Actif ou révoqué.
    pub status: KeyStatus,
}

impl ClientKey {
    /// Clé active, sans libellé ni droit.
    #[must_use]
    pub fn new(client: impl Into<String>, public_key: [u8; 32]) -> Self {
        Self {
            client: client.into(),
            label: None,
            public_key,
            rights: Vec::new(),
            status: KeyStatus::Active,
        }
    }

    /// Ajoute un libellé lisible.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Ajoute un droit.
    #[must_use]
    pub fn with_right(mut self, right: impl Into<String>) -> Self {
        self.rights.push(right.into());
        self
    }

    /// Ajoute plusieurs droits.
    #[must_use]
    pub fn with_rights<I, S>(mut self, rights: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.rights.extend(rights.into_iter().map(Into::into));
        self
    }

    /// Marque la clé comme révoquée.
    #[must_use]
    pub fn revoked(mut self) -> Self {
        self.status = KeyStatus::Revoked;
        self
    }
}

/// Magasin de clés publiques, **fourni par l'application**.
///
/// `lookup` est appelé à chaque requête : c'est ce qui rend une révocation
/// immédiate (à la requête suivante). Un `None` est un refus (`key_unknown`),
/// jamais un laissez-passer.
pub trait KeyStore: Send + Sync {
    /// Clé associée à `keyid`, ou `None` si le `keyid` est inconnu.
    fn lookup(&self, keyid: &str) -> Option<ClientKey>;
}

/// Magasin de **test** — `#[doc(hidden)]`, réservé aux tests d'intégration et
/// aux harnais.
///
/// Il n'est pas une implémentation de référence (aucune persistance, aucun
/// chiffrement au repos) : une application fournit son propre [`KeyStore`].
#[doc(hidden)]
#[derive(Default)]
pub struct TestKeyStore {
    keys: std::sync::RwLock<std::collections::HashMap<String, ClientKey>>,
}

impl TestKeyStore {
    /// Magasin vide.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inscrit (ou remplace) une clé sous `keyid`.
    pub fn insert(&self, keyid: &str, key: ClientKey) {
        if let Ok(mut keys) = self.keys.write() {
            keys.insert(keyid.to_owned(), key);
        }
    }

    /// Révoque une clé inscrite ; sans effet si elle est inconnue.
    pub fn revoke(&self, keyid: &str) {
        if let Ok(mut keys) = self.keys.write() {
            if let Some(key) = keys.get_mut(keyid) {
                key.status = KeyStatus::Revoked;
            }
        }
    }

    /// Retire une clé du magasin.
    pub fn remove(&self, keyid: &str) {
        if let Ok(mut keys) = self.keys.write() {
            keys.remove(keyid);
        }
    }
}

impl KeyStore for TestKeyStore {
    fn lookup(&self, keyid: &str) -> Option<ClientKey> {
        self.keys
            .read()
            .ok()
            .and_then(|keys| keys.get(keyid).cloned())
    }
}

/// Requête à signer : méthode, chemin, autorité, en-têtes **tels qu'ils
/// partiront** et corps exact.
///
/// `path` peut porter une query : elle n'est pas signée (`@path` s'arrête au
/// `?`). `authority` est normalisée (minuscules, port par défaut omis) avant
/// signature.
#[derive(Clone, Copy)]
pub struct RequestToSign<'a> {
    /// Méthode HTTP (`POST` pour le profil MCP).
    pub method: &'a str,
    /// Chemin absolu, query exclue.
    pub path: &'a str,
    /// Autorité (`Host`).
    pub authority: &'a str,
    /// En-têtes de la requête, dans l'ordre d'envoi.
    pub headers: &'a [(&'a str, &'a str)],
    /// Corps exact qui sera transmis.
    pub body: &'a [u8],
}

impl<'a> RequestToSign<'a> {
    /// Assemble une requête à signer.
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

    /// Première valeur de l'en-tête `name` (insensible à la casse ASCII).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&'a str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| *value)
    }

    /// Nombre d'occurrences de l'en-tête `name`.
    fn header_count(&self, name: &str) -> usize {
        self.headers
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case(name))
            .count()
    }

    /// Valeur unique d'un en-tête exigé : `None` s'il est absent, dupliqué ou
    /// vide.
    fn unique_header(&self, name: &str) -> Option<&'a str> {
        let mut values = self
            .headers
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case(name));
        let first = values.next()?;
        if values.next().is_some() {
            return None;
        }
        let value = trim_ows(first.1);
        if value.is_empty() {
            return None;
        }
        Some(value)
    }
}

impl std::fmt::Debug for RequestToSign<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestToSign")
            .field("method", &self.method)
            .field("path", &self.path)
            .field("authority", &self.authority)
            .field("headers", &self.headers.len())
            .field("body_len", &self.body.len())
            .finish()
    }
}

/// Les trois en-têtes produits par [`Signer`].
///
/// [`SignedHeaders::iter`] les rend dans l'ordre de `docs/auth.md` §2.1 :
/// `Content-Digest`, `Signature-Input`, `Signature`.
#[derive(Clone, PartialEq, Eq)]
pub struct SignedHeaders {
    content_digest: String,
    signature_input: String,
    signature: String,
    complete: bool,
}

impl SignedHeaders {
    /// `Content-Digest: sha-256=:<base64 standard>:`
    #[must_use]
    pub fn content_digest(&self) -> &str {
        &self.content_digest
    }

    /// `Signature-Input: sig1=(…)…`
    #[must_use]
    pub fn signature_input(&self) -> &str {
        &self.signature_input
    }

    /// `Signature: sig1=:<base64 standard>:`
    #[must_use]
    pub fn signature(&self) -> &str {
        &self.signature
    }

    /// La requête portait-elle tout ce que le profil exige pour être
    /// vérifiable ?
    ///
    /// `false` signale un `keyid` absent ou hors grammaire, un `Content-Type`
    /// ou un `Mcp-Method` absent, dupliqué ou vide : les en-têtes produits
    /// sont alors **refusés** par le vérificateur (échec fermé), jamais
    /// acceptés à moitié.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// Les trois paires `(nom, valeur)`, prêtes à être ajoutées à la requête.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        [
            ("Content-Digest", self.content_digest.as_str()),
            ("Signature-Input", self.signature_input.as_str()),
            ("Signature", self.signature.as_str()),
        ]
        .into_iter()
    }
}

impl std::fmt::Debug for SignedHeaders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignedHeaders")
            .field("content_digest", &self.content_digest)
            .field("signature_input", &self.signature_input)
            .field("signature", &self.signature)
            .field("complete", &self.complete)
            .finish()
    }
}

/// Signeur côté client : clé privée Ed25519 + requête → en-têtes du profil.
///
/// ```
/// use litemcp::auth::sign::{RequestToSign, Signer, SigningKey};
///
/// let signer = Signer::new(SigningKey::generate()).with_keyid("poste-1");
/// let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
/// let headers = [
///     ("Host", "127.0.0.1:23080"),
///     ("Content-Type", "application/json"),
///     ("Mcp-Method", "tools/list"),
/// ];
/// let request = RequestToSign::new("POST", "/mcp", "127.0.0.1:23080", &headers, body);
/// let signed = signer.headers(&request);
/// assert!(signed.is_complete());
/// assert_eq!(signed.iter().count(), 3);
/// ```
pub struct Signer {
    key: SigningKey,
    keyid: String,
}

impl Signer {
    /// Signeur sans `keyid` : la requête produite est **incomplète** tant que
    /// [`Signer::with_keyid`] n'a pas fourni l'identifiant public du client.
    #[must_use]
    pub fn new(key: SigningKey) -> Self {
        Self {
            key,
            keyid: String::new(),
        }
    }

    /// Fixe le `keyid` annoncé dans `Signature-Input` (grammaire de
    /// `docs/auth.md` §3). Il est choisi par l'application : la lib ne le
    /// dérive jamais de la clé.
    #[must_use]
    pub fn with_keyid(mut self, keyid: impl Into<String>) -> Self {
        self.keyid = keyid.into();
        self
    }

    /// Clé publique correspondante (à inscrire au magasin du serveur).
    #[must_use]
    pub fn verifying_key(&self) -> VerifyingKey {
        self.key.verifying_key()
    }

    /// Empreinte de la clé publique.
    #[must_use]
    pub fn fingerprint(&self) -> Fingerprint {
        self.key.fingerprint()
    }

    /// En-têtes pour cette requête, avec l'horloge murale et un nonce neuf du
    /// générateur du système.
    #[must_use]
    pub fn headers(&self, request: &RequestToSign<'_>) -> SignedHeaders {
        self.headers_at(request, unix_now(), &nonce::system_nonce())
    }

    /// En-têtes avec `created` et `nonce` **imposés** — vecteurs de test,
    /// clients qui gèrent eux-mêmes l'horloge et le rejeu.
    ///
    /// Le nonce fourni doit être neuf pour ce `keyid` : le serveur refuse un
    /// nonce déjà vu, et cette fonction ne tient aucun état.
    #[must_use]
    pub fn headers_at(
        &self,
        request: &RequestToSign<'_>,
        created: u64,
        nonce: &Nonce,
    ) -> SignedHeaders {
        let content_type = request.unique_header("content-type");
        let mcp_method = request.unique_header("mcp-method");
        let mcp_name = request.unique_header("mcp-name");
        // Un `Mcp-Name` dupliqué n'a pas de valeur unique : la requête n'est
        // pas signable, même si la première occurrence est exploitable.
        let mcp_name_unique = request.header_count("mcp-name") <= 1;

        let digest = crate::request_state::sha256(request.body);
        let content_digest = format!("sha-256=:{}:", base64::encode(&digest));

        let path = normalize_path(request.path);
        let authority = normalize_authority(request.authority);
        let components: &[Component] = if mcp_name.is_some() {
            &WITH_NAME
        } else {
            &WITHOUT_NAME
        };
        let params = signature_params(components, created, nonce, &self.keyid);
        let values = ComponentValues {
            method: request.method,
            path,
            authority: &authority,
            content_digest: &content_digest,
            content_type: content_type.unwrap_or(""),
            mcp_method: mcp_method.unwrap_or(""),
            mcp_name,
        };
        let base = headers::build_base(components, &values, &params);
        let signature = match &base {
            Ok(base) => base64::encode(&self.key.sign_message(base.as_bytes())),
            // Requête incomplète : la base n'est pas constructible. On signe
            // une chaîne vide, que le serveur refusera — jamais d'en-tête
            // « valide » sur une requête qui ne l'est pas.
            Err(_) => base64::encode(&self.key.sign_message(b"")),
        };

        SignedHeaders {
            content_digest,
            signature_input: format!("{LABEL}={params}"),
            signature: format!("{LABEL}=:{signature}:"),
            complete: content_type.is_some()
                && mcp_method.is_some()
                && mcp_name_unique
                && headers::keyid_allowed(&self.keyid)
                && base.is_ok(),
        }
    }
}

/// Vérificateur du profil : [`KeyStore`] fourni par l'application, cache de
/// nonces borné par clé.
///
/// Il implémente [`Verifier`] : le transport l'appelle avant le dispatch, et
/// un `Err` devient un `401` de corps constant (le motif précis reste côté
/// application).
pub struct SignatureVerifier {
    keys: Arc<dyn KeyStore>,
    nonces: NonceCache,
    clock: Clock,
}

/// Source de temps de la vérification (secondes UNIX).
type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

impl SignatureVerifier {
    /// Vérificateur adossé à `keys`, avec le cache de nonces fourni.
    ///
    /// Le cache doit être **partagé** entre toutes les requêtes d'un même
    /// serveur (un par processus, pas un par requête) : c'est lui qui détecte
    /// les rejeux.
    #[must_use]
    pub fn new(keys: Arc<dyn KeyStore>, nonces: NonceCache) -> Self {
        Self {
            keys,
            nonces,
            clock: Arc::new(unix_now),
        }
    }

    /// Remplace la source de temps (**tests** et environnements à horloge
    /// maîtrisée uniquement).
    ///
    /// Une horloge figée désactive la fenêtre de ±60 s : ne l'utiliser que
    /// dans un harnais.
    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// Cache de nonces utilisé (état partagé, lecture seule).
    #[must_use]
    pub fn nonces(&self) -> &NonceCache {
        &self.nonces
    }

    /// Vérifie la requête et établit l'identité.
    ///
    /// # Erreurs
    ///
    /// [`AuthError`] dont le motif appartient à la liste fermée de
    /// `docs/auth.md` §2.7. Aucun motif ne contient de secret : ni clé, ni
    /// nonce, ni signature.
    fn authenticate(&self, request: &VerifiedRequest<'_>) -> Result<AuthContext, AuthError> {
        // 1. En-têtes : présence, unicité, valeur non vide.
        let signature_input = unique_header(
            request,
            "Signature-Input",
            "signature_input_missing",
            "multiple_signatures",
        )?;
        let signature_header = unique_header(
            request,
            "Signature",
            "signature_missing",
            "multiple_signatures",
        )?;
        let content_digest = unique_header(
            request,
            "Content-Digest",
            "content_digest_missing",
            "content_digest_mismatch",
        )?;
        let content_type = required_header(request, "Content-Type")?;
        let mcp_method = required_header(request, "Mcp-Method")?;
        let mcp_name = optional_header(request, "Mcp-Name")?;
        // `Mcp-Param-*` : réservé, refusé tant qu'il n'est pas signable.
        if request.headers.iter().any(|(name, _)| is_mcp_param(name)) {
            return Err(AuthError::new("mcp_param_present"));
        }

        // 2. Composants et paramètres.
        let input = headers::parse_signature_input(signature_input)?;
        if !headers::covered_set_allowed(&input.components) {
            return Err(AuthError::new("covered_set_not_allowed"));
        }
        // `mcp-name` est couvert si et seulement si la requête en porte un.
        let covered_name = input.components.contains(&Component::McpName);
        if covered_name != mcp_name.is_some() {
            return Err(AuthError::new("covered_set_not_allowed"));
        }
        let (label, payload) = headers::parse_signature(signature_header)?;
        if label != input.label {
            return Err(AuthError::new("label_mismatch"));
        }
        let signature: [u8; 64] =
            base64::decode_array(payload).map_err(|_| AuthError::new("signature_invalid"))?;

        // 3. Corps : le digest annoncé est comparé aux octets reçus.
        let claimed = headers::parse_content_digest(content_digest)?;
        if !ct_eq(&claimed, &crate::request_state::sha256(request.body)) {
            return Err(AuthError::new("content_digest_mismatch"));
        }

        // 4. Les en-têtes de routage SEP-2243 ne doivent pas contredire le
        //    corps que la signature vient de sceller.
        check_mcp_binding(request.body, mcp_method, mcp_name)?;

        // 5. Fenêtre d'horloge (±60 s, arithmétique saturante).
        let now = (self.clock)();
        if input.created < now.saturating_sub(CLOCK_WINDOW_SECONDS)
            || input.created > now.saturating_add(CLOCK_WINDOW_SECONDS)
        {
            return Err(AuthError::new("clock_skew"));
        }

        // 6. Nonce déjà vu pour cette clé.
        if self.nonces.contains(input.keyid, &input.nonce) {
            return Err(AuthError::new("nonce_replay"));
        }

        // 7. Clé : inconnue ou révoquée.
        let key = self
            .keys
            .lookup(input.keyid)
            .ok_or_else(|| AuthError::new("key_unknown"))?;
        if key.status == KeyStatus::Revoked {
            return Err(AuthError::new("key_revoked"));
        }
        let verifying =
            VerifyingKey::from_bytes(&key.public_key).map_err(|_| AuthError::new("key_unknown"))?;

        // 8. Ed25519 en dernier : tout ce qui précède est moins cher.
        let values = ComponentValues {
            method: request.method,
            path: normalize_path(request.path),
            authority: &normalize_authority(request.authority),
            content_digest,
            content_type,
            mcp_method,
            mcp_name,
        };
        let base = headers::build_base(&input.components, &values, input.params)?;
        if !verifying.verify_message(base.as_bytes(), &signature) {
            return Err(AuthError::new("signature_invalid"));
        }

        // 9. Réservation du nonce, après succès : le cache ne peut être gonflé
        //    qu'avec des signatures valides. Un nonce encore dans la fenêtre
        //    n'est jamais évincé : compartiment plein = refus (échec fermé).
        match self.nonces.insert(input.keyid, &input.nonce) {
            // Deux requêtes identiques concurrentes : la seconde est un rejeu.
            NonceInsert::Replay => return Err(AuthError::new("nonce_replay")),
            NonceInsert::CacheFull => return Err(AuthError::new("nonce_cache_full")),
            NonceInsert::Fresh => {}
        }

        let mut context = AuthContext::new(
            key.client,
            ProofMethod::Signature {
                keyid: input.keyid.to_owned(),
            },
        )
        .with_rights(key.rights)
        .with_verified_at(SystemTime::now());
        if let Some(label) = key.label {
            context = context.with_label(label);
        }
        Ok(context)
    }
}

impl Verifier for SignatureVerifier {
    fn verify(&self, request: &VerifiedRequest<'_>) -> Result<AuthContext, AuthError> {
        self.authenticate(request)
    }
}

impl std::fmt::Debug for SignatureVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignatureVerifier")
            .field("nonces", &self.nonces)
            .finish_non_exhaustive()
    }
}

/// Le nom est-il un `Mcp-Param-*` ? (comparaison ASCII insensible à la casse)
fn is_mcp_param(name: &str) -> bool {
    name.as_bytes()
        .get(..10)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"mcp-param-"))
}

/// En-tête exigé, une seule occurrence, valeur non vide.
fn unique_header<'a>(
    request: &VerifiedRequest<'a>,
    name: &str,
    missing: &str,
    duplicated: &str,
) -> Result<&'a str, AuthError> {
    let mut values = request.header_values(name);
    let Some(first) = values.next() else {
        return Err(AuthError::new(missing));
    };
    // Deux occurrences, même si l'une est vide : duplication.
    if values.next().is_some() {
        return Err(AuthError::new(duplicated));
    }
    let first = trim_ows(first);
    if first.is_empty() {
        return Err(AuthError::new(missing));
    }
    Ok(first)
}

/// En-tête couvert obligatoire : absent, dupliqué ou vide = jeu de composants
/// non satisfiable.
fn required_header<'a>(request: &VerifiedRequest<'a>, name: &str) -> Result<&'a str, AuthError> {
    unique_header(
        request,
        name,
        "covered_set_not_allowed",
        "covered_set_not_allowed",
    )
}

/// En-tête couvert facultatif (`Mcp-Name`) : `None` s'il est absent ou vide,
/// refus s'il est dupliqué.
fn optional_header<'a>(
    request: &VerifiedRequest<'a>,
    name: &str,
) -> Result<Option<&'a str>, AuthError> {
    if request.header_values(name).count() > 1 {
        return Err(AuthError::new("covered_set_not_allowed"));
    }
    Ok(request
        .header_values(name)
        .map(trim_ows)
        .find(|value| !value.is_empty()))
}

/// Champ du corps qui porte le nom routé par `Mcp-Name` (SEP-2243).
///
/// `resources/read` route sur l'URI de la ressource, `tools/call` et
/// `prompts/get` sur le nom de l'élément ; les autres méthodes n'ont pas de
/// nom à router (le transport y interdit l'en-tête).
fn mcp_name_field(method: &str) -> Option<&'static str> {
    match method {
        "tools/call" | "prompts/get" => Some("name"),
        "resources/read" => Some("uri"),
        _ => None,
    }
}

/// `Mcp-Method` (et `Mcp-Name`) doivent correspondre au corps scellé.
///
/// La liaison de `Mcp-Name` est **consciente de la méthode** : `params.uri`
/// pour `resources/read`, `params.name` pour `tools/call` et `prompts/get`.
/// Un `Mcp-Name` présent sur une méthode qui n'en admet pas est refusé.
///
/// En profil moderne (SEP-2243), un nom non-ASCII voyage encodé en
/// `=?base64?…?=` dans `Mcp-Name` : la liaison décode la sentinelle avant
/// de comparer (une valeur non décodable est un refus). La base de
/// signature, elle, porte la valeur **brute** reçue — la liaison n'altère
/// jamais ce qui a été scellé.
///
/// Le motif est `covered_set_not_allowed` : la liste fermée de §2.7 n'a pas de
/// motif dédié à une contradiction en-tête/corps, et un composant couvert qui
/// ne décrit pas la requête ne peut pas être accepté.
fn check_mcp_binding(
    body: &[u8],
    mcp_method: &str,
    mcp_name: Option<&str>,
) -> Result<(), AuthError> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| AuthError::new("covered_set_not_allowed"))?;
    let method = value
        .get("method")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| AuthError::new("covered_set_not_allowed"))?;
    if method != mcp_method {
        return Err(AuthError::new("covered_set_not_allowed"));
    }
    if let Some(name) = mcp_name {
        let Some(field) = mcp_name_field(mcp_method) else {
            return Err(AuthError::new("covered_set_not_allowed"));
        };
        let declared = value
            .get("params")
            .and_then(|params| params.get(field))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| AuthError::new("covered_set_not_allowed"))?;
        // SEP-2243 : la liaison compare la valeur **décodée** de la
        // sentinelle, jamais la sentinelle brute.
        let decoded = decode_mcp_name(name)?;
        if declared != decoded {
            return Err(AuthError::new("covered_set_not_allowed"));
        }
    }
    Ok(())
}

/// Décode la sentinelle `=?base64?…?=` de `Mcp-Name` (SEP-2243).
///
/// Un nom d'outil/prompt ou une URI non-ASCII voyage encodée dans l'en-tête :
/// c'est la valeur **décodée** qui désigne l'élément appelé. Hors sentinelle,
/// la valeur est rendue telle quelle (OWS de bord compris) : un nom ASCII
/// se compare directement, comme avant.
///
/// Une valeur non décodable (base64 invalide, octets non UTF-8) est un échec
/// de liaison : le motif reste `covered_set_not_allowed`, comme toute
/// contradiction en-tête/corps.
///
/// Même grammaire et même base64 standard que le décodeur du transport
/// (`decode_mcp_header_value` dans `src/server/dispatch.rs`) : le profil de
/// signature est auto-contenu — il possède déjà son décodeur base64 strict —
/// et `dispatch` est un module privé du noyau, les deux décodeurs restent
/// donc volontairement en phase, sentinelle pour sentinelle.
fn decode_mcp_name(raw: &str) -> Result<String, AuthError> {
    const SENTINELLE: &[u8] = b"=?base64?";
    let raw_trimmed = raw.trim();
    // La comparaison se fait sur les octets : un préfixe non-ASCII ne peut
    // jamais y ressembler, et aucune frontière de caractère n'est coupée.
    if raw_trimmed.len() >= 11
        && raw_trimmed.as_bytes()[..9].eq_ignore_ascii_case(SENTINELLE)
        && raw_trimmed.ends_with("?=")
    {
        let encoded = &raw_trimmed[9..raw_trimmed.len() - 2];
        let bytes =
            base64::decode(encoded).map_err(|_| AuthError::new("covered_set_not_allowed"))?;
        String::from_utf8(bytes).map_err(|_| AuthError::new("covered_set_not_allowed"))
    } else {
        Ok(raw_trimmed.to_string())
    }
}

/// Retire les OWS de bord d'une valeur d'en-tête (RFC 9110 §5.5).
fn trim_ows(value: &str) -> &str {
    value.trim_matches([' ', '\t'])
}

/// Autorité normalisée : minuscules ASCII, port par défaut omis.
///
/// Les ports par défaut sont `80` et `443` : le profil ne connaît pas le
/// schéma, et les deux seules autorités qui perdent leur port sont celles-là.
/// Signeur et vérificateur appliquent **la même** fonction, donc un client
/// conforme est accepté quel que soit le côté qui voit `Host: example.com:443`.
fn normalize_authority(authority: &str) -> String {
    let authority = trim_ows(authority).to_ascii_lowercase();
    let (host, port) = split_authority(&authority);
    match port {
        Some(port) if port == "80" || port == "443" => host.to_owned(),
        _ => authority,
    }
}

/// Sépare hôte et port, en respectant la forme `[::1]:443`.
fn split_authority(authority: &str) -> (&str, Option<&str>) {
    if let Some(rest) = authority.strip_prefix('[') {
        let Some(close) = rest.find(']') else {
            return (authority, None);
        };
        let host_end = close + 2;
        let Some(tail) = authority.get(host_end..) else {
            return (authority, None);
        };
        return match tail.strip_prefix(':') {
            Some(port) => (authority.get(..host_end).unwrap_or(authority), Some(port)),
            None => (authority, None),
        };
    }
    match authority.rsplit_once(':') {
        Some((host, port))
            if !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            (host, Some(port))
        }
        _ => (authority, None),
    }
}

/// `@path` : chemin absolu sans query, `/` si vide, `%XX` **non décodés**.
fn normalize_path(path: &str) -> &str {
    let without_query = match path.find('?') {
        Some(index) => path.get(..index).unwrap_or(path),
        None => path,
    };
    if without_query.is_empty() {
        "/"
    } else {
        without_query
    }
}

/// Texte du membre `Signature-Input`, après `label=`.
fn signature_params(components: &[Component], created: u64, nonce: &Nonce, keyid: &str) -> String {
    let mut params = String::with_capacity(256);
    params.push('(');
    for (index, component) in components.iter().enumerate() {
        if index > 0 {
            params.push(' ');
        }
        params.push('"');
        params.push_str(component.name());
        params.push('"');
    }
    params.push(')');
    params.push_str(";created=");
    params.push_str(&created.to_string());
    params.push_str(";nonce=\"");
    params.push_str(&nonce::encode_nonce(nonce));
    params.push_str("\";keyid=\"");
    params.push_str(keyid);
    params.push_str("\";alg=\"");
    params.push_str(ALG);
    params.push_str("\";tag=\"");
    params.push_str(TAG);
    params.push('"');
    params
}

/// Comparaison en temps constant (même longueur, pas de sortie anticipée).
///
/// Utilisée pour les empreintes et les digests : le résultat ne dépend que de
/// l'égalité, jamais de la position du premier octet différent.
pub(crate) fn ct_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right) {
        difference |= a ^ b;
    }
    difference == 0
}

/// Octets du **CSPRNG du système d'exploitation** (`getrandom`).
///
/// Source des clés Ed25519 et des nonces : l'API d'entropie de l'OS, sans
/// générateur ensemencé ni horloge. Voir la section « Aléa » du module.
///
/// # Panique
///
/// Panique si l'OS ne fournit pas d'entropie : un serveur qui ne peut pas
/// tirer une clé ne doit pas démarrer avec une clé faible.
pub(crate) fn system_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    getrandom::fill(&mut out).expect("the OS entropy source must provide keys and nonces");
    out
}

/// Secondes UNIX de l'horloge murale, `0` si l'horloge précède l'époque.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vecteur 1 de la RFC 8032 §7.1 (graine publique).
    const SEED: [u8; 32] = [
        0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c,
        0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae,
        0x7f, 0x60,
    ];
    const CREATED: u64 = 1_794_700_800;
    const NONCE: Nonce = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f,
    ];

    const BODY: &[u8] = br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"db.query","arguments":{"sql":"select 1"},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}"#;

    fn entetes() -> [(&'static str, &'static str); 4] {
        [
            ("Host", "127.0.0.1:23080"),
            ("Content-Type", "application/json"),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "db.query"),
        ]
    }

    fn signeur() -> Signer {
        Signer::new(SigningKey::from_seed(&SEED)).with_keyid("client-demo")
    }

    #[test]
    fn le_vecteur_de_reference_produit_exactement_les_trois_entetes() {
        let entetes = entetes();
        let req = RequestToSign::new("POST", "/mcp", "127.0.0.1:23080", &entetes, BODY);
        let signed = signeur().headers_at(&req, CREATED, &NONCE);
        assert_eq!(
            signed.content_digest(),
            "sha-256=:JTvMexIVHBnIyJ6QrKgnW6o2OifuQwJxUf7qaLvtlM0=:"
        );
        assert_eq!(
            signed.signature_input(),
            "sig1=(\"@method\" \"@path\" \"@authority\" \"content-digest\" \"content-type\" \"mcp-method\" \"mcp-name\");created=1794700800;nonce=\"AAECAwQFBgcICQoLDA0ODw\";keyid=\"client-demo\";alg=\"ed25519\";tag=\"litemcp-auth-v1\""
        );
        assert_eq!(
            signed.signature(),
            "sig1=:kOnwfzENaxBF6ubQiJGlXkRfUYUJuELYzHnvqwrzpScvO3JchjFOsREa4j7CsRrdiEZifLmZZZEBBFPJqpsJDA==:"
        );
        assert!(signed.is_complete());
        // Le corps du vecteur fait bien 171 octets et son SHA-256 est celui
        // de la spécification.
        assert_eq!(BODY.len(), 171);
    }

    #[test]
    fn le_nonce_du_vecteur_est_l_encodage_base64url_attendu() {
        assert_eq!(nonce::encode_nonce(&NONCE), "AAECAwQFBgcICQoLDA0ODw");
        assert_eq!(nonce::encode_nonce(&nonce::system_nonce()).len(), 22);
    }

    #[test]
    fn une_requete_sans_mcp_name_couvre_six_composants() {
        let entetes = [
            ("Host", "127.0.0.1:23080"),
            ("Content-Type", "application/json"),
            ("Mcp-Method", "tools/list"),
        ];
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let req = RequestToSign::new("POST", "/mcp", "127.0.0.1:23080", &entetes, body);
        let signed = signeur().headers_at(&req, CREATED, &NONCE);
        assert!(
            signed
                .signature_input()
                .starts_with("sig1=(\"@method\" \"@path\" \"@authority\" \"content-digest\" \"content-type\" \"mcp-method\")")
        );
        assert!(!signed.signature_input().contains("mcp-name"));
        assert!(signed.is_complete());
    }

    #[test]
    fn une_requete_incomplete_est_signalee_et_non_verifiable() {
        let sans_couverture = [("Host", "127.0.0.1:23080")];
        let req = RequestToSign::new("POST", "/mcp", "127.0.0.1:23080", &sans_couverture, b"{}");
        let signed = signeur().headers_at(&req, CREATED, &NONCE);
        assert!(!signed.is_complete(), "Content-Type et Mcp-Method manquent");
        // Le `keyid` manquant rend aussi la requête incomplète.
        let complet = Signer::new(SigningKey::from_seed(&SEED));
        let entetes = entetes();
        let req = RequestToSign::new("POST", "/mcp", "127.0.0.1:23080", &entetes, BODY);
        assert!(!complet.headers_at(&req, CREATED, &NONCE).is_complete());
        // `Mcp-Name` dupliqué : incomplet aussi.
        let doubles = [
            ("Host", "127.0.0.1:23080"),
            ("Content-Type", "application/json"),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "db.query"),
            ("mcp-name", "autre"),
        ];
        let req = RequestToSign::new("POST", "/mcp", "127.0.0.1:23080", &doubles, BODY);
        assert!(!signeur().headers_at(&req, CREATED, &NONCE).is_complete());
    }

    #[test]
    fn l_autorite_est_normalisee() {
        assert_eq!(normalize_authority("Example.COM"), "example.com");
        assert_eq!(normalize_authority("example.com:443"), "example.com");
        assert_eq!(normalize_authority("example.com:80"), "example.com");
        assert_eq!(normalize_authority("127.0.0.1:23080"), "127.0.0.1:23080");
        assert_eq!(normalize_authority("[::1]:443"), "[::1]");
        assert_eq!(normalize_authority("[::1]:23080"), "[::1]:23080");
        assert_eq!(normalize_authority(" example.com "), "example.com");
    }

    #[test]
    fn le_chemin_perd_la_query_et_gagne_un_slash() {
        assert_eq!(normalize_path("/mcp"), "/mcp");
        assert_eq!(normalize_path("/mcp?x=1&y=2"), "/mcp");
        assert_eq!(normalize_path(""), "/");
        assert_eq!(normalize_path("?a=b"), "/");
        assert_eq!(normalize_path("/a%20b"), "/a%20b");
    }

    #[test]
    fn la_comparaison_est_celle_d_un_xor_accumule() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd"));
        assert!(ct_eq(b"", b""));
    }

    #[test]
    fn le_generateur_du_systeme_ne_repete_pas() {
        let a = system_bytes::<16>();
        let b = system_bytes::<16>();
        assert_ne!(a, b, "deux tirages identiques : source suspecte");
        assert_ne!(system_bytes::<32>(), [0u8; 32]);
    }

    #[test]
    fn l_en_tete_des_401_est_exactement_celui_de_la_specification() {
        assert_eq!(
            WWW_AUTHENTICATE,
            "Signature realm=\"litemcp\", tag=\"litemcp-auth-v1\""
        );
        assert_eq!(UNAUTHORIZED_STATUS, 401);
    }
}

/// Liaison `Mcp-Name` ↔ corps avec sentinelle `=?base64?…?=` (audit
/// 2026-11-25, B1) : le nom non-ASCII doit être décodé avant la comparaison,
/// une valeur non décodable reste un refus de liaison.
#[cfg(test)]
mod tests_liaison_mcp_name_2026_11_25 {
    use super::*;

    const CORPS: &str =
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"outil-café"}}"#;

    #[test]
    fn la_liaison_decode_la_sentinelle_base64_avant_comparaison() {
        // base64 standard de « outil-café » : `b3V0aWwtY2Fmw6k=`.
        assert!(
            check_mcp_binding(
                CORPS.as_bytes(),
                "tools/call",
                Some("=?base64?b3V0aWwtY2Fmw6k=?=")
            )
            .is_ok()
        );
    }

    #[test]
    fn la_liaison_refuse_une_sentinelle_ne_correspondant_pas() {
        // base64 de « echo » : la sentinelle décode vers un autre nom.
        let erreur = check_mcp_binding(CORPS.as_bytes(), "tools/call", Some("=?base64?ZWNoby==?="))
            .expect_err("la liaison doit échouer");
        assert_eq!(erreur.reason(), "covered_set_not_allowed");
    }

    #[test]
    fn la_liaison_refuse_une_sentinelle_indecodable() {
        for brute in [
            "=?base64?b3V0aWwtY2Fmw6?=", // longueur non multiple de 4
            "=?base64?ZW!oby==?=",       // caractère hors alphabet
            "=?base64?/w==?=",           // décode hors UTF-8
        ] {
            let erreur = check_mcp_binding(CORPS.as_bytes(), "tools/call", Some(brute))
                .expect_err("une valeur non décodable est un échec de liaison");
            assert_eq!(
                erreur.reason(),
                "covered_set_not_allowed",
                "accepté : {brute}"
            );
        }
    }

    #[test]
    fn la_liaison_compare_le_nom_brut_hors_sentinelle() {
        assert!(check_mcp_binding(CORPS.as_bytes(), "tools/call", Some("outil-café")).is_ok());
        assert!(check_mcp_binding(CORPS.as_bytes(), "tools/call", Some("echo")).is_err());
    }
}
