//! base64 **standard** (RFC 4648 §4, alphabet `A–Za–z0–9+/`, padding `=`) —
//! encodage seulement, aucune cryptographie.
//!
//! Le profil de `docs/auth.md` n'admet que cette variante : `Content-Digest`,
//! `Signature` et les blocs de clés portent du base64 standard **avec
//! padding**, jamais du base64url ni du base64 non paddé. Le décodeur est donc
//! strict : alphabet fixe, longueur multiple de 4, padding exact, bits de
//! bourrage nuls (une seule écriture possible pour chaque octet).

/// base64 standard avec padding refusé : alphabet, longueur, padding ou bits
/// de bourrage non conformes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InvalidBase64;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode en base64 standard avec padding (`0` octet → chaîne vide, `1` octet
/// → 4 caractères dont `==`).
pub(crate) fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks_exact(3) {
        let [first, second, third] = chunk else {
            // `chunks_exact(3)` garantit trois octets ; le motif évite toute
            // indexation directe sur un tampon venu du réseau.
            return out;
        };
        let triple = (u32::from(*first) << 16) | (u32::from(*second) << 8) | u32::from(*third);
        out.push(ALPHABET[(triple >> 18) as usize & 63] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 63] as char);
        out.push(ALPHABET[(triple >> 6) as usize & 63] as char);
        out.push(ALPHABET[triple as usize & 63] as char);
    }
    match data.chunks_exact(3).remainder() {
        [] => {}
        [first] => {
            let triple = u32::from(*first) << 16;
            out.push(ALPHABET[(triple >> 18) as usize & 63] as char);
            out.push(ALPHABET[(triple >> 12) as usize & 63] as char);
            out.push('=');
            out.push('=');
        }
        [first, second] => {
            let triple = (u32::from(*first) << 16) | (u32::from(*second) << 8);
            out.push(ALPHABET[(triple >> 18) as usize & 63] as char);
            out.push(ALPHABET[(triple >> 12) as usize & 63] as char);
            out.push(ALPHABET[(triple >> 6) as usize & 63] as char);
            out.push('=');
        }
        _ => unreachable!("chunks_exact(3) ne rend jamais plus de deux octets"),
    }
    out
}

/// Décode du base64 standard **strict**.
///
/// Refusés : tout caractère hors alphabet (donc `-` et `_` du base64url), tout
/// blanc, longueur non multiple de 4, `=` ailleurs qu'en fin, padding
/// incohérent, et bits de bourrage non nuls (deux encodages pour un même
/// octet rendraient les empreintes ambiguës).
pub(crate) fn decode(input: &str) -> Result<Vec<u8>, InvalidBase64> {
    let bytes = input.as_bytes();
    if bytes.len() % 4 != 0 {
        return Err(InvalidBase64);
    }
    let quads = bytes.len() / 4;
    let mut out = Vec::with_capacity(quads * 3);
    for (index, chunk) in bytes.chunks_exact(4).enumerate() {
        let [_, _, third, fourth] = chunk else {
            return Err(InvalidBase64);
        };
        let last = index + 1 == quads;
        let padding = match (*third, *fourth) {
            (b'=', b'=') => 2,
            (_, b'=') => 1,
            _ => 0,
        };
        // Le padding n'existe que sur le dernier quantum.
        if padding > 0 && !last {
            return Err(InvalidBase64);
        }
        let mut triple: u32 = 0;
        for (position, byte) in chunk.iter().enumerate() {
            let value = if *byte == b'=' {
                if position < 4 - padding {
                    return Err(InvalidBase64);
                }
                0
            } else {
                value_of(*byte)?
            };
            triple = (triple << 6) | u32::from(value);
        }
        match padding {
            // Les 2 derniers bits doivent être nuls (1 octet utile).
            2 => {
                if triple & 0x00_00_ff_ff != 0 {
                    return Err(InvalidBase64);
                }
                out.push((triple >> 16) as u8);
            }
            // Les 4 derniers bits doivent être nuls (2 octets utiles).
            1 => {
                if triple & 0x00_00_00_ff != 0 {
                    return Err(InvalidBase64);
                }
                out.push((triple >> 16) as u8);
                out.push((triple >> 8) as u8);
            }
            _ => {
                out.push((triple >> 16) as u8);
                out.push((triple >> 8) as u8);
                out.push(triple as u8);
            }
        }
    }
    Ok(out)
}

/// Valeur d'un caractère de l'alphabet, ou refus.
fn value_of(byte: u8) -> Result<u8, InvalidBase64> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err(InvalidBase64),
    }
}

/// Décode en un tableau de longueur fixe — utilisé pour les clés (32 octets),
/// les empreintes (32) et les signatures (64).
pub(crate) fn decode_array<const N: usize>(input: &str) -> Result<[u8; N], InvalidBase64> {
    let bytes = decode(input)?;
    <[u8; N]>::try_from(bytes.as_slice()).map_err(|_| InvalidBase64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4648 §10 : les six vecteurs de référence.
    #[test]
    fn rfc4648_vectors() {
        for (clear, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(clear.as_bytes()), encoded, "encodage de {clear:?}");
            assert_eq!(
                decode(encoded).expect("vecteur RFC valide"),
                clear.as_bytes(),
                "décodage de {encoded:?}"
            );
        }
    }

    /// RFC 9530 annexe D : `{"hello": "world"}` → `sha-256=:X48E…DBPE=:`.
    #[test]
    fn vecteur_sha256_de_rfc9530() {
        let digest: [u8; 32] =
            decode_array("X48E9qOokqqrvdts8nOJRJN3OWDUoyWxBf7kbu9DBPE=").expect("base64 valide");
        assert_eq!(
            crate::request_state::sha256(b"{\"hello\": \"world\"}"),
            digest
        );
        assert_eq!(
            encode(&digest),
            "X48E9qOokqqrvdts8nOJRJN3OWDUoyWxBf7kbu9DBPE="
        );
    }

    #[test]
    fn les_32_octets_tiennent_en_44_caracteres_avec_un_padding() {
        let digest = crate::request_state::sha256(b"corps");
        let text = encode(&digest);
        assert_eq!(text.len(), 44, "{text}");
        assert!(text.ends_with('='), "{text}");
        assert_eq!(decode_array::<32>(&text).expect("relu"), digest);
    }

    #[test]
    fn le_base64url_est_refuse() {
        // Mêmes octets que « foobar », alphabet URL.
        assert_eq!(decode("Zm9vYmFy").expect("standard"), b"foobar");
        assert_eq!(decode("Zm9vYmF-"), Err(InvalidBase64));
        assert_eq!(decode("Zm9vYmF_"), Err(InvalidBase64));
        assert_eq!(decode("_-_-"), Err(InvalidBase64));
    }

    #[test]
    fn le_padding_est_exige_et_exact() {
        // Non paddé.
        assert_eq!(decode("Zg"), Err(InvalidBase64));
        assert_eq!(decode("Zm8"), Err(InvalidBase64));
        // Padding partiel sur un quantum incomplet, ou trop.
        assert_eq!(decode("Zg="), Err(InvalidBase64));
        assert_eq!(decode("Zg==="), Err(InvalidBase64));
        assert_eq!(decode("Z==="), Err(InvalidBase64));
        // Padding au milieu.
        assert_eq!(decode("Zg==Zg=="), Err(InvalidBase64));
        // Longueur non multiple de 4.
        assert_eq!(decode("Zg==="), Err(InvalidBase64));
        assert_eq!(decode("Zm9vY"), Err(InvalidBase64));
    }

    #[test]
    fn les_bits_de_bourrage_doivent_etre_nuls() {
        // « Zg== » est canonique ; « Zh== » porte un bit de bourrage à 1.
        assert!(decode("Zg==").is_ok());
        assert_eq!(decode("Zh=="), Err(InvalidBase64));
        assert!(decode("Zm8=").is_ok());
        assert_eq!(decode("Zm9="), Err(InvalidBase64));
    }

    #[test]
    fn aucun_blanc_ni_caractere_hors_alphabet() {
        for input in [
            " Zm9v",
            "Zm9v ",
            "Zm 9v",
            "Zm9v\n",
            "\tZm9v",
            "Zm9v!",
            "Zm9vé",
            "Zm9v\u{0}",
        ] {
            assert_eq!(decode(input), Err(InvalidBase64), "accepté : {input:?}");
        }
    }

    #[test]
    fn l_aller_retour_couvre_les_trois_restes() {
        for length in 0..64usize {
            let data: Vec<u8> = (0..length).map(|i| (i * 37 % 256) as u8).collect();
            let encoded = encode(&data);
            assert_eq!(encoded.len() % 4, 0, "longueur {length}");
            assert_eq!(decode(&encoded).expect("relu"), data, "longueur {length}");
        }
    }
}
