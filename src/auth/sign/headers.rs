//! Analyse des champs `Signature-Input`, `Signature`, `Content-Digest` et
//! construction de la chaîne à signer (RFC 9421 §2.5, profil de
//! `docs/auth.md` §2).
//!
//! Tout ce qui vient du réseau passe par un curseur d'octets qui **ne peut pas
//! déborder** : aucune indexation directe, aucune conversion non vérifiée,
//! aucun `unwrap`. Les longueurs sont bornées (composants et paramètres) pour
//! qu'un en-tête géant ne se transforme pas en allocation géante. Un refus
//! porte toujours un motif de la liste fermée de `docs/auth.md` §2.7.

use super::base64;
use super::nonce::Nonce;
use crate::auth::AuthError;

/// Nombre maximal de composants analysés (le profil en admet sept).
const MAX_COMPONENTS: usize = 16;
/// Nombre maximal de paramètres de signature analysés (le profil en admet cinq).
const MAX_PARAMS: usize = 16;

/// Composants couverts admis, avec `mcp-name` (sept).
pub(crate) const WITH_NAME: [Component; 7] = [
    Component::Method,
    Component::Path,
    Component::Authority,
    Component::ContentDigest,
    Component::ContentType,
    Component::McpMethod,
    Component::McpName,
];

/// Composants couverts admis, sans `mcp-name` (six).
pub(crate) const WITHOUT_NAME: [Component; 6] = [
    Component::Method,
    Component::Path,
    Component::Authority,
    Component::ContentDigest,
    Component::ContentType,
    Component::McpMethod,
];

/// Un composant couvert du profil.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Component {
    Method,
    Path,
    Authority,
    ContentDigest,
    ContentType,
    McpMethod,
    McpName,
}

impl Component {
    /// Nom tel qu'il apparaît dans `Signature-Input`.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Method => "@method",
            Self::Path => "@path",
            Self::Authority => "@authority",
            Self::ContentDigest => "content-digest",
            Self::ContentType => "content-type",
            Self::McpMethod => "mcp-method",
            Self::McpName => "mcp-name",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "@method" => Self::Method,
            "@path" => Self::Path,
            "@authority" => Self::Authority,
            "content-digest" => Self::ContentDigest,
            "content-type" => Self::ContentType,
            "mcp-method" => Self::McpMethod,
            "mcp-name" => Self::McpName,
            _ => return None,
        })
    }
}

/// `Signature-Input` analysé et validé (hors fenêtre d'horloge).
#[derive(Debug)]
pub(crate) struct SignatureInput<'a> {
    /// Étiquette du membre (`sig1`).
    pub(crate) label: &'a str,
    /// Liste des composants couverts, dans l'ordre reçu.
    pub(crate) components: Vec<Component>,
    /// Texte brut de la valeur du membre, après `label=` — **c'est** la
    /// dernière ligne de la chaîne à signer, caractère pour caractère.
    pub(crate) params: &'a str,
    /// `created`, secondes UNIX.
    pub(crate) created: u64,
    /// `nonce`, 16 octets décodés.
    pub(crate) nonce: Nonce,
    /// `keyid`, déjà validé contre la grammaire de `docs/auth.md` §3.
    pub(crate) keyid: &'a str,
}

/// Curseur d'octets sans dépassement possible.
struct Cursor<'a> {
    input: &'a [u8],
    text: &'a str,
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            input: text.as_bytes(),
            text,
            pos: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.pos += 1;
        Some(byte)
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn eat_ows(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.pos += 1;
        }
    }

    fn at_end(&self) -> bool {
        self.pos >= self.input.len()
    }

    /// Tranche `[start, pos)` en `&str` — les bornes viennent du curseur et
    /// tombent toujours sur des caractères ASCII.
    fn slice(&self, start: usize) -> &'a str {
        self.text.get(start..self.pos).unwrap_or("")
    }

    /// Clé de dictionnaire RFC 8941 : `( lcalpha / "*" ) *( … )`.
    fn parse_key(&mut self) -> Option<&'a str> {
        let start = self.pos;
        match self.peek()? {
            b'a'..=b'z' | b'*' => {
                self.pos += 1;
            }
            _ => return None,
        }
        while let Some(byte) = self.peek() {
            match byte {
                b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'.' | b'*' => self.pos += 1,
                _ => break,
            }
        }
        Some(self.slice(start))
    }

    /// Chaîne RFC 8941 **sans échappement** : le profil n'en a pas besoin
    /// (`keyid`, `alg`, `tag`, noms de composants sont ASCII sans `\`), et
    /// refuser l'échappement évite toute désambiguïsation.
    fn parse_string(&mut self) -> Option<&'a str> {
        if !self.eat(b'"') {
            return None;
        }
        let start = self.pos;
        loop {
            match self.peek()? {
                b'"' => {
                    let value = self.slice(start);
                    self.pos += 1;
                    return Some(value);
                }
                b'\\' => return None,
                0x20..=0x7e => self.pos += 1,
                _ => return None,
            }
        }
    }
}

/// Valeur d'un paramètre de signature, réduite aux types utiles au profil.
enum ParamValue<'a> {
    String(&'a str),
    Integer(i64),
    /// Booléen, séquence d'octets ou jeton : consommé, puis refusé (le profil
    /// n'admet que des chaînes et un entier).
    Other,
}

/// Refus motivé, motif de la liste fermée.
fn refuse(reason: &str) -> AuthError {
    AuthError::new(reason)
}

/// Analyse et valide un membre de `Signature-Input`.
///
/// Refus (motifs de §2.7) : liste de composants hors du profil
/// (`covered_set_not_allowed`), paramètre de composant (`component_param`),
/// paramètres absents, en trop, dans le désordre ou du mauvais type
/// (`params_not_allowed`), algorithme (`alg_not_ed25519`), étiquette
/// (`tag_mismatch`), `keyid` (`keyid_malformed`).
pub(crate) fn parse_signature_input(value: &str) -> Result<SignatureInput<'_>, AuthError> {
    let mut cursor = Cursor::new(value);
    cursor.eat_ows();
    let label = cursor
        .parse_key()
        .ok_or_else(|| refuse("covered_set_not_allowed"))?;
    if !cursor.eat(b'=') {
        return Err(refuse("covered_set_not_allowed"));
    }
    // Le texte brut du membre commence ici : c'est la liste des composants et
    // les paramètres, tels qu'ils ont été signés.
    let start = cursor.pos;
    if !cursor.eat(b'(') {
        return Err(refuse("covered_set_not_allowed"));
    }

    let mut components = Vec::new();
    cursor.eat_ows();
    if !cursor.eat(b')') {
        loop {
            let name = cursor
                .parse_string()
                .ok_or_else(|| refuse("covered_set_not_allowed"))?;
            // Un composant porteur d'un paramètre (`;sf`, `;bs`, `;key`…) est
            // refusé : le profil n'en admet aucun.
            if cursor.peek() == Some(b';') {
                return Err(refuse("component_param"));
            }
            components
                .push(Component::from_name(name).ok_or_else(|| refuse("covered_set_not_allowed"))?);
            if components.len() > MAX_COMPONENTS {
                return Err(refuse("covered_set_not_allowed"));
            }
            cursor.eat_ows();
            if cursor.eat(b')') {
                break;
            }
            if cursor.peek() != Some(b'"') {
                return Err(refuse("covered_set_not_allowed"));
            }
        }
    }

    let mut end = cursor.pos;
    let mut params: Vec<(&str, ParamValue<'_>)> = Vec::new();
    while cursor.eat(b';') {
        let key = cursor
            .parse_key()
            .ok_or_else(|| refuse("params_not_allowed"))?;
        if !cursor.eat(b'=') {
            return Err(refuse("params_not_allowed"));
        }
        let value = parse_param_value(&mut cursor).ok_or_else(|| refuse("params_not_allowed"))?;
        params.push((key, value));
        if params.len() > MAX_PARAMS {
            return Err(refuse("params_not_allowed"));
        }
        // Le texte brut de la valeur du membre s'arrête au dernier paramètre.
        end = cursor.pos;
    }
    cursor.eat_ows();
    // Deux membres dans le même champ : deux signatures.
    if cursor.peek() == Some(b',') {
        return Err(refuse("multiple_signatures"));
    }
    if !cursor.at_end() {
        return Err(refuse("params_not_allowed"));
    }

    // Cinq paramètres, dans cet ordre, aucun autre.
    const ORDER: [&str; 5] = ["created", "nonce", "keyid", "alg", "tag"];
    if params.len() != ORDER.len() {
        return Err(refuse("params_not_allowed"));
    }
    for (index, (key, _)) in params.iter().enumerate() {
        if ORDER.get(index) != Some(key) {
            return Err(refuse("params_not_allowed"));
        }
    }

    let mut created = None;
    let mut nonce = None;
    let mut keyid = None;
    for (key, value) in &params {
        match (*key, value) {
            ("created", ParamValue::Integer(seconds)) => {
                let seconds = u64::try_from(*seconds).map_err(|_| refuse("params_not_allowed"))?;
                created = Some(seconds);
            }
            ("nonce", ParamValue::String(text)) => {
                nonce = Some(decode_nonce(text).ok_or_else(|| refuse("params_not_allowed"))?);
            }
            ("keyid", ParamValue::String(text)) => {
                if !keyid_allowed(text) {
                    return Err(refuse("keyid_malformed"));
                }
                keyid = Some(*text);
            }
            ("alg", ParamValue::String(text)) => {
                if *text != "ed25519" {
                    return Err(refuse("alg_not_ed25519"));
                }
            }
            ("tag", ParamValue::String(text)) => {
                if *text != "litemcp-auth-v1" {
                    return Err(refuse("tag_mismatch"));
                }
            }
            // Type inattendu (`created="…"`, `nonce=12`, booléen, jeton…).
            _ => return Err(refuse("params_not_allowed")),
        }
    }

    Ok(SignatureInput {
        label,
        components,
        params: value.get(start..end).unwrap_or(""),
        created: created.ok_or_else(|| refuse("params_not_allowed"))?,
        nonce: nonce.ok_or_else(|| refuse("params_not_allowed"))?,
        keyid: keyid.ok_or_else(|| refuse("params_not_allowed"))?,
    })
}

/// Un item RFC 8941, réduit à ce que le profil peut porter.
fn parse_param_value<'a>(cursor: &mut Cursor<'a>) -> Option<ParamValue<'a>> {
    match cursor.peek()? {
        b'"' => cursor.parse_string().map(ParamValue::String),
        b'-' | b'0'..=b'9' => {
            let start = cursor.pos;
            let negative = cursor.eat(b'-');
            let digits_start = cursor.pos;
            while matches!(cursor.peek(), Some(b'0'..=b'9')) {
                cursor.pos += 1;
            }
            let digits = cursor.text.get(digits_start..cursor.pos)?;
            // 1 à 15 chiffres, sans zéro de tête (RFC 8941 §3.3.1).
            if digits.is_empty()
                || digits.len() > 15
                || (digits.len() > 1 && digits.starts_with('0'))
            {
                cursor.pos = start;
                return None;
            }
            let mut value: i64 = 0;
            for byte in digits.bytes() {
                value = value
                    .checked_mul(10)?
                    .checked_add(i64::from(byte.checked_sub(b'0')?))?;
            }
            Some(ParamValue::Integer(if negative { -value } else { value }))
        }
        // Booléens, séquences d'octets et jetons : consommés puis refusés par
        // l'appelant (type inattendu), pour que la suite du champ reste
        // analysable.
        b'?' => {
            cursor.bump()?;
            match cursor.bump()? {
                b'0' | b'1' => Some(ParamValue::Other),
                _ => None,
            }
        }
        b':' => {
            cursor.bump()?;
            let mut closed = false;
            while let Some(byte) = cursor.bump() {
                if byte == b':' {
                    closed = true;
                    break;
                }
            }
            closed.then_some(ParamValue::Other)
        }
        _ => {
            let start = cursor.pos;
            while let Some(byte) = cursor.peek() {
                match byte {
                    b'a'..=b'z'
                    | b'A'..=b'Z'
                    | b'0'..=b'9'
                    | b'_'
                    | b'-'
                    | b'.'
                    | b'*'
                    | b'/'
                    | b':' => cursor.pos += 1,
                    _ => break,
                }
            }
            (cursor.pos > start).then_some(ParamValue::Other)
        }
    }
}

/// Grammaire de `docs/auth.md` §3 : `^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$`.
pub(crate) fn keyid_allowed(keyid: &str) -> bool {
    let bytes = keyid.as_bytes();
    if bytes.is_empty() || bytes.len() > 128 {
        return false;
    }
    let Some(first) = bytes.first() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    bytes
        .iter()
        .skip(1)
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
}

/// Décode un `nonce` : 22 caractères base64url **sans padding** = 16 octets.
fn decode_nonce(text: &str) -> Option<Nonce> {
    if text.len() != 22 {
        return None;
    }
    let mut out = [0u8; 16];
    let mut accumulator: u32 = 0;
    let mut bits: u32 = 0;
    let mut written = 0usize;
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        accumulator = (accumulator << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            let decoded = ((accumulator >> bits) & 0xff) as u8;
            *out.get_mut(written)? = decoded;
            written += 1;
        }
    }
    // Les 4 bits restants doivent être nuls (encodage canonique).
    if bits > 0 && accumulator & ((1 << bits) - 1) != 0 {
        return None;
    }
    (written == 16).then_some(out)
}

/// Le jeu de composants est-il l'une des deux listes admises ?
pub(crate) fn covered_set_allowed(components: &[Component]) -> bool {
    components == WITH_NAME || components == WITHOUT_NAME
}

/// Analyse le champ `Signature` : une étiquette et un payload base64.
///
/// Refus : `multiple_signatures` dès qu'un second membre apparaît,
/// `signature_invalid` pour toute autre malformation.
pub(crate) fn parse_signature(value: &str) -> Result<(&str, &str), AuthError> {
    let mut cursor = Cursor::new(value);
    cursor.eat_ows();
    let label = cursor
        .parse_key()
        .ok_or_else(|| refuse("signature_invalid"))?;
    if !cursor.eat(b'=') || !cursor.eat(b':') {
        return Err(refuse("signature_invalid"));
    }
    let start = cursor.pos;
    while let Some(byte) = cursor.peek() {
        if byte == b':' {
            break;
        }
        cursor.pos += 1;
    }
    let payload = cursor.slice(start);
    if !cursor.eat(b':') {
        return Err(refuse("signature_invalid"));
    }
    cursor.eat_ows();
    if cursor.peek() == Some(b',') {
        return Err(refuse("multiple_signatures"));
    }
    if !cursor.at_end() {
        return Err(refuse("signature_invalid"));
    }
    Ok((label, payload))
}

/// Décode un `Content-Digest` du profil : **exactement** `sha-256=:<44>:`.
///
/// Refuse un second membre, `sha-512`, le base64url, le base64 non paddé et
/// toute capitalisation de l'algorithme — motif `content_digest_mismatch`.
pub(crate) fn parse_content_digest(value: &str) -> Result<[u8; 32], AuthError> {
    let mismatch = || refuse("content_digest_mismatch");
    let rest = value.strip_prefix("sha-256=:").ok_or_else(mismatch)?;
    let rest = rest.strip_suffix(':').ok_or_else(mismatch)?;
    // 32 octets → 44 caractères, dont un `=` de padding.
    if rest.len() != 44 {
        return Err(mismatch());
    }
    base64::decode_array::<32>(rest).map_err(|_| mismatch())
}

/// Valeurs des composants couverts, telles que dérivées de la requête reçue.
pub(crate) struct ComponentValues<'a> {
    pub(crate) method: &'a str,
    pub(crate) path: &'a str,
    pub(crate) authority: &'a str,
    pub(crate) content_digest: &'a str,
    pub(crate) content_type: &'a str,
    pub(crate) mcp_method: &'a str,
    pub(crate) mcp_name: Option<&'a str>,
}

/// Construit la chaîne à signer (RFC 9421 §2.5, `docs/auth.md` §2.4).
///
/// Pour chaque composant : `"nom": valeur` puis **un** LF ; dernière ligne
/// `"@signature-params": ` suivie du texte brut du membre, **sans** LF final.
/// Aucun CR, aucune valeur vide, aucun blanc de bord : une valeur qui ne peut
/// pas être représentée est un refus (`covered_set_not_allowed`), jamais une
/// chaîne ambiguë.
pub(crate) fn build_base(
    components: &[Component],
    values: &ComponentValues<'_>,
    params: &str,
) -> Result<String, AuthError> {
    let mut base = String::with_capacity(512);
    for component in components {
        let value = match component {
            Component::Method => values.method,
            Component::Path => values.path,
            Component::Authority => values.authority,
            Component::ContentDigest => values.content_digest,
            Component::ContentType => values.content_type,
            Component::McpMethod => values.mcp_method,
            Component::McpName => values
                .mcp_name
                .ok_or_else(|| refuse("covered_set_not_allowed"))?,
        };
        if value.is_empty() || value.contains(['\r', '\n']) {
            return Err(refuse("covered_set_not_allowed"));
        }
        base.push('"');
        base.push_str(component.name());
        base.push_str("\": ");
        base.push_str(value);
        base.push('\n');
    }
    base.push_str("\"@signature-params\": ");
    base.push_str(params);
    Ok(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Le membre exact de `docs/auth.md` §2.5.
    const REFERENCE: &str = "sig1=(\"@method\" \"@path\" \"@authority\" \"content-digest\" \
\"content-type\" \"mcp-method\" \"mcp-name\");created=1794700800;nonce=\"AAECAwQFBgcICQoLDA0ODw\";\
keyid=\"client-demo\";alg=\"ed25519\";tag=\"litemcp-auth-v1\"";

    #[test]
    fn le_membre_de_reference_est_analyse_et_restitue_a_l_identique() {
        let parsed = parse_signature_input(REFERENCE).expect("membre valide");
        assert_eq!(parsed.label, "sig1");
        assert_eq!(parsed.components, WITH_NAME.to_vec());
        assert_eq!(parsed.created, 1_794_700_800);
        assert_eq!(parsed.keyid, "client-demo");
        assert_eq!(
            parsed.nonce,
            [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
        );
        // Le texte brut est celui qui a été signé : ni reconstruit, ni normalisé.
        assert_eq!(
            parsed.params,
            &REFERENCE["sig1=".len()..],
            "la ligne @signature-params doit être l'octet pour l'octet du membre reçu"
        );
        assert!(parsed.params.starts_with("(\"@method\""));
        assert!(parsed.params.ends_with("tag=\"litemcp-auth-v1\""));
    }

    #[test]
    fn la_liste_sans_mcp_name_est_admise() {
        let sans = REFERENCE.replace(" \"mcp-name\"", "");
        let parsed = parse_signature_input(&sans).expect("six composants");
        assert_eq!(parsed.components, WITHOUT_NAME.to_vec());
    }

    #[test]
    fn les_listes_de_composants_hors_profil_sont_refusees() {
        let cas = [
            // Ordre inversé.
            REFERENCE.replace("\"@method\" \"@path\"", "\"@path\" \"@method\""),
            // Composant manquant.
            REFERENCE.replace(" \"content-type\"", ""),
            // Composant en trop.
            REFERENCE.replace(" \"mcp-name\"", " \"mcp-name\" \"content-length\""),
            // Répétition.
            REFERENCE.replace(" \"mcp-name\"", " \"mcp-name\" \"@method\""),
            // Composant dérivé inconnu.
            REFERENCE.replace("\"mcp-name\"", "\"@query\""),
            // Liste vide.
            REFERENCE.replace(
                "(\"@method\" \"@path\" \"@authority\" \"content-digest\" \"content-type\" \
\"mcp-method\" \"mcp-name\")",
                "()",
            ),
        ];
        for texte in cas {
            // Soit l'analyse échoue, soit la liste est syntaxiquement valide
            // mais hors du profil fermé : dans les deux cas, un refus.
            let refuse = match parse_signature_input(&texte) {
                Err(erreur) => erreur.reason() == "covered_set_not_allowed",
                Ok(parsed) => !covered_set_allowed(&parsed.components),
            };
            assert!(refuse, "liste acceptée à tort : {texte}");
        }
    }

    #[test]
    fn un_parametre_de_composant_est_refuse() {
        for (avant, apres) in [
            ("\"@method\"", "\"@method\";sf"),
            ("\"content-digest\"", "\"content-digest\";bs"),
            ("\"mcp-name\"", "\"mcp-name\";key=\"x\""),
            ("\"@path\"", "\"@path\";req"),
            ("\"@authority\"", "\"@authority\";tr"),
        ] {
            let texte = REFERENCE.replacen(avant, apres, 1);
            let erreur = parse_signature_input(&texte).expect_err(&texte);
            assert_eq!(erreur.reason(), "component_param", "{texte}");
        }
    }

    #[test]
    fn les_parametres_de_signature_sont_exactement_les_cinq_du_profil() {
        let cas = [
            // Manquant.
            (";tag=\"litemcp-auth-v1\"", String::new()),
            // En trop.
            (
                ";tag=\"litemcp-auth-v1\"",
                ";tag=\"litemcp-auth-v1\";expires=1794700900".to_string(),
            ),
            // Désordre.
            (
                ";created=1794700800;nonce=\"AAECAwQFBgcICQoLDA0ODw\"",
                ";nonce=\"AAECAwQFBgcICQoLDA0ODw\";created=1794700800".to_string(),
            ),
            // Mauvais type.
            (";created=1794700800", ";created=\"1794700800\"".to_string()),
        ];
        for (cible, remplacement) in cas {
            let texte = REFERENCE.replacen(cible, &remplacement, 1);
            let erreur = parse_signature_input(&texte).expect_err(&texte);
            assert_eq!(erreur.reason(), "params_not_allowed", "{texte}");
        }
        // Paramètre sans valeur.
        let texte = REFERENCE.replacen(";alg=\"ed25519\"", ";alg", 1);
        assert_eq!(
            parse_signature_input(&texte)
                .expect_err("sans valeur")
                .reason(),
            "params_not_allowed"
        );
    }

    #[test]
    fn algorithme_et_etiquette_sont_verrouilles() {
        let texte = REFERENCE.replace("alg=\"ed25519\"", "alg=\"ed448\"");
        assert_eq!(
            parse_signature_input(&texte).expect_err("alg").reason(),
            "alg_not_ed25519"
        );
        let texte = REFERENCE.replace("tag=\"litemcp-auth-v1\"", "tag=\"other\"");
        assert_eq!(
            parse_signature_input(&texte).expect_err("tag").reason(),
            "tag_mismatch"
        );
    }

    #[test]
    fn le_keyid_est_verrouille_sur_sa_grammaire() {
        for mauvais in ["", "a b", "-a", "a:b c", &"a".repeat(129)] {
            let texte = REFERENCE.replace("client-demo", mauvais);
            let erreur = parse_signature_input(&texte).expect_err(mauvais);
            assert_eq!(erreur.reason(), "keyid_malformed", "{mauvais:?}");
        }
        // Chaîne non analysable (guillemet, antislash, non-ASCII) : le refus
        // tombe avant la grammaire, mais reste un refus.
        for mauvais in ["\"", "a\\b", "aé"] {
            let texte = REFERENCE.replace("client-demo", mauvais);
            let erreur = parse_signature_input(&texte).expect_err(mauvais);
            assert_eq!(erreur.reason(), "params_not_allowed", "{mauvais:?}");
        }
        for bon in ["a", "A0", "client-demo", &"a".repeat(128), "a.b_c:d-e"] {
            let texte = REFERENCE.replace("client-demo", bon);
            assert!(
                parse_signature_input(&texte).is_ok(),
                "refusé à tort : {bon:?}"
            );
        }
    }

    #[test]
    fn le_nonce_doit_faire_seize_octets_canoniques() {
        // 21 caractères, base64 standard, padding, non-ASCII, bits non nuls.
        for mauvais in [
            "AAECAwQFBgcICQoLDA0OD",
            "AAECAwQFBgcICQoLDA0ODw==",
            "AAECAwQFBgcICQoLDA0OD+",
            "AAECAwQFBgcICQoLDA0ODx",
        ] {
            let texte = REFERENCE.replace("AAECAwQFBgcICQoLDA0ODw", mauvais);
            let erreur = parse_signature_input(&texte).expect_err(mauvais);
            assert_eq!(erreur.reason(), "params_not_allowed", "{mauvais:?}");
        }
    }

    #[test]
    fn un_second_membre_est_un_second_signature() {
        let texte = format!("{REFERENCE},sig2=(\"@method\");created=1");
        assert_eq!(
            parse_signature_input(&texte).expect_err("deux").reason(),
            "multiple_signatures"
        );
    }

    #[test]
    fn les_entrees_malformees_ne_paniquent_jamais() {
        let troncons = [
            "",
            " ",
            "sig1",
            "sig1=",
            "sig1=(",
            "sig1=()",
            "sig1=(\"@method\"",
            "sig1=(\"@method\")",
            "sig1=(\"@method\"))",
            "sig1=(\"@method\");",
            "sig1=(\"@method\");created",
            "sig1=(\"@method\");created=",
            "sig1=(\"@method\");created=-1",
            "sig1=(\"@method\");created=9999999999999999",
            "sig1=(\"@method\");created=1;created=2",
            "sig1=(\"@method\");nonce=\"\\\";created=1",
            "@@@",
            "sig1=(\"@method\");;",
            "sig1=(\"@method\");=1",
            "sig1=(\"@method\");created=1;nonce=?:;keyid=\"a\";alg=\"ed25519\";tag=\"litemcp-auth-v1\"",
        ];
        for texte in troncons {
            let resultat = parse_signature_input(texte);
            if let Err(erreur) = resultat {
                assert!(
                    [
                        "covered_set_not_allowed",
                        "component_param",
                        "params_not_allowed",
                        "alg_not_ed25519",
                        "tag_mismatch",
                        "keyid_malformed",
                        "multiple_signatures",
                    ]
                    .contains(&erreur.reason()),
                    "motif hors liste pour {texte:?} : {}",
                    erreur.reason()
                );
            }
        }
        // Un en-tête géant (64 Kio) est refusé en temps borné, sans panique.
        let geant = format!("sig1=(\"@method\" {})", "\"a\" ".repeat(16_000));
        assert!(parse_signature_input(&geant).is_err());
        // Non ASCII partout.
        let non_ascii = "sig1=(«@method»);created=1";
        assert!(parse_signature_input(non_ascii).is_err());
    }

    #[test]
    fn le_champ_signature_est_analyse() {
        fn signature_bytes(value: &str) -> Result<[u8; 64], AuthError> {
            let (label, payload) = parse_signature(value)?;
            assert_eq!(label, "sig1");
            base64::decode_array::<64>(payload).map_err(|_| AuthError::new("signature_invalid"))
        }

        assert!(
            signature_bytes("sig1=:kOnwfzENaxBF6ubQiJGlXkRfUYUJuELYzHnvqwrzpScvO3JchjFOsREa4j7CsRrdiEZifLmZZZEBBFPJqpsJDA==:")
                .is_ok()
        );
        for mauvais in [
            "",
            "sig1",
            "sig1=:",
            "sig1=:abc",
            "sig1=:abc=:",
            "sig1=:abc:def:",
            "sig1=x:abc:",
            ":abc:",
            "sig1=:!!!:",
            "sig1=:AAAA:",
        ] {
            assert!(
                signature_bytes(mauvais).is_err(),
                "accepté à tort : {mauvais:?}"
            );
        }
        // Deux membres.
        assert_eq!(
            parse_signature("sig1=:AA==:,sig2=:AA==:")
                .expect_err("deux")
                .reason(),
            "multiple_signatures"
        );
    }

    #[test]
    fn le_content_digest_est_verrouille_sur_sha256_standard() {
        let bon = "sha-256=:JTvMexIVHBnIyJ6QrKgnW6o2OifuQwJxUf7qaLvtlM0=:";
        assert_eq!(
            base64::encode(&parse_content_digest(bon).expect("valide")),
            "JTvMexIVHBnIyJ6QrKgnW6o2OifuQwJxUf7qaLvtlM0="
        );
        for mauvais in [
            "",
            "sha-256=:JTvMexIVHBnIyJ6QrKgnW6o2OifuQwJxUf7qaLvtlM0=",
            "SHA-256=:JTvMexIVHBnIyJ6QrKgnW6o2OifuQwJxUf7qaLvtlM0=:",
            "sha-512=:JTvMexIVHBnIyJ6QrKgnW6o2OifuQwJxUf7qaLvtlM0=:",
            "sha-256=:JTvMexIVHBnIyJ6QrKgnW6o2OifuQwJxUf7qaLvtlM0-:",
            "sha-256=:JTvMexIVHBnIyJ6QrKgnW6o2OifuQwJxUf7qaLvtlM0:",
            "sha-256=:JTvMexIVHBnIyJ6QrKgnW6o2OifuQwJxUf7qaLvtlM0=:,sha-512=:AAAA:",
            "sha-256=::",
            "sha-256=:AAAA:",
        ] {
            assert_eq!(
                parse_content_digest(mauvais).expect_err(mauvais).reason(),
                "content_digest_mismatch",
                "{mauvais:?}"
            );
        }
    }

    #[test]
    fn la_chaine_a_signer_est_exactement_celle_du_paragraphe_2_5() {
        let parsed = parse_signature_input(REFERENCE).expect("membre valide");
        let valeurs = ComponentValues {
            method: "POST",
            path: "/mcp",
            authority: "127.0.0.1:23080",
            content_digest: "sha-256=:JTvMexIVHBnIyJ6QrKgnW6o2OifuQwJxUf7qaLvtlM0=:",
            content_type: "application/json",
            mcp_method: "tools/call",
            mcp_name: Some("db.query"),
        };
        let base = build_base(&parsed.components, &valeurs, parsed.params).expect("base");
        let attendu = "\"@method\": POST\n\
\"@path\": /mcp\n\
\"@authority\": 127.0.0.1:23080\n\
\"content-digest\": sha-256=:JTvMexIVHBnIyJ6QrKgnW6o2OifuQwJxUf7qaLvtlM0=:\n\
\"content-type\": application/json\n\
\"mcp-method\": tools/call\n\
\"mcp-name\": db.query\n\
\"@signature-params\": (\"@method\" \"@path\" \"@authority\" \"content-digest\" \"content-type\" \"mcp-method\" \"mcp-name\");created=1794700800;nonce=\"AAECAwQFBgcICQoLDA0ODw\";keyid=\"client-demo\";alg=\"ed25519\";tag=\"litemcp-auth-v1\"";
        assert_eq!(base, attendu);
        assert_eq!(base.len(), 427, "longueur du vecteur de référence");
        assert_eq!(base.lines().count(), 8);
        assert!(!base.ends_with('\n'));
        assert!(!base.contains('\r'));
    }

    #[test]
    fn une_valeur_impossible_a_signer_est_un_refus() {
        let parsed = parse_signature_input(REFERENCE).expect("membre valide");
        for (path, content_type, mcp_name) in [
            ("/mcp", "application/json", None),
            ("", "application/json", Some("db.query")),
            ("/mcp", "application/json\r\nX: y", Some("db.query")),
            ("/mcp", "application/json", Some("")),
        ] {
            let valeurs = ComponentValues {
                method: "POST",
                path,
                authority: "127.0.0.1:23080",
                content_digest: "sha-256=:AAAA:",
                content_type,
                mcp_method: "tools/call",
                mcp_name,
            };
            assert_eq!(
                build_base(&parsed.components, &valeurs, parsed.params)
                    .expect_err("valeur impossible")
                    .reason(),
                "covered_set_not_allowed"
            );
        }
    }
}
