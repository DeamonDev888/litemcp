//! `requestState` MRTR — jeton **opaque signé**, sans état serveur.
//!
//! La spec MCP `2026-07-28` (multi round-trip requests) exige qu'un
//! `requestState` renvoyé par le client soit traité comme une entrée
//! **attaquable** : si le jeton influence une autorisation, un accès ou la
//! logique métier, le serveur **doit** en protéger l'intégrité (HMAC ou AEAD)
//! et **doit** refuser un jeton qui échoue à la vérification.
//!
//! Le plus petit modèle compatible **stateless** : le jeton porte lui-même
//! tout ce qu'il faut pour reprendre (méthode, digest des paramètres
//! saillants, id d'origine, expiration, tour, charge utile du handler) et il
//! est authentifié par HMAC-SHA256 avec une clé du serveur —
//! `base64url(payload_json) "." base64url(hmac_sha256(payload))`.
//!
//! Aucune dépendance : SHA-256 et HMAC-SHA256 sont implémentés ici (ils sont
//! couverts par des vecteurs de la RFC 6234 / RFC 4231, voir les tests).
//!
//! ## Durée de vie
//!
//! Un jeton est valide **au plus** `request_state_ttl` (défaut 10 min) à
//! partir de sa frappe. Il est **porteur** (bearer) : quiconque le détient
//! peut le présenter ; la liaison méthode/paramètres/id limite sa réutilisation
//! à la requête d'origine, le TTL borne la fenêtre de rejeu — mais **la
//! consommation unique n'est pas garantie** par le jeton (la spec le dit :
//! un serveur qui exige « au plus une fois » doit l'imposer de son côté).
//! Le jeton est **signé, pas chiffré** : le client peut le décoder — n'y
//! mettez aucun secret. La clé est celle du process (ou celle fournie par
//! l'opérateur, partagée par la flotte) : un redémarrage sans clé partagée
//! invalide tous les jetons en vol.

use serde_json::Value;

/// Longueur maximale (octets) du jeton `requestState` accepté en entrée —
/// au-delà, refus immédiat sans décodage (le jeton est borné par
/// construction : payload ≤ [`MAX_STATE_PAYLOAD_BYTES`] + enveloppe).
pub(crate) const MAX_REQUEST_STATE_CHARS: usize = 16_384;

/// Taille maximale (octets JSON) de la charge utile opaque fournie par un
/// handler (`InputRequiredSpec::with_state`) — un handler qui la dépasse est
/// un bogue serveur (refusé en `-32603` à l'émission).
pub(crate) const MAX_STATE_PAYLOAD_BYTES: usize = 8_192;

/// Durée de vie par défaut d'un `requestState` : 10 minutes — « a short
/// expiry (TTL) » (spec MRTR, prévention du rejeu).
pub(crate) const DEFAULT_REQUEST_STATE_TTL: std::time::Duration =
    std::time::Duration::from_secs(600);

/// Échec de lecture d'un `requestState` — tous rendus au client en
/// `-32602` avec un message distinct (le détail aide au diagnostic, jamais
/// la clé ni le contenu du jeton).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StateError {
    /// Structure invalide : pas de `.`, base64url invalide, payload non-JSON.
    Malformed,
    /// La signature HMAC ne correspond pas (jeton altéré, clé différente).
    BadSignature,
    /// Payload JSON illisible après vérification (ne devrait pas arriver :
    /// la signature couvre les octets, il s'agit d'un jeton forgé avec la
    /// bonne clé mais un payload non-JSON).
    BadPayload,
}

impl StateError {
    /// Message lisible (français) pour l'erreur `-32602`.
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Malformed => "malformed requestState (expected base64url payload.signature)",
            Self::BadSignature => "invalid requestState signature",
            Self::BadPayload => "malformed requestState payload",
        }
    }
}

/// Frappe d'un jeton : `base64url(payload) "." base64url(hmac(payload))`.
///
/// Infaillible par construction : un [`Value`] se sérialise toujours.
pub(crate) fn mint_token(key: &[u8], payload: &Value) -> String {
    let bytes = serde_json::to_vec(payload).expect("Value serialization is infallible");
    let signature = hmac_sha256(key, &bytes);
    format!("{}.{}", b64url_encode(&bytes), b64url_encode(&signature))
}

/// Vérification d'un jeton : structure → signature (comparaison à temps
/// constant) → payload JSON. Tout échec est un refus net.
pub(crate) fn verify_token(key: &[u8], token: &str) -> Result<Value, StateError> {
    let Some((payload_part, signature_part)) = token.split_once('.') else {
        return Err(StateError::Malformed);
    };
    if payload_part.is_empty() || signature_part.is_empty() || signature_part.contains('.') {
        return Err(StateError::Malformed);
    }
    let payload_bytes = b64url_decode(payload_part)?;
    let signature = b64url_decode(signature_part)?;
    let expected = hmac_sha256(key, &payload_bytes);
    // Comparaison à temps constant : `|` n'accumule pas de branche dépendant
    // du contenu (même longueur vérifiée d'abord).
    let mut diff = (signature.len() ^ expected.len()) as u8;
    for (a, b) in signature.iter().zip(expected.iter()) {
        diff |= a ^ b;
    }
    if diff != 0 {
        return Err(StateError::BadSignature);
    }
    serde_json::from_slice(&payload_bytes).map_err(|_| StateError::BadPayload)
}

/// Digest d'un identifiant de requête (méthode + paramètres saillants) —
/// SHA-256 tronqué à 128 bits, hexadécimal. Lie le `requestState` à la
/// requête d'origine (spec MRTR, exigence 5).
pub(crate) fn digest_salient(value: &Value) -> String {
    let bytes = serde_json::to_vec(value).expect("Value serialization is infallible");
    let digest = sha256(&bytes);
    let mut hex = String::with_capacity(32);
    for byte in &digest[..16] {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Clé HMAC par défaut d'un process — récoltée via [`std::collections::hash_map::RandomState`]
/// (dont les graines viennent de l'OS) mélangée au PID et à l'horloge.
///
/// Ce n'est **pas** une clé partageable : elle ne survit ni au redémarrage ni
/// à une seconde instance. Un déploiement en flotte (gateways, répliques,
/// redémarrages) doit fournir `ServerBuilder::request_state_key`.
pub(crate) fn harvest_key() -> Vec<u8> {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    let mut key = Vec::with_capacity(32);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    for round in 0..4u64 {
        // Chaque `RandomState::new()` tire des graines distinctes ; les
        // quatre hachages sont indépendants et couvrent 256 bits.
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(now);
        hasher.write_u32(std::process::id());
        hasher.write_u64(round);
        hasher.write_usize(&key as *const _ as usize);
        key.extend_from_slice(&hasher.finish().to_le_bytes());
    }
    key
}

// ===========================================================================
// SHA-256 (FIPS 180-4) — implémentation locale, sans dépendance.
// ===========================================================================

/// Constantes de tour SHA-256 (racines cubiques des 64 premiers nombres
/// premiers, partie fractionnaire × 2³²).
const SHA256_K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// SHA-256 d'un tampon d'octets.
pub(crate) fn sha256(data: &[u8]) -> [u8; 32] {
    let mut state: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    // Rem Bourrage : 0x80, des zéros jusqu'à 56 mod 64, longueur en bits BE.
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut message = Vec::with_capacity(data.len() + 72);
    message.extend_from_slice(data);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    let mut schedule = [0u32; 64];
    for block in message.chunks_exact(64) {
        for (index, word) in block.chunks_exact(4).enumerate() {
            schedule[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..64 {
            let s0 = schedule[index - 15].rotate_right(7)
                ^ schedule[index - 15].rotate_right(18)
                ^ (schedule[index - 15] >> 3);
            let s1 = schedule[index - 2].rotate_right(17)
                ^ schedule[index - 2].rotate_right(19)
                ^ (schedule[index - 2] >> 10);
            schedule[index] = schedule[index - 16]
                .wrapping_add(s0)
                .wrapping_add(schedule[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(choice)
                .wrapping_add(SHA256_K[index])
                .wrapping_add(schedule[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }

    let mut out = [0u8; 32];
    for (index, word) in state.iter().enumerate() {
        out[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// HMAC-SHA256 (RFC 2104) — clé au-delà d'un bloc hachée d'abord.
pub(crate) fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut padded = [0u8; BLOCK];
    if key.len() > BLOCK {
        padded[..32].copy_from_slice(&sha256(key));
    } else {
        padded[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36u8; BLOCK];
    let mut outer_pad = [0x5cu8; BLOCK];
    for index in 0..BLOCK {
        inner_pad[index] ^= padded[index];
        outer_pad[index] ^= padded[index];
    }
    let mut inner = Vec::with_capacity(BLOCK + data.len());
    inner.extend_from_slice(&inner_pad);
    inner.extend_from_slice(data);
    let inner_hash = sha256(&inner);
    let mut outer = Vec::with_capacity(BLOCK + 32);
    outer.extend_from_slice(&outer_pad);
    outer.extend_from_slice(&inner_hash);
    sha256(&outer)
}

// ===========================================================================
// base64url (RFC 4648 §5) — sans padding, alphabet URL.
// ===========================================================================

const B64URL_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn b64url_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks_exact(3) {
        let triple = (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]);
        out.push(B64URL_ALPHABET[(triple >> 18) as usize & 63] as char);
        out.push(B64URL_ALPHABET[(triple >> 12) as usize & 63] as char);
        out.push(B64URL_ALPHABET[(triple >> 6) as usize & 63] as char);
        out.push(B64URL_ALPHABET[triple as usize & 63] as char);
    }
    match data.len() % 3 {
        1 => {
            let byte = u32::from(data[data.len() - 1]);
            out.push(B64URL_ALPHABET[(byte >> 2) as usize] as char);
            out.push(B64URL_ALPHABET[((byte & 0x03) << 4) as usize] as char);
        }
        2 => {
            let pair = (u32::from(data[data.len() - 2]) << 8) | u32::from(data[data.len() - 1]);
            out.push(B64URL_ALPHABET[(pair >> 10) as usize & 63] as char);
            out.push(B64URL_ALPHABET[(pair >> 4) as usize & 63] as char);
            out.push(B64URL_ALPHABET[((pair & 0x0f) << 2) as usize] as char);
        }
        _ => {}
    }
    out
}

fn b64url_decode(input: &str) -> Result<Vec<u8>, StateError> {
    let mut accumulator: u32 = 0;
    let mut bits: u32 = 0;
    let mut out = Vec::with_capacity(input.len() * 3 / 4 + 3);
    for byte in input.bytes() {
        // Les jetons émis n'ont pas de padding ; un `=` est une structure
        // étrangère — refus (deux encodages pour un même jeton = ambiguïté).
        let value = match byte {
            b'A'..=b'Z' => u32::from(byte - b'A'),
            b'a'..=b'z' => u32::from(byte - b'a') + 26,
            b'0'..=b'9' => u32::from(byte - b'0') + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return Err(StateError::Malformed),
        };
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((accumulator >> bits) & 0xff) as u8);
        }
    }
    // Bits résiduels : un encodage non canonique (longueur % 4 == 1) est
    // refusé — sinon plusieurs chaînes produiraient les mêmes octets.
    if bits >= 6 {
        return Err(StateError::Malformed);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

    #[test]
    fn sha256_matches_fips_vectors() {
        // FIPS 180-4 / RFC 6234, vecteurs publics.
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(b"The quick brown fox jumps over the lazy dog")),
            "d7a8fbb307d7809469ca9abcb0082e4f8d5651e46d3cdb762d02d0bf37c9e592"
        );
        // Plusieurs blocs (padding compris).
        let long = vec![b'a'; 1000];
        assert_eq!(
            hex(&sha256(&long)),
            "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
        );
    }

    #[test]
    fn hmac_sha256_matches_rfc4231_vectors() {
        // RFC 4231, cas 1 et 2.
        assert_eq!(
            hex(&hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Cas 6 : clé plus longue qu'un bloc (hachée d'abord).
        assert_eq!(
            hex(&hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
        assert_eq!(
            hex(&hmac_sha256(
                b"key",
                b"The quick brown fox jumps over the lazy dog"
            )),
            "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
    }

    #[test]
    fn b64url_round_trips_and_rejects_foreign_encodings() {
        for payload in [
            &b""[..],
            b"f",
            b"fo",
            b"foo",
            b"foob",
            b"fooba",
            b"foobar",
            b"\xff\x00\xfe",
        ] {
            let encoded = b64url_encode(payload);
            assert!(
                !encoded.contains('=') && !encoded.contains('+') && !encoded.contains('/'),
                "alphabet URL sans padding : {encoded}"
            );
            assert_eq!(b64url_decode(&encoded).unwrap(), payload);
        }
        assert_eq!(b64url_decode("").unwrap(), Vec::<u8>::new());
        assert_eq!(b64url_decode("Zg").unwrap(), b"f");
        assert_eq!(b64url_decode("Zm8").unwrap(), b"fo");
        assert!(matches!(b64url_decode("Zg=="), Err(StateError::Malformed)));
        // Longueur % 4 == 1 : encodage non canonique (6 bits résiduels).
        assert!(matches!(b64url_decode("Zm9vY"), Err(StateError::Malformed)));
        assert!(matches!(b64url_decode("Z!8"), Err(StateError::Malformed)));
    }

    #[test]
    fn token_round_trip_and_tampering_is_refused() {
        let key = b"0123456789abcdef0123456789abcdef";
        let payload = serde_json::json!({ "v": 1, "m": "tools/call", "t": 2 });
        let token = mint_token(key, &payload);
        assert_eq!(verify_token(key, &token).unwrap(), payload);

        // Octet du payload modifié → signature invalide.
        let (payload_part, signature_part) = token.split_once('.').unwrap();
        let mut tampered = payload_part.to_string();
        let last = if tampered.ends_with('A') { 'B' } else { 'A' };
        tampered.pop();
        tampered.push(last);
        let tampered = format!("{tampered}.{signature_part}");
        assert_eq!(
            verify_token(key, &tampered).unwrap_err(),
            StateError::BadSignature
        );

        // Octet de signature modifié → signature invalide.
        let mut bad_sig = signature_part.to_string();
        let last = if bad_sig.ends_with('A') { 'B' } else { 'A' };
        bad_sig.pop();
        bad_sig.push(last);
        assert_eq!(
            verify_token(key, &format!("{payload_part}.{bad_sig}")).unwrap_err(),
            StateError::BadSignature
        );

        // Structure invalide.
        assert_eq!(
            verify_token(key, "garbage").unwrap_err(),
            StateError::Malformed
        );
        assert_eq!(
            verify_token(key, ".abc").unwrap_err(),
            StateError::Malformed
        );
        // Clé différente → signature invalide (jamais « payload illisible »).
        assert_eq!(
            verify_token(b"another-key-another-key-another-k", &token).unwrap_err(),
            StateError::BadSignature
        );
    }

    #[test]
    fn digest_is_stable_and_sensitive_to_every_field() {
        let base =
            serde_json::json!({ "method": "tools/call", "name": "x", "arguments": { "a": 1 } });
        let digest = digest_salient(&base);
        assert_eq!(digest.len(), 32);
        assert_eq!(digest, digest_salient(&base));
        let reordered =
            serde_json::json!({ "arguments": { "a": 1 }, "name": "x", "method": "tools/call" });
        // `serde_json` ordonne les clés : l'ordre d'insertion n'influence pas.
        assert_eq!(digest, digest_salient(&reordered));
        for changed in [
            serde_json::json!({ "method": "prompts/get", "name": "x", "arguments": { "a": 1 } }),
            serde_json::json!({ "method": "tools/call", "name": "y", "arguments": { "a": 1 } }),
            serde_json::json!({ "method": "tools/call", "name": "x", "arguments": { "a": 2 } }),
            serde_json::json!({ "method": "tools/call", "name": "x", "arguments": null }),
        ] {
            assert_ne!(digest, digest_salient(&changed), "{changed}");
        }
    }

    #[test]
    fn harvested_keys_are_32_bytes_and_distinct() {
        let first = harvest_key();
        let second = harvest_key();
        assert_eq!(first.len(), 32);
        assert_eq!(second.len(), 32);
        assert_ne!(first, second, "deux récoltes doivent différer");
    }
}
