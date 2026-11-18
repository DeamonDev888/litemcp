//! TLS 1.3 **serveur** — feature `tls` (contrat C10).
//!
//! Ce module ne fait qu'une chose : transformer un certificat et sa clé en une
//! configuration `rustls` **TLS 1.3 seulement**, et exposer l'empreinte du
//! certificat pour l'épinglage côté client
//! ([`crate::auth::PinnedServer`]). Aucune option « ne pas vérifier » n'existe
//! ici : un client qui refuse un certificat doit refuser la connexion.
//!
//! # Ce qui est configuré, et rien d'autre
//!
//! * **Version** : TLS 1.3 uniquement (`rustls` est compilé sans la feature
//!   `tls12`) ; les trois suites TLS 1.3 de `ring` s'appliquent, non
//!   configurables par l'appelant.
//! * **ALPN** : `http/1.1` seulement.
//! * **Client** : pas d'authentification mutuelle (mTLS hors périmètre,
//!   `docs/auth.md` §9).
//! * **Certificat** : fourni par l'application (`TlsConfig::from_pem`,
//!   `TlsConfig::from_der`) ou auto-signé **en mémoire** sur demande
//!   explicite (`self_signed_ephemeral`, feature `tls` + `rcgen`).
//!   La lib n'écrit **jamais** de clé ni de certificat sur disque.
//!
//! ```no_run
//! use litemcp::prelude::*;
//!
//! # fn main() -> std::io::Result<()> {
//! let tls = litemcp::tls::TlsConfig::self_signed_ephemeral("localhost")
//!     .expect("certificat éphémère");
//! let bound = ServerBuilder::new("calc", "1.0.0")
//!     .tool("add", "Additionne", |_args| Ok(serde_json::json!({})))
//!     .tls(tls)
//!     .bind("127.0.0.1:23080")?;
//! // Empreinte à épingler côté client (`sha256:` + SHA-256 du DER).
//! let empreinte = bound.certificate_fingerprint().expect("TLS configuré");
//! eprintln!("certificat : {empreinte}");
//! bound.serve()?;
//! # Ok(())
//! # }
//! ```

use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// Erreur de configuration TLS — message **fixe** et court, sans détail
/// interne : une configuration refusée ne doit pas recopier de matériel de
/// clé (même partiel) dans un journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct TlsError {
    reason: &'static str,
}

impl TlsError {
    const fn new(reason: &'static str) -> Self {
        Self { reason }
    }

    /// Motif stable (`pem_invalid`, `certificate_missing`…), pour un test ou
    /// un journal applicatif.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        self.reason
    }
}

impl std::fmt::Display for TlsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid TLS configuration: {}", self.reason)
    }
}

impl std::error::Error for TlsError {}

/// Empreinte d'un certificat : `sha256:` + SHA-256 du DER, hexadécimal
/// **minuscule** (64 caractères) — forme de `docs/auth.md` §8.
///
/// La comparaison est en **temps constant** : deux empreintes ne se
/// distinguent pas par le temps mis à les comparer.
#[derive(Clone, Copy)]
pub struct CertificateFingerprint([u8; 32]);

impl CertificateFingerprint {
    /// Calcule l'empreinte d'un certificat DER.
    ///
    /// SHA-256 vient de l'implémentation interne du crate (celle du
    /// `Content-Digest` du profil de signature) : aucune primitive n'est
    /// ajoutée ici.
    #[must_use]
    pub fn of_der(der: &[u8]) -> Self {
        Self(crate::request_state::sha256(der))
    }

    /// Octets bruts de l'empreinte (32 octets).
    #[must_use]
    pub fn to_bytes(self) -> [u8; 32] {
        self.0
    }

    /// Forme canonique `sha256:<64 hex minuscules>`.
    #[must_use]
    pub fn to_hex(self) -> String {
        let mut texte = String::with_capacity(71);
        texte.push_str("sha256:");
        for byte in self.0 {
            texte.push(hex_digit(byte >> 4));
            texte.push(hex_digit(byte & 0x0f));
        }
        texte
    }
}

impl PartialEq for CertificateFingerprint {
    fn eq(&self, other: &Self) -> bool {
        let mut difference = 0u8;
        for (left, right) in self.0.iter().zip(other.0.iter()) {
            difference |= left ^ right;
        }
        difference == 0
    }
}

impl Eq for CertificateFingerprint {}

impl std::fmt::Display for CertificateFingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl std::fmt::Debug for CertificateFingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CertificateFingerprint({})", self.to_hex())
    }
}

fn hex_digit(value: u8) -> char {
    match value & 0x0f {
        0..=9 => char::from(b'0' + (value & 0x0f)),
        other => char::from(b'a' + other - 10),
    }
}

/// Certificat + clé privée d'un serveur TLS — construit en mémoire, jamais
/// écrit sur disque par la lib.
pub struct TlsConfig {
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    fingerprint: CertificateFingerprint,
}

impl Clone for TlsConfig {
    fn clone(&self) -> Self {
        Self {
            chain: self.chain.clone(),
            // `clone_key` est le seul clone prévu par `rustls-pki-types`
            // (une clé privée ne se duplique pas par `Clone`).
            key: self.key.clone_key(),
            fingerprint: self.fingerprint,
        }
    }
}

impl TlsConfig {
    /// Depuis un **PEM** : chaîne de certificats (feuille d'abord, une ou
    /// plusieurs `CERTIFICATE`) et clé privée (`PRIVATE KEY` PKCS#8,
    /// `RSA PRIVATE KEY` ou `EC PRIVATE KEY`).
    ///
    /// # Erreurs
    ///
    /// [`TlsError`] (`pem_invalid`, `certificate_missing`, `key_missing`,
    /// `key_ambiguous`) — un PEM qui ne se lit pas n'est pas une
    /// configuration par défaut.
    pub fn from_pem(certificates_pem: &[u8], private_key_pem: &[u8]) -> Result<Self, TlsError> {
        use rustls::pki_types::pem::PemObject;

        let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(certificates_pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| TlsError::new("pem_invalid"))?;
        if chain.is_empty() {
            return Err(TlsError::new("certificate_missing"));
        }
        let key = PrivateKeyDer::from_pem_slice(private_key_pem)
            .map_err(|_| TlsError::new("key_missing"))?;
        Self::assemble(chain, key)
    }

    /// Depuis un **DER** : un seul certificat et une clé privée PKCS#8.
    ///
    /// # Erreurs
    ///
    /// [`TlsError`] (`certificate_empty`, `key_empty`) pour une entrée vide.
    pub fn from_der(certificate_der: Vec<u8>, private_key_der: Vec<u8>) -> Result<Self, TlsError> {
        if certificate_der.is_empty() {
            return Err(TlsError::new("certificate_empty"));
        }
        if private_key_der.is_empty() {
            return Err(TlsError::new("key_empty"));
        }
        let chain = vec![CertificateDer::from(certificate_der)];
        let key = PrivateKeyDer::Pkcs8(private_key_der.into());
        Self::assemble(chain, key)
    }

    fn assemble(
        chain: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
    ) -> Result<Self, TlsError> {
        let leaf = chain.first().ok_or(TlsError::new("certificate_missing"))?;
        let fingerprint = CertificateFingerprint::of_der(leaf.as_ref());
        Ok(Self {
            chain,
            key,
            fingerprint,
        })
    }

    /// Empreinte du certificat **feuille** (celui que le client voit et
    /// épingle).
    #[must_use]
    pub fn fingerprint(&self) -> CertificateFingerprint {
        self.fingerprint
    }

    /// DER du certificat **feuille** — la forme à distribuer à un client qui
    /// épingle ou vérifie l'autorité (l'empreinte est calculée dessus).
    #[must_use]
    pub fn certificate_der(&self) -> &[u8] {
        self.chain.first().map_or(&[], |cert| cert.as_ref())
    }

    /// Configuration `rustls` du serveur : TLS 1.3 seulement, ALPN
    /// `http/1.1`, aucun client à authentifier.
    pub(crate) fn server_config(&self) -> Result<rustls::ServerConfig, TlsError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|_| TlsError::new("tls_version"))?
            .with_no_client_auth()
            .with_single_cert(self.chain.clone(), self.key.clone_key())
            .map_err(|_| TlsError::new("certificate_invalid"))?;
        // ALPN : HTTP/1.1 seulement (pas de h2 dans ce crate).
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        // Sessions stateless : le serveur MCP l'est aussi.
        config.session_storage = rustls::server::ServerSessionMemoryCache::new(256);
        Ok(config)
    }
}

impl std::fmt::Debug for TlsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsConfig")
            .field("certificates", &self.chain.len())
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "tls")]
impl TlsConfig {
    /// Certificat **auto-signé éphémère**, généré en mémoire pour le nom
    /// fourni — aucune écriture disque, aucun secret committé.
    ///
    /// Utilisé par les exemples et les tests ; une application qui veut un
    /// épinglage stable fournit son propre certificat
    /// ([`TlsConfig::from_pem`]) et persiste la clé elle-même.
    ///
    /// # Erreurs
    ///
    /// [`TlsError`] (`hostname_invalid`, `rcgen_failed`) si le nom est vide
    /// ou si la génération échoue.
    pub fn self_signed_ephemeral(hostname: &str) -> Result<Self, TlsError> {
        if hostname.is_empty() {
            return Err(TlsError::new("hostname_invalid"));
        }
        let key = rcgen::KeyPair::generate().map_err(|_| TlsError::new("rcgen_failed"))?;
        let mut params = rcgen::CertificateParams::new(vec![hostname.to_string()])
            .map_err(|_| TlsError::new("hostname_invalid"))?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, hostname);
        let certificate = params
            .self_signed(&key)
            .map_err(|_| TlsError::new("rcgen_failed"))?;
        let chain = vec![CertificateDer::from(certificate.der().to_vec())];
        let key = PrivateKeyDer::Pkcs8(key.serialize_der().into());
        Self::assemble(chain, key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l_empreinte_est_celle_du_sha256_du_der() {
        // SHA-256("abc") = ba7816bf… (vecteur FIPS 180-4) : l'empreinte ne
        // dépend que du DER fourni, jamais d'un état interne.
        let empreinte = CertificateFingerprint::of_der(b"abc");
        assert_eq!(
            empreinte.to_hex(),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(empreinte.to_bytes().len(), 32);
        assert_eq!(empreinte, CertificateFingerprint::of_der(b"abc"));
        assert_ne!(empreinte, CertificateFingerprint::of_der(b"abd"));
    }

    #[test]
    fn une_configuration_vide_est_refusee() {
        assert_eq!(
            TlsConfig::from_der(Vec::new(), vec![1])
                .unwrap_err()
                .reason(),
            "certificate_empty"
        );
        assert_eq!(
            TlsConfig::from_der(vec![1], Vec::new())
                .unwrap_err()
                .reason(),
            "key_empty"
        );
        let erreur = TlsConfig::from_pem(b"pas un pem", b"pas un pem").unwrap_err();
        assert!(
            matches!(erreur.reason(), "pem_invalid" | "certificate_missing"),
            "motif : {}",
            erreur.reason()
        );
    }

    #[cfg(feature = "tls")]
    #[test]
    fn un_certificat_ephemere_regle_tls13_et_alpn_http11() {
        let config = TlsConfig::self_signed_ephemeral("localhost").expect("éphémère");
        let server = config.server_config().expect("configuration serveur");
        assert_eq!(server.alpn_protocols, vec![b"http/1.1".to_vec()]);
        // Le certificat est bien celui dont l'empreinte est publiée.
        assert_eq!(
            CertificateFingerprint::of_der(config.chain[0].as_ref()),
            config.fingerprint()
        );
    }
}
