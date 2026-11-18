//! Clés Ed25519 du profil : [`SigningKey`] (privée), [`VerifyingKey`]
//! (publique) et [`Fingerprint`] (empreinte lisible, comparaison en temps
//! constant) — voir `docs/auth.md` §3.
//!
//! La clé privée **n'a ni `Debug` ni `Display`** : un journal ne peut pas la
//! recopier. Le type enveloppe [`ed25519_dalek::SigningKey`], dont le `Drop`
//! efface la graine (`zeroize`, activé dans `Cargo.toml`) ; il n'est pas
//! `Clone`, pour ne pas multiplier les copies d'un secret.
//!
//! # Formats d'échange
//!
//! Un seul bloc texte, une seule clé par fichier, fins de ligne LF ou CRLF :
//!
//! ```text
//! -----BEGIN LITEMCP ED25519 PRIVATE KEY-----
//! <base64 standard avec padding de la graine de 32 octets, sur une ligne>
//! -----END LITEMCP ED25519 PRIVATE KEY-----
//! ```
//!
//! Variante publique : `PUBLIC KEY` et les 32 octets de clé publique. Refusés :
//! autre libellé, bloc public présenté à l'import d'une clé privée (et
//! réciproquement), contenu décodé de longueur ≠ 32, alphabet base64url,
//! base64 non paddé, blanc à l'intérieur du base64, texte hors des gardes,
//! second bloc, fichier vide.

use ed25519_dalek::{
    Signature as DalekSignature, Signer as _, SigningKey as DalekSigningKey,
    VerifyingKey as DalekVerifyingKey,
};

use super::{base64, ct_eq, system_bytes};
use crate::auth::AuthError;

/// Libellé du bloc de clé privée.
const PRIVATE_LABEL: &str = "LITEMCP ED25519 PRIVATE KEY";
/// Libellé du bloc de clé publique.
const PUBLIC_LABEL: &str = "LITEMCP ED25519 PUBLIC KEY";
/// Longueur d'une graine ou d'une clé publique Ed25519, en octets.
const KEY_LEN: usize = 32;

/// Clé privée Ed25519 — **jamais** journalisée, effacée à la destruction.
///
/// Ni `Debug` ni `Display` : les deux implémentations sont volontairement
/// absentes, donc `format!("{key:?}")` ne compile pas. La graine est effacée
/// par le `Drop` de [`ed25519_dalek::SigningKey`] (feature `zeroize`).
///
/// ```
/// use litemcp::auth::sign::SigningKey;
///
/// let key = SigningKey::generate();
/// let export = key.export();
/// assert!(export.starts_with("-----BEGIN LITEMCP ED25519 PRIVATE KEY-----"));
/// let relue = SigningKey::import(&export).expect("bloc valide");
/// assert_eq!(relue.fingerprint(), key.fingerprint());
/// ```
pub struct SigningKey {
    inner: DalekSigningKey,
}

impl SigningKey {
    /// Génère une paire avec l'aléa du **système** (aucun générateur
    /// ensemencé, aucune horloge comme graine).
    #[must_use]
    pub fn generate() -> Self {
        Self {
            inner: DalekSigningKey::from_bytes(&system_bytes::<KEY_LEN>()),
        }
    }

    /// Reconstruit la clé depuis sa graine de 32 octets (vecteurs RFC 8032,
    /// import applicatif déjà validé).
    #[must_use]
    pub fn from_seed(seed: &[u8; KEY_LEN]) -> Self {
        Self {
            inner: DalekSigningKey::from_bytes(seed),
        }
    }

    /// Clé publique correspondante.
    #[must_use]
    pub fn verifying_key(&self) -> VerifyingKey {
        VerifyingKey {
            inner: self.inner.verifying_key(),
        }
    }

    /// Empreinte `ed25519:` + `SHA-256(clé publique)` (voir [`Fingerprint`]).
    #[must_use]
    pub fn fingerprint(&self) -> Fingerprint {
        self.verifying_key().fingerprint()
    }

    /// Bloc texte exportable (format de `docs/auth.md` §3, LF).
    ///
    /// L'encodage lit directement la graine dans la clé : aucune copie
    /// intermédiaire du secret n'est créée.
    #[must_use]
    pub fn export(&self) -> String {
        format!(
            "-----BEGIN {PRIVATE_LABEL}-----\n{}\n-----END {PRIVATE_LABEL}-----\n",
            base64::encode(self.inner.as_bytes())
        )
    }

    /// Relit un bloc privé (voir le format du module).
    ///
    /// # Erreurs
    ///
    /// [`AuthError`] avec un motif propre à l'import (`key_block_missing`,
    /// `key_block_label`, `key_base64_invalid`, `key_length_invalid`…). Ces
    /// motifs décrivent une **configuration locale**, pas un refus de requête :
    /// la liste fermée de `docs/auth.md` §2.7 ne s'applique qu'à
    /// [`SignatureVerifier`](super::SignatureVerifier).
    pub fn import(text: &str) -> Result<Self, AuthError> {
        let payload = extract_block(text, PRIVATE_LABEL)?;
        let bytes = base64::decode(&payload).map_err(|_| AuthError::new("key_base64_invalid"))?;
        let seed: [u8; KEY_LEN] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| AuthError::new("key_length_invalid"))?;
        Ok(Self::from_seed(&seed))
    }

    /// Signature détachée Ed25519 **pure** (RFC 8032 §5.1.6, sans préhash),
    /// 64 octets `R‖S`.
    ///
    /// **Hors profil HTTP** : réservé aux vecteurs publics et à
    /// l'interopérabilité. Le profil de `docs/auth.md` ne signe que la chaîne
    /// RFC 9421 — jamais un message applicatif.
    #[must_use]
    pub fn sign_message(&self, message: &[u8]) -> [u8; 64] {
        self.inner.sign(message).to_bytes()
    }

    /// Graine exportable — réservée aux tests de vecteurs, jamais journalisée.
    #[cfg(test)]
    pub(crate) fn seed(&self) -> [u8; KEY_LEN] {
        self.inner.to_bytes()
    }
}

/// Clé publique Ed25519 d'un client.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct VerifyingKey {
    inner: DalekVerifyingKey,
}

impl VerifyingKey {
    /// Depuis les 32 octets comprimés. Un point non canonique ou hors courbe
    /// est refusé (échec fermé).
    pub fn from_bytes(bytes: &[u8; KEY_LEN]) -> Result<Self, AuthError> {
        DalekVerifyingKey::from_bytes(bytes)
            .map(|inner| Self { inner })
            .map_err(|_| AuthError::new("key_point_invalid"))
    }

    /// Les 32 octets comprimés de la clé publique.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; KEY_LEN] {
        self.inner.to_bytes()
    }

    /// Empreinte `ed25519:` + `SHA-256(clé publique)`.
    #[must_use]
    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint::of_public_key(&self.inner.to_bytes())
    }

    /// Bloc public exportable (format de `docs/auth.md` §3, LF).
    #[must_use]
    pub fn export(&self) -> String {
        format!(
            "-----BEGIN {PUBLIC_LABEL}-----\n{}\n-----END {PUBLIC_LABEL}-----\n",
            base64::encode(&self.inner.to_bytes())
        )
    }

    /// Relit un bloc public ; mêmes refus que [`SigningKey::import`].
    pub fn import(text: &str) -> Result<Self, AuthError> {
        let payload = extract_block(text, PUBLIC_LABEL)?;
        let bytes = base64::decode(&payload).map_err(|_| AuthError::new("key_base64_invalid"))?;
        let bytes: [u8; KEY_LEN] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| AuthError::new("key_length_invalid"))?;
        Self::from_bytes(&bytes)
    }

    /// Vérification **stricte** (`verify_strict` : rejette les clés faibles et
    /// la malléabilité de `S`), en temps constant.
    ///
    /// **Hors profil HTTP** : voir [`SigningKey::sign_message`].
    #[must_use]
    pub fn verify_message(&self, message: &[u8], signature: &[u8; 64]) -> bool {
        let signature = DalekSignature::from_bytes(signature);
        self.inner.verify_strict(message, &signature).is_ok()
    }
}

impl std::fmt::Debug for VerifyingKey {
    /// Empreinte complète : deux clés différentes ne s'affichent jamais de la
    /// même façon, même tronquées.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "VerifyingKey({})", self.fingerprint().full())
    }
}

/// Empreinte d'une clé publique : `SHA-256(clé publique, 32 octets)`.
///
/// `PartialEq` **délègue** à [`Fingerprint::ct_eq`] (comparaison en temps
/// constant, sans sortie anticipée) ; `Debug` rend la forme complète
/// `ed25519:<64 hex minuscules>`, donc deux empreintes distinctes ne peuvent
/// pas s'afficher à l'identique. La forme courte (`short`) est réservée à
/// l'affichage et à l'appairage : elle n'est pas une preuve d'identité.
#[derive(Clone, Copy)]
pub struct Fingerprint([u8; KEY_LEN]);

impl Fingerprint {
    /// Empreinte des 32 octets d'une clé publique.
    #[must_use]
    fn of_public_key(public_key: &[u8; KEY_LEN]) -> Self {
        Self(crate::request_state::sha256(public_key))
    }

    /// Les 32 octets de l'empreinte (SHA-256).
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }

    /// Forme complète : `ed25519:` + 64 caractères hexadécimaux minuscules.
    #[must_use]
    pub fn full(&self) -> String {
        format!("ed25519:{}", hex(&self.0))
    }

    /// Forme courte d'affichage : `ed25519:` + les 8 premiers octets en hex.
    #[must_use]
    pub fn short(&self) -> String {
        format!("ed25519:{}", hex(&self.0[..8]))
    }

    /// Comparaison en temps constant (longueurs égales par construction).
    #[must_use]
    pub fn ct_eq(&self, other: &Self) -> bool {
        ct_eq(&self.0, &other.0)
    }
}

impl PartialEq for Fingerprint {
    fn eq(&self, other: &Self) -> bool {
        self.ct_eq(other)
    }
}

impl Eq for Fingerprint {}

impl std::fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Fingerprint({})", self.full())
    }
}

/// Hexadécimal minuscule — l'unique forme admise par `docs/auth.md` §3.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Extrait l'unique bloc `label` d'un fichier de clé, ou explique pourquoi il
/// n'y en a pas exactement un.
fn extract_block(text: &str, label: &str) -> Result<String, AuthError> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    // LF ou CRLF admis ; un CR isolé est une structure étrangère.
    let normalized = text.replace("\r\n", "\n");
    if normalized.contains('\r') {
        return Err(AuthError::new("key_block_label"));
    }

    #[derive(PartialEq)]
    enum State {
        Outside,
        Inside,
        Done,
    }

    let mut state = State::Outside;
    let mut payload: Option<&str> = None;
    for line in normalized.lines() {
        match state {
            State::Outside => {
                if line.trim().is_empty() {
                    continue;
                }
                if line == begin {
                    state = State::Inside;
                    continue;
                }
                // Tout autre bloc (autre libellé, sens inversé) est un refus
                // explicite : c'est le cas « bloc public à l'import privé ».
                if line.starts_with("-----BEGIN ") || line.starts_with("-----END ") {
                    return Err(AuthError::new("key_block_label"));
                }
                return Err(AuthError::new("key_text_outside_block"));
            }
            State::Inside => {
                if line == end {
                    state = State::Done;
                    continue;
                }
                if payload.is_some() {
                    return Err(AuthError::new("key_block_lines"));
                }
                if line.is_empty() {
                    return Err(AuthError::new("key_block_empty"));
                }
                payload = Some(line);
            }
            State::Done => {
                if line.trim().is_empty() {
                    continue;
                }
                // Un second bloc (ou du texte) après la garde de fin.
                return Err(AuthError::new("key_text_outside_block"));
            }
        }
    }

    if state != State::Done {
        return Err(AuthError::new("key_block_unterminated"));
    }
    payload
        .map(str::to_owned)
        .ok_or_else(|| AuthError::new("key_block_empty"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Import privé qui doit échouer — `SigningKey` n'a pas de `Debug`, donc
    /// `expect_err` est indisponible (et c'est voulu).
    fn refus(text: &str) -> AuthError {
        match SigningKey::import(text) {
            Ok(_) => panic!("bloc accepté à tort"),
            Err(error) => error,
        }
    }

    /// RFC 8032 §7.1, vecteur 1 (clé publique seule : la graine est publique
    /// elle aussi, c'est un vecteur de test).
    const RFC8032_SEED_1: [u8; 32] = [
        0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c,
        0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae,
        0x7f, 0x60,
    ];
    const RFC8032_PUBLIC_1: [u8; 32] = [
        0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64, 0x07,
        0x3a, 0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68, 0xf7, 0x07,
        0x51, 0x1a,
    ];

    #[test]
    fn une_graine_donne_la_cle_publique_de_la_rfc8032() {
        let key = SigningKey::from_seed(&RFC8032_SEED_1);
        assert_eq!(key.seed(), RFC8032_SEED_1);
        assert_eq!(key.verifying_key().to_bytes(), RFC8032_PUBLIC_1);
    }

    #[test]
    fn la_cle_privee_ne_s_affiche_pas() {
        // Compilation : ni `Debug` ni `Display`. Le test vérifie seulement que
        // l'empreinte, elle, est disponible et complète.
        let key = SigningKey::from_seed(&RFC8032_SEED_1);
        let empreinte = key.fingerprint().full();
        assert_eq!(empreinte.len(), "ed25519:".len() + 64);
        assert!(empreinte.starts_with("ed25519:"));
        assert_eq!(key.fingerprint().short().len(), "ed25519:".len() + 16);
        // Debug de la clé publique : empreinte complète, non ambiguë.
        assert_eq!(
            format!("{:?}", key.verifying_key()),
            format!("VerifyingKey({empreinte})")
        );
    }

    #[test]
    fn l_empreinte_est_sha256_de_la_cle_publique() {
        let key = SigningKey::from_seed(&RFC8032_SEED_1);
        let attendu = crate::request_state::sha256(&RFC8032_PUBLIC_1);
        assert_eq!(key.fingerprint().as_bytes(), &attendu);
        // Forme hexadécimale minuscule, 64 caractères.
        let hex = key.fingerprint().full();
        assert!(
            hex["ed25519:".len()..]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "{hex}"
        );
        assert_eq!(key.fingerprint(), key.verifying_key().fingerprint());
    }

    #[test]
    fn l_import_export_privé_fait_un_aller_retour() {
        let key = SigningKey::from_seed(&RFC8032_SEED_1);
        let export = key.export();
        assert!(export.contains("-----BEGIN LITEMCP ED25519 PRIVATE KEY-----"));
        assert!(export.ends_with("-----\n"));
        let relue = SigningKey::import(&export).expect("bloc valide");
        assert_eq!(relue.seed(), RFC8032_SEED_1);
        assert_eq!(relue.fingerprint(), key.fingerprint());
        // CRLF admis, une seule clé par fichier.
        let crlf = export.replace('\n', "\r\n");
        assert_eq!(
            SigningKey::import(&crlf).expect("CRLF admis").fingerprint(),
            key.fingerprint()
        );
        // Lignes vides autour du bloc admises.
        let entoure = format!("\n\n{export}\n\n");
        assert!(SigningKey::import(&entoure).is_ok());
    }

    #[test]
    fn l_import_export_public_fait_un_aller_retour() {
        let key = SigningKey::from_seed(&RFC8032_SEED_1).verifying_key();
        let export = key.export();
        assert!(export.starts_with("-----BEGIN LITEMCP ED25519 PUBLIC KEY-----"));
        assert_eq!(VerifyingKey::import(&export).expect("bloc"), key);
        // RFC 9421 §B.1.4 : la clé `test-key-ed25519` en base64 (JWK `x`).
        let attendu = base64::encode(&RFC8032_PUBLIC_1);
        assert!(export.contains(&attendu), "{export}");
    }

    #[test]
    fn un_bloc_du_mauvais_type_est_refuse() {
        let key = SigningKey::from_seed(&RFC8032_SEED_1);
        let public = key.verifying_key().export();
        assert_eq!(
            SigningKey::import(&public)
                .map(|_| ())
                .unwrap_err()
                .reason(),
            "key_block_label"
        );
        let prive = key.export();
        assert_eq!(
            VerifyingKey::import(&prive)
                .expect_err("privé ≠ public")
                .reason(),
            "key_block_label"
        );
    }

    #[test]
    fn les_formes_de_bloc_invalides_sont_refusees() {
        let key = SigningKey::from_seed(&RFC8032_SEED_1);
        let payload = base64::encode(&RFC8032_SEED_1);
        let cas: [(&str, &str); 10] = [
            ("", "key_block_unterminated"),
            ("pas un bloc\n", "key_text_outside_block"),
            (
                "-----BEGIN LITEMCP ED25519 PRIVATE KEY-----\n",
                "key_block_unterminated",
            ),
            (
                "-----BEGIN LITEMCP ED25519 PRIVATE KEY-----\n-----END LITEMCP ED25519 PRIVATE KEY-----\n",
                "key_block_empty",
            ),
            // Base64 non paddé.
            (
                "-----BEGIN LITEMCP ED25519 PRIVATE KEY-----\nAAECAwQFBgcICQoLDA0ODw\n-----END LITEMCP ED25519 PRIVATE KEY-----\n",
                "key_base64_invalid",
            ),
            // Base64url : `-` hors de l'alphabet standard.
            (
                "-----BEGIN LITEMCP ED25519 PRIVATE KEY-----\nAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA-=\n-----END LITEMCP ED25519 PRIVATE KEY-----\n",
                "key_base64_invalid",
            ),
            // Blanc à l'intérieur du base64.
            (
                "-----BEGIN LITEMCP ED25519 PRIVATE KEY-----\nAAECAwQF BgcICQoLDA0ODw==\n-----END LITEMCP ED25519 PRIVATE KEY-----\n",
                "key_base64_invalid",
            ),
            // 16 octets au lieu de 32.
            (
                "-----BEGIN LITEMCP ED25519 PRIVATE KEY-----\nAAECAwQFBgcICQoLDA0ODw==\n-----END LITEMCP ED25519 PRIVATE KEY-----\n",
                "key_length_invalid",
            ),
            // Deux lignes de charge.
            (
                "-----BEGIN LITEMCP ED25519 PRIVATE KEY-----\nAAECAwQFBgcICQoLDA0ODw==\nAAECAwQFBgcICQoLDA0ODw==\n-----END LITEMCP ED25519 PRIVATE KEY-----\n",
                "key_block_lines",
            ),
            // Texte après la garde de fin.
            (
                "-----BEGIN LITEMCP ED25519 PRIVATE KEY-----\nAAECAwQFBgcICQoLDA0ODw==\n-----END LITEMCP ED25519 PRIVATE KEY-----\ntrafic\n",
                "key_text_outside_block",
            ),
        ];
        for (texte, motif) in cas {
            let erreur = refus(texte);
            assert_eq!(erreur.reason(), motif, "{texte:?}");
        }
        assert_eq!(payload.len(), 44);
        // Second bloc identique : refusé aussi.
        let deux = format!("{}{}", key.export(), key.export());
        assert_eq!(refus(&deux).reason(), "key_text_outside_block");
    }

    #[test]
    fn une_cle_publique_hors_courbe_est_refusee() {
        // Un encodage de point n'est valide que si `y` donne un `x` au carré :
        // la moitié environ des valeurs sont rejetées par la décompression.
        let mut rejetes = 0usize;
        for y in 2u8..64 {
            let mut bytes = [0u8; 32];
            bytes[0] = y;
            if let Err(erreur) = VerifyingKey::from_bytes(&bytes) {
                assert_eq!(erreur.reason(), "key_point_invalid");
                rejetes += 1;
            }
        }
        assert!(rejetes > 0, "aucune valeur testée n'est hors courbe");
    }

    #[test]
    fn la_comparaison_d_empreintes_passe_par_ct_eq() {
        let a = SigningKey::from_seed(&RFC8032_SEED_1).fingerprint();
        let b = a;
        let autre = SigningKey::from_seed(&[7u8; 32]).fingerprint();
        assert!(a.ct_eq(&b));
        assert_eq!(a, b);
        assert!(!a.ct_eq(&autre));
        assert_ne!(a, autre);
        // Une différence d'un seul bit est détectée.
        let mut octets = *a.as_bytes();
        octets[31] ^= 0x01;
        assert_ne!(octets, *b.as_bytes());
    }
}
