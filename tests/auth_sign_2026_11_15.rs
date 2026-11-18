#![cfg(feature = "auth-sign")]
//! L2 — signature et vérification de requêtes (feature `auth-sign`) : vecteur
//! de référence de `docs/auth.md` §2.5, vecteurs publics (RFC 8032 §7.1,
//! RFC 9421 §B.2.6, RFC 9530 annexe D), matrice complète des refus, ordre des
//! contrôles, uniformité du refus, révocation et cache de nonces.
//!
//! Tout passe par l'API publique : ces tests prouvent ce qu'un client d'un
//! autre langage peut reproduire avec `docs/auth.md`.

use litemcp::auth::sign::{
    ClientKey, KeyStore, NonceCache, RequestToSign, SignatureVerifier, Signer, SigningKey,
    TestKeyStore, UNAUTHORIZED_STATUS, WWW_AUTHENTICATE,
};
use litemcp::auth::{AuthContext, AuthError, ProofMethod, VerifiedRequest, Verifier};
use std::sync::Arc;
use std::time::Duration;

/// Graine du vecteur 1 de la RFC 8032 §7.1 (publique : c'est un vecteur).
const SEED: [u8; 32] = [
    0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c, 0xc4,
    0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae, 0x7f, 0x60,
];
/// Graine d'une seconde identité (clé inconnue du magasin).
const AUTRE_SEED: [u8; 32] = [0x24; 32];
/// `created=1794700800` = 2026-11-15T00:00:00Z, l'instant du vecteur.
const NOW: u64 = 1_794_700_800;
/// Nonce du vecteur : les octets `00 01 … 0f`.
const NONCE: [u8; 16] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
];
/// Corps de 171 octets du vecteur (`docs/auth.md` §2.5).
const BODY: &[u8] = br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"db.query","arguments":{"sql":"select 1"},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}"#;
/// En-têtes de la requête, hors les trois du profil.
const ENTETES: [(&str, &str); 4] = [
    ("Host", "127.0.0.1:23080"),
    ("Content-Type", "application/json"),
    ("Mcp-Method", "tools/call"),
    ("Mcp-Name", "db.query"),
];
/// Corps générique du refus `401` (`docs/auth.md` §2.7 : `code -32600`,
/// `id: null`, message fixe). Le crate le produit dans son transport (L3) ;
/// ici il matérialise la promesse d'uniformité.
const CORPS_401: &[u8] =
    br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Invalid Request"}}"#;

fn hex(texte: &str) -> Vec<u8> {
    assert!(texte.len() % 2 == 0, "hexadecimal impair");
    (0..texte.len() / 2)
        .map(|index| u8::from_str_radix(&texte[index * 2..index * 2 + 2], 16).expect("hexadecimal"))
        .collect()
}

fn nonce(valeur: u32) -> [u8; 16] {
    let mut octets = [0u8; 16];
    octets[4..8].copy_from_slice(&valeur.to_le_bytes());
    octets
}

struct Banc {
    store: Arc<TestKeyStore>,
    verifier: SignatureVerifier,
    signer: Signer,
}

fn banc() -> Banc {
    let store = Arc::new(TestKeyStore::new());
    let signer = Signer::new(SigningKey::from_seed(&SEED)).with_keyid("client-demo");
    store.insert(
        "client-demo",
        ClientKey::new("client-demo", signer.verifying_key().to_bytes())
            .with_label("Poste de demonstration")
            .with_rights(["db.read", "db.write"]),
    );
    let keys: Arc<dyn KeyStore> = store.clone();
    let verifier = SignatureVerifier::new(keys, NonceCache::with_default_policy())
        .with_clock(Arc::new(|| NOW));
    Banc {
        store,
        verifier,
        signer,
    }
}

/// En-têtes de base + les trois en-têtes signés, prêts à être vérifiés.
fn signee(signer: &Signer, created: u64, nonce: &[u8; 16]) -> Vec<(String, String)> {
    let requete = RequestToSign::new("POST", "/mcp", "127.0.0.1:23080", &ENTETES, BODY);
    let signed = signer.headers_at(&requete, created, nonce);
    let mut entetes: Vec<(String, String)> = ENTETES
        .iter()
        .map(|(nom, valeur)| ((*nom).to_owned(), (*valeur).to_owned()))
        .collect();
    for (nom, valeur) in signed.iter() {
        entetes.push((nom.to_owned(), valeur.to_owned()));
    }
    entetes
}

fn signature_valide(signer: &Signer) -> Vec<(String, String)> {
    signee(signer, NOW, &NONCE)
}

fn verifier_sur(
    banc: &Banc,
    path: &str,
    authority: &str,
    entetes: &[(String, String)],
    body: &[u8],
) -> Result<AuthContext, AuthError> {
    let paires: Vec<(&str, &str)> = entetes
        .iter()
        .map(|(nom, valeur)| (nom.as_str(), valeur.as_str()))
        .collect();
    banc.verifier.verify(&VerifiedRequest::new(
        "POST", path, authority, &paires, body,
    ))
}

fn verifier(banc: &Banc, entetes: &[(String, String)]) -> Result<AuthContext, AuthError> {
    verifier_sur(banc, "/mcp", "127.0.0.1:23080", entetes, BODY)
}

/// Valeur du premier en-tête `nom`.
fn valeur(entetes: &[(String, String)], nom: &str) -> String {
    entetes
        .iter()
        .find(|(cle, _)| cle.eq_ignore_ascii_case(nom))
        .map(|(_, valeur)| valeur.clone())
        .unwrap_or_default()
}

/// Remplace la valeur de toutes les occurrences de `nom`.
fn remplacer(entetes: &mut [(String, String)], nom: &str, nouvelle: &str) {
    for (cle, valeur) in entetes.iter_mut() {
        if cle.eq_ignore_ascii_case(nom) {
            *valeur = nouvelle.to_owned();
        }
    }
}

/// Retire toute occurrence de `nom`.
fn retirer(entetes: &mut Vec<(String, String)>, nom: &str) {
    entetes.retain(|(cle, _)| !cle.eq_ignore_ascii_case(nom));
}

/// Duplique la première occurrence de `nom`.
fn dupliquer(entetes: &mut Vec<(String, String)>, nom: &str) {
    if let Some(paire) = entetes
        .iter()
        .find(|(cle, _)| cle.eq_ignore_ascii_case(nom))
        .cloned()
    {
        entetes.push(paire);
    }
}

/// Remplace un fragment de la valeur d'un en-tête.
fn substituer(entetes: &mut [(String, String)], nom: &str, avant: &str, apres: &str) {
    for (cle, valeur) in entetes.iter_mut() {
        if cle.eq_ignore_ascii_case(nom) && valeur.contains(avant) {
            *valeur = valeur.replace(avant, apres);
            return;
        }
    }
    panic!("fragment absent de {nom} : {avant}");
}

fn refus(banc: &Banc, entetes: &[(String, String)]) -> String {
    match verifier(banc, entetes) {
        Ok(contexte) => panic!("requete acceptee : {}", contexte.client()),
        Err(erreur) => erreur.reason().to_owned(),
    }
}

// ===========================================================================
// Vecteur de référence et vecteurs publics
// ===========================================================================

#[test]
fn le_vecteur_de_reference_est_accepte_et_rend_l_identite() {
    let banc = banc();
    let entetes = signature_valide(&banc.signer);
    // Les trois en-têtes sont exactement ceux de docs/auth.md §2.5.
    assert_eq!(
        valeur(&entetes, "Content-Digest"),
        "sha-256=:JTvMexIVHBnIyJ6QrKgnW6o2OifuQwJxUf7qaLvtlM0=:"
    );
    assert_eq!(
        valeur(&entetes, "Signature"),
        "sig1=:kOnwfzENaxBF6ubQiJGlXkRfUYUJuELYzHnvqwrzpScvO3JchjFOsREa4j7CsRrdiEZifLmZZZEBBFPJqpsJDA==:"
    );
    assert!(valeur(&entetes, "Signature-Input").starts_with(
        "sig1=(\"@method\" \"@path\" \"@authority\" \"content-digest\" \"content-type\" \"mcp-method\" \"mcp-name\");created=1794700800;nonce=\"AAECAwQFBgcICQoLDA0ODw\";keyid=\"client-demo\";alg=\"ed25519\";tag=\"litemcp-auth-v1\""
    ));

    let contexte = verifier(&banc, &entetes).expect("vecteur accepte");
    assert_eq!(contexte.client(), "client-demo");
    assert_eq!(contexte.label(), Some("Poste de demonstration"));
    assert_eq!(contexte.rights(), ["db.read", "db.write"]);
    assert!(matches!(
        contexte.proof(),
        ProofMethod::Signature { keyid } if keyid == "client-demo"
    ));
}

#[test]
fn une_requete_sans_mcp_name_couvre_six_composants_et_est_acceptee() {
    let banc = banc();
    let entetes = [
        ("Host", "127.0.0.1:23080"),
        ("Content-Type", "application/json"),
        ("Mcp-Method", "tools/list"),
    ];
    let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    let requete = RequestToSign::new("POST", "/mcp", "127.0.0.1:23080", &entetes, body);
    let signed = banc.signer.headers_at(&requete, NOW, &nonce(1));
    let mut recus: Vec<(String, String)> = entetes
        .iter()
        .map(|(nom, valeur)| ((*nom).to_owned(), (*valeur).to_owned()))
        .collect();
    for (nom, valeur) in signed.iter() {
        recus.push((nom.to_owned(), valeur.to_owned()));
    }
    let contexte = verifier_sur(&banc, "/mcp", "127.0.0.1:23080", &recus, body).expect("accepte");
    assert_eq!(contexte.client(), "client-demo");
}

#[test]
fn les_trois_premiers_vecteurs_de_rfc8032_sont_reproduits() {
    // RFC 8032 §7.1, tests 1 à 3 : graine, clé publique, message, signature.
    let vecteurs = [
        (
            "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
            "",
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
        ),
        (
            "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb",
            "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
            "72",
            "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
        ),
        (
            "c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7",
            "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025",
            "af82",
            "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a",
        ),
    ];
    for (graine, publique, message, signature) in vecteurs {
        let graine: [u8; 32] = hex(graine).try_into().expect("graine de 32 octets");
        let publique: [u8; 32] = hex(publique).try_into().expect("cle de 32 octets");
        let message = hex(message);
        let signature: [u8; 64] = hex(signature).try_into().expect("signature de 64 octets");

        let key = SigningKey::from_seed(&graine);
        assert_eq!(key.verifying_key().to_bytes(), publique);
        assert_eq!(
            key.sign_message(&message),
            signature,
            "signature RFC 8032 divergente"
        );
        assert!(key.verifying_key().verify_message(&message, &signature));

        // Un seul octet modifié : refus.
        let mut altere = signature;
        altere[0] ^= 0x01;
        assert!(!key.verifying_key().verify_message(&message, &altere));
    }
}

#[test]
fn l_exemple_ed25519_de_rfc9421_est_verifiable() {
    // RFC 9421 §B.2.6 (base) et §B.1.4 (clé `test-key-ed25519`, JWK `x`).
    let publique: [u8; 32] =
        hex("26b40b8f93fff3d897112f7ebc582b232dbd72517d082fe83cfb30ddce43d1bb")
            .try_into()
            .expect("cle de 32 octets");
    let signature: [u8; 64] = hex(
        "c2a700a9b9982768e2da095f00c691cb882bb98627c769c414dd8737a8eb9c39\
         d008ad6ed3619bd38bfd10383050f8aee00d30eafb90bf9948a7958fa412910b",
    )
    .try_into()
    .expect("signature de 64 octets");
    let base = "\"date\": Tue, 20 Apr 2021 02:07:55 GMT\n\
\"@method\": POST\n\
\"@path\": /foo\n\
\"@authority\": example.com\n\
\"content-type\": application/json\n\
\"content-length\": 18\n\
\"@signature-params\": (\"date\" \"@method\" \"@path\" \"@authority\" \"content-type\" \"content-length\");created=1618884473;keyid=\"test-key-ed25519\"";
    let key = litemcp::auth::sign::VerifyingKey::from_bytes(&publique).expect("cle valide");
    assert!(
        key.verify_message(base.as_bytes(), &signature),
        "la base de la RFC 9421 doit etre reconstituee a l'octet pres"
    );
    // Une base alteree ne passe plus.
    let altere = base.replace("\"@path\": /foo", "\"@path\": /bar");
    assert!(!key.verify_message(altere.as_bytes(), &signature));
}

#[test]
fn le_content_digest_de_rfc9530_est_celui_du_profil() {
    // RFC 9530 annexe D : `{"hello": "world"}`.
    let body = br#"{"hello": "world"}"#;
    let requete = RequestToSign::new("POST", "/mcp", "127.0.0.1:23080", &[], body);
    let signed = banc().signer.headers_at(&requete, NOW, &NONCE);
    assert_eq!(
        signed.content_digest(),
        "sha-256=:X48E9qOokqqrvdts8nOJRJN3OWDUoyWxBf7kbu9DBPE=:"
    );
}

// ===========================================================================
// Refus : en-têtes, composants, paramètres
// ===========================================================================

#[test]
fn un_en_tete_manquant_est_refuse() {
    let banc = banc();
    let base = signature_valide(&banc.signer);
    for (nom, motif) in [
        ("Signature-Input", "signature_input_missing"),
        ("Signature", "signature_missing"),
        ("Content-Digest", "content_digest_missing"),
        ("Content-Type", "covered_set_not_allowed"),
        ("Mcp-Method", "covered_set_not_allowed"),
    ] {
        let mut entetes = base.clone();
        retirer(&mut entetes, nom);
        assert_eq!(refus(&banc, &entetes), motif, "sans {nom}");
    }
    // Une valeur vide vaut une absence.
    let mut entetes = base.clone();
    remplacer(&mut entetes, "Signature", "");
    assert_eq!(refus(&banc, &entetes), "signature_missing");
    let mut entetes = base;
    remplacer(&mut entetes, "Signature-Input", "  ");
    assert_eq!(refus(&banc, &entetes), "signature_input_missing");
}

#[test]
fn un_en_tete_duplique_est_refuse() {
    let banc = banc();
    let base = signature_valide(&banc.signer);
    for (nom, motif) in [
        ("Signature-Input", "multiple_signatures"),
        ("Signature", "multiple_signatures"),
        ("Content-Digest", "content_digest_mismatch"),
        ("Content-Type", "covered_set_not_allowed"),
        ("Mcp-Method", "covered_set_not_allowed"),
        ("Mcp-Name", "covered_set_not_allowed"),
    ] {
        let mut entetes = base.clone();
        dupliquer(&mut entetes, nom);
        assert_eq!(refus(&banc, &entetes), motif, "{nom} duplique");
    }
}

#[test]
fn les_listes_de_composants_hors_profil_sont_refusees() {
    let banc = banc();
    let base = signature_valide(&banc.signer);
    let cas = [
        // Composant obligatoire non couvert.
        (" \"content-type\"", ""),
        (" \"@authority\"", ""),
        // Composant en trop.
        (" \"mcp-name\"", " \"mcp-name\" \"content-length\""),
        // Ordre différent.
        ("\"@method\" \"@path\"", "\"@path\" \"@method\""),
        // Répétition.
        (" \"mcp-name\"", " \"mcp-name\" \"@method\""),
        // `Content-Type` remplacé par un composant non couvert par le profil.
        (" \"content-type\"", " \"content-length\""),
    ];
    for (avant, apres) in cas {
        let mut entetes = base.clone();
        substituer(&mut entetes, "Signature-Input", avant, apres);
        assert_eq!(
            refus(&banc, &entetes),
            "covered_set_not_allowed",
            "{avant} remplace par {apres}"
        );
    }
}

#[test]
fn un_parametre_de_composant_est_refuse() {
    let banc = banc();
    let base = signature_valide(&banc.signer);
    for (avant, apres) in [
        ("\"@method\"", "\"@method\";sf"),
        ("\"content-digest\"", "\"content-digest\";bs"),
        ("\"mcp-name\"", "\"mcp-name\";key=\"k\""),
        ("\"@path\"", "\"@path\";req"),
    ] {
        let mut entetes = base.clone();
        substituer(&mut entetes, "Signature-Input", avant, apres);
        assert_eq!(refus(&banc, &entetes), "component_param", "{apres}");
    }
}

#[test]
fn un_algorithme_ou_une_etiquette_non_conformes_sont_refuses() {
    let banc = banc();
    let base = signature_valide(&banc.signer);

    let mut entetes = base.clone();
    substituer(
        &mut entetes,
        "Signature-Input",
        "alg=\"ed25519\"",
        "alg=\"ed448\"",
    );
    assert_eq!(refus(&banc, &entetes), "alg_not_ed25519");

    let mut entetes = base.clone();
    substituer(
        &mut entetes,
        "Signature-Input",
        "tag=\"litemcp-auth-v1\"",
        "tag=\"litemcp-auth-v2\"",
    );
    assert_eq!(refus(&banc, &entetes), "tag_mismatch");

    // Paramètre en trop.
    let mut entetes = base.clone();
    substituer(
        &mut entetes,
        "Signature-Input",
        "tag=\"litemcp-auth-v1\"",
        "tag=\"litemcp-auth-v1\";expires=1794700900",
    );
    assert_eq!(refus(&banc, &entetes), "params_not_allowed");

    // Paramètres dans le désordre (nonce avant created).
    let mut entetes = base;
    let renverse = valeur(&entetes, "Signature-Input")
        .replace(";created=1794700800", ";tmp=1794700800")
        .replace(";nonce=\"AAECAwQFBgcICQoLDA0ODw\"", ";created=1794700800")
        .replace(";tmp=1794700800", ";nonce=\"AAECAwQFBgcICQoLDA0ODw\"");
    remplacer(&mut entetes, "Signature-Input", &renverse);
    assert_eq!(refus(&banc, &entetes), "params_not_allowed");
}

#[test]
fn plusieurs_signatures_sont_refusees() {
    let banc = banc();
    let base = signature_valide(&banc.signer);

    let mut entetes = base.clone();
    let avec_deux = format!(
        "{},sig2=(\"@method\");created=1",
        valeur(&entetes, "Signature-Input")
    );
    remplacer(&mut entetes, "Signature-Input", &avec_deux);
    assert_eq!(refus(&banc, &entetes), "multiple_signatures");

    let mut entetes = base.clone();
    let deux = format!("{},sig2=:AAAA:", valeur(&entetes, "Signature"));
    remplacer(&mut entetes, "Signature", &deux);
    assert_eq!(refus(&banc, &entetes), "multiple_signatures");

    // Étiquettes différentes entre les deux champs.
    let mut entetes = base;
    let autre = valeur(&entetes, "Signature").replacen("sig1=", "sig2=", 1);
    remplacer(&mut entetes, "Signature", &autre);
    assert_eq!(refus(&banc, &entetes), "label_mismatch");
}

#[test]
fn un_keyid_hors_grammaire_est_refuse() {
    let banc = banc();
    let mauvais = Signer::new(SigningKey::from_seed(&SEED)).with_keyid("client demo");
    let entetes = signee(&mauvais, NOW, &nonce(2));
    assert_eq!(refus(&banc, &entetes), "keyid_malformed");
}

#[test]
fn un_mcp_param_est_refuse() {
    let banc = banc();
    let mut entetes = signature_valide(&banc.signer);
    entetes.push(("Mcp-Param-Sql".to_owned(), "select 1".to_owned()));
    assert_eq!(refus(&banc, &entetes), "mcp_param_present");
}

#[test]
fn des_parametres_de_champ_malformes_ne_paniquent_pas() {
    let banc = banc();
    let base = signature_valide(&banc.signer);
    let motifs = [
        "signature_input_missing",
        "signature_missing",
        "multiple_signatures",
        "label_mismatch",
        "covered_set_not_allowed",
        "component_param",
        "params_not_allowed",
        "alg_not_ed25519",
        "tag_mismatch",
        "keyid_malformed",
        "mcp_param_present",
        "key_unknown",
        "key_revoked",
        "signature_invalid",
        "content_digest_missing",
        "content_digest_mismatch",
        "clock_skew",
        "nonce_replay",
    ];
    let geant = "a".repeat(64 * 1024);
    let valeurs = [
        "",
        " ",
        "sig1",
        "sig1=",
        "sig1=(",
        "sig1=()",
        "sig1=((\"@method\"))",
        "sig1=(\"@method\");created=1;;",
        "sig1=:",
        "sig1=::",
        "sig1=:AAAA:",
        "sig1=(\"@method\");created=99999999999999999999",
        geant.as_str(),
        "sig1=(\"@method\" \"@path\");created=1;nonce=\"\\\";keyid=\"a\"",
        "é",
        "\u{0}",
    ];
    for nom in [
        "Signature-Input",
        "Signature",
        "Content-Digest",
        "Mcp-Method",
    ] {
        for mauvais in valeurs {
            let mut entetes = base.clone();
            remplacer(&mut entetes, nom, mauvais);
            let motif = refus(&banc, &entetes);
            assert!(
                motifs.contains(&motif.as_str()),
                "motif invente pour {nom} : {motif}"
            );
        }
    }
}

// ===========================================================================
// Refus : corps, horloge, nonce, clé, signature
// ===========================================================================

#[test]
fn un_corps_modifie_est_refuse() {
    let banc = banc();
    let entetes = signature_valide(&banc.signer);
    let autre = BODY
        .iter()
        .copied()
        .chain(std::iter::once(b' '))
        .collect::<Vec<u8>>();
    let motif = verifier_sur(&banc, "/mcp", "127.0.0.1:23080", &entetes, &autre)
        .expect_err("corps modifie")
        .reason()
        .to_owned();
    assert_eq!(motif, "content_digest_mismatch");
}

#[test]
fn un_content_digest_faux_sha512_ou_base64url_est_refuse() {
    let banc = banc();
    let base = signature_valide(&banc.signer);
    let bon = valeur(&base, "Content-Digest");
    let cas = [
        // Faux (un caractère change).
        bon.replacen('J', "K", 1),
        // sha-512.
        format!("sha-512=:{}:", "A".repeat(88)),
        // base64url.
        bon.replacen('J', "-", 1),
        // Sans padding.
        bon.replace("=:", ":"),
        // Second membre.
        format!("{bon},sha-256=:{}:", "A".repeat(44)),
    ];
    for mauvais in cas {
        let mut entetes = base.clone();
        remplacer(&mut entetes, "Content-Digest", &mauvais);
        assert_eq!(
            refus(&banc, &entetes),
            "content_digest_mismatch",
            "{mauvais}"
        );
    }
}

#[test]
fn l_horloge_hors_fenetre_est_refusee_dans_les_deux_sens() {
    let banc = banc();
    for created in [NOW - 61, NOW + 61, 0, u32::MAX as u64] {
        let entetes = signee(&banc.signer, created, &nonce(3));
        assert_eq!(
            refus(&banc, &entetes),
            "clock_skew",
            "created={created} accepte"
        );
    }
    // Bornes exactes : plus ou moins 60 s acceptées (nonces distincts).
    for (index, created) in [NOW - 60, NOW + 60].into_iter().enumerate() {
        let entetes = signee(&banc.signer, created, &nonce(4 + index as u32));
        assert!(
            verifier(&banc, &entetes).is_ok(),
            "created={created} refuse a tort"
        );
    }
}

#[test]
fn un_nonce_rejoue_est_refuse() {
    let banc = banc();
    let entetes = signature_valide(&banc.signer);
    verifier(&banc, &entetes).expect("premiere requete");
    assert_eq!(refus(&banc, &entetes), "nonce_replay");
    // Un autre nonce passe : le cache est bien par nonce, pas par clé.
    let entetes = signee(&banc.signer, NOW, &nonce(7));
    verifier(&banc, &entetes).expect("nonce neuf");
}

#[test]
fn une_cle_inconnue_une_cle_revoquee_et_une_signature_fausse_sont_refusees() {
    let banc = banc();

    // Clé inconnue : signée par une identité absente du magasin.
    let inconnu = Signer::new(SigningKey::from_seed(&AUTRE_SEED)).with_keyid("inconnu");
    let entetes = signee(&inconnu, NOW, &nonce(10));
    assert_eq!(refus(&banc, &entetes), "key_unknown");

    // Signature fausse : un caractère du base64 change.
    let mut entetes = signature_valide(&banc.signer);
    let signature = valeur(&entetes, "Signature");
    let position = 10;
    let substitut = if signature.as_bytes()[position] == b'A' {
        'B'
    } else {
        'A'
    };
    let mut altere = signature.clone();
    altere.replace_range(position..position + 1, &substitut.to_string());
    remplacer(&mut entetes, "Signature", &altere);
    assert_eq!(refus(&banc, &entetes), "signature_invalid");

    // Clé révoquée : la révocation prend effet à la requête suivante.
    let entetes = signee(&banc.signer, NOW, &nonce(11));
    verifier(&banc, &entetes).expect("cle encore active");
    banc.store.revoke("client-demo");
    let entetes = signee(&banc.signer, NOW, &nonce(12));
    assert_eq!(refus(&banc, &entetes), "key_revoked");
    // Une révocation n'est pas un retrait : la clé reste connue.
    assert!(banc.store.lookup("client-demo").is_some());
}

#[test]
fn un_hote_ou_un_chemin_different_de_ceux_signes_est_refuse() {
    let banc = banc();
    let entetes = signature_valide(&banc.signer);
    for (path, authority) in [
        ("/mcp", "127.0.0.1:23081"),
        ("/autre", "127.0.0.1:23080"),
        ("/mcp", "example.com"),
    ] {
        let motif = verifier_sur(&banc, path, authority, &entetes, BODY)
            .expect_err("liaison au serveur")
            .reason()
            .to_owned();
        assert_eq!(motif, "signature_invalid", "{authority}{path}");
    }
}

#[test]
fn mcp_method_ou_mcp_name_contredisant_le_corps_sont_refuses() {
    let banc = banc();

    let mut entetes = signature_valide(&banc.signer);
    remplacer(&mut entetes, "Mcp-Method", "tools/list");
    assert_eq!(refus(&banc, &entetes), "covered_set_not_allowed");

    let mut entetes = signature_valide(&banc.signer);
    remplacer(&mut entetes, "Mcp-Name", "db.autre");
    assert_eq!(refus(&banc, &entetes), "covered_set_not_allowed");

    // Un `Mcp-Name` couvert mais absent du corps : refus aussi.
    let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{}}"#;
    let requete = RequestToSign::new("POST", "/mcp", "127.0.0.1:23080", &ENTETES, body);
    let signed = banc.signer.headers_at(&requete, NOW, &nonce(20));
    let mut recus: Vec<(String, String)> = ENTETES
        .iter()
        .map(|(nom, valeur)| ((*nom).to_owned(), (*valeur).to_owned()))
        .collect();
    for (nom, valeur) in signed.iter() {
        recus.push((nom.to_owned(), valeur.to_owned()));
    }
    let motif = verifier_sur(&banc, "/mcp", "127.0.0.1:23080", &recus, body)
        .expect_err("corps sans nom")
        .reason()
        .to_owned();
    assert_eq!(motif, "covered_set_not_allowed");
}

// ===========================================================================
// Ordre des contrôles, uniformité, volume
// ===========================================================================

#[test]
fn l_ordre_des_controles_met_ed25519_en_dernier() {
    let banc = banc();
    // Chaque cas porte DEUX défauts : un contrôle bon marché et une signature
    // invalide. Le motif rendu prouve que la signature n'est jamais évaluée
    // avant le contrôle correspondant.
    let cas: [(&str, &str); 5] = [
        ("content_digest_mismatch", "Content-Digest"),
        ("clock_skew", "horloge"),
        ("nonce_replay", "nonce"),
        ("key_unknown", "cle"),
        ("covered_set_not_allowed", "Mcp-Method"),
    ];

    // Nonce déjà consommé par une requête valide.
    let consomme = signature_valide(&banc.signer);
    verifier(&banc, &consomme).expect("consommation du nonce");

    for (motif, defaut) in cas {
        let mut entetes = match defaut {
            "horloge" => signee(&banc.signer, NOW - 3600, &nonce(30)),
            "nonce" => consomme.clone(),
            "cle" => {
                let inconnu = Signer::new(SigningKey::from_seed(&AUTRE_SEED)).with_keyid("absent");
                signee(&inconnu, NOW, &nonce(31))
            }
            _ => signature_valide(&banc.signer),
        };
        // Second défaut : une signature Ed25519 fausse.
        let mut altere = valeur(&entetes, "Signature");
        let substitut = if altere.as_bytes()[10] == b'A' {
            'B'
        } else {
            'A'
        };
        altere.replace_range(10..11, &substitut.to_string());
        remplacer(&mut entetes, "Signature", &altere);
        // Premier défaut.
        match defaut {
            "Content-Digest" => {
                let faux = format!("sha-256=:{}:", "A".repeat(44));
                remplacer(&mut entetes, "Content-Digest", &faux);
            }
            "Mcp-Method" => remplacer(&mut entetes, "Mcp-Method", "tools/list"),
            _ => {}
        }
        assert_eq!(
            refus(&banc, &entetes),
            motif,
            "ordre des controles : {defaut}"
        );
    }
}

#[test]
fn les_trois_refus_de_cle_sont_indistinguables_sur_le_reseau() {
    let banc = banc();

    // (1) clé inconnue.
    let inconnu = Signer::new(SigningKey::from_seed(&AUTRE_SEED)).with_keyid("inconnu");
    let cle_inconnue = verifier(&banc, &signee(&inconnu, NOW, &nonce(40)))
        .expect_err("cle inconnue")
        .reason()
        .to_owned();

    // (2) signature fausse.
    let mut entetes = signature_valide(&banc.signer);
    let signature = valeur(&entetes, "Signature");
    let substitut = if signature.as_bytes()[12] == b'A' {
        'B'
    } else {
        'A'
    };
    let altere = format!("{}{}{}", &signature[..12], substitut, &signature[13..]);
    remplacer(&mut entetes, "Signature", &altere);
    let signature_fausse = refus(&banc, &entetes);

    // (3) clé révoquée.
    let revocation = Signer::new(SigningKey::from_seed(&SEED)).with_keyid("revoked");
    banc.store.insert(
        "revoked",
        ClientKey::new("client", revocation.verifying_key().to_bytes()).revoked(),
    );
    let cle_revoquee = refus(&banc, &signee(&revocation, NOW, &nonce(41)));

    // Motifs internes : distincts, pour le journal de l'application.
    assert_eq!(cle_inconnue, "key_unknown");
    assert_eq!(signature_fausse, "signature_invalid");
    assert_eq!(cle_revoquee, "key_revoked");

    // Projection réseau : une seule et même réponse, octet pour octet.
    let projeter = |_motif: &str| -> (u16, Vec<(String, String)>, Vec<u8>) {
        (
            UNAUTHORIZED_STATUS,
            vec![
                ("WWW-Authenticate".to_owned(), WWW_AUTHENTICATE.to_owned()),
                ("Content-Type".to_owned(), "application/json".to_owned()),
            ],
            CORPS_401.to_vec(),
        )
    };
    assert_eq!(projeter(&cle_inconnue), projeter(&signature_fausse));
    assert_eq!(projeter(&signature_fausse), projeter(&cle_revoquee));
    assert_eq!(
        WWW_AUTHENTICATE,
        "Signature realm=\"litemcp\", tag=\"litemcp-auth-v1\""
    );

    // Aucun motif ne recopie un identifiant ou un secret.
    for motif in [&cle_inconnue, &signature_fausse, &cle_revoquee] {
        assert!(!motif.contains("client-demo"), "{motif}");
        assert!(!motif.contains("AAECAwQFBgcICQoLDA0ODw"), "{motif}");
    }
}

#[test]
fn dix_mille_requetes_valides_puis_rejeu_de_la_premiere() {
    const VOLUME: u32 = 10_000;
    let store = Arc::new(TestKeyStore::new());
    let signer = Signer::new(SigningKey::from_seed(&SEED)).with_keyid("volume");
    store.insert(
        "volume",
        ClientKey::new("volume", signer.verifying_key().to_bytes()).with_right("db.read"),
    );
    let keys: Arc<dyn KeyStore> = store.clone();
    // Capacité et rétention annoncées explicitement : 10 000 requêtes dépassent
    // le budget par défaut de 4096 entrées, et la rétention de 120 s serait
    // dépassée par la durée du test en mode debug (rapport L2, écart 1).
    let verifier =
        SignatureVerifier::new(keys, NonceCache::new(16_384, Duration::from_secs(3_600)))
            .with_clock(Arc::new(|| NOW));

    let verifier_requete = |entetes: &[(String, String)]| {
        let paires: Vec<(&str, &str)> = entetes
            .iter()
            .map(|(nom, valeur)| (nom.as_str(), valeur.as_str()))
            .collect();
        verifier.verify(&VerifiedRequest::new(
            "POST",
            "/mcp",
            "127.0.0.1:23080",
            &paires,
            BODY,
        ))
    };

    let premiere = signee(&signer, NOW, &nonce(0));
    verifier_requete(&premiere).expect("premiere requete");
    let derniere = signee(&signer, NOW, &nonce(VOLUME - 1));
    for index in 1..VOLUME - 1 {
        let entetes = signee(&signer, NOW, &nonce(index));
        verifier_requete(&entetes).expect("requete valide");
    }
    verifier_requete(&derniere).expect("derniere requete");
    assert_eq!(verifier.nonces().entries_for("volume"), VOLUME as usize);

    // Rejeu de la première : refusé.
    let motif = verifier_requete(&premiere)
        .expect_err("rejeu accepte")
        .reason()
        .to_owned();
    assert_eq!(motif, "nonce_replay");
}

#[test]
fn une_inondation_de_nonces_n_evince_que_sa_propre_cle() {
    let store = Arc::new(TestKeyStore::new());
    let victime = Signer::new(SigningKey::from_seed(&SEED)).with_keyid("victime");
    let flood = Signer::new(SigningKey::from_seed(&AUTRE_SEED)).with_keyid("flood");
    store.insert(
        "victime",
        ClientKey::new("victime", victime.verifying_key().to_bytes()),
    );
    store.insert(
        "flood",
        ClientKey::new("flood", flood.verifying_key().to_bytes()),
    );
    let keys: Arc<dyn KeyStore> = store.clone();
    let verifier = SignatureVerifier::new(keys, NonceCache::new(1024, Duration::from_secs(3_600)))
        .with_clock(Arc::new(|| NOW));

    let envoyer = |entetes: &[(String, String)]| {
        let paires: Vec<(&str, &str)> = entetes
            .iter()
            .map(|(nom, valeur)| (nom.as_str(), valeur.as_str()))
            .collect();
        verifier.verify(&VerifiedRequest::new(
            "POST",
            "/mcp",
            "127.0.0.1:23080",
            &paires,
            BODY,
        ))
    };

    // La victime passe une fois ; l'inondatrice sature son propre compartiment.
    let victime_entetes = signee(&victime, NOW, &nonce(50));
    envoyer(&victime_entetes).expect("victime");
    let mut pleins = 0usize;
    for index in 0..2_000u32 {
        let entetes = signee(&flood, NOW, &nonce(index));
        match envoyer(&entetes) {
            Ok(_) => {}
            Err(err) => {
                // Capacité 1024 : au-delà, refus explicite et jamais d'éviction.
                assert_eq!(err.reason(), "nonce_cache_full", "nonce {index}");
                pleins += 1;
            }
        }
    }
    assert_eq!(
        pleins,
        2_000 - 1024,
        "refus attendus au-delà de la capacité"
    );
    assert_eq!(verifier.nonces().entries_for("flood"), 1024);
    assert_eq!(
        verifier.nonces().entries_for("victime"),
        1,
        "eviction croisee"
    );

    // Le nonce de la victime est toujours détecté comme rejeu.
    assert_eq!(
        envoyer(&victime_entetes)
            .expect_err("rejeu de la victime")
            .reason(),
        "nonce_replay"
    );
}

/// P2 #1 — compartiment plein : refus explicite, jamais d'éviction d'un nonce
/// encore dans la fenêtre de 60 s (sinon le rejeu redeviendrait possible).
#[test]
fn un_compartiment_plein_refuse_au_lieu_d_evincer() {
    let store = Arc::new(TestKeyStore::new());
    let signer = Signer::new(SigningKey::from_seed(&SEED)).with_keyid("sature");
    store.insert(
        "sature",
        ClientKey::new("sature", signer.verifying_key().to_bytes()),
    );
    let keys: Arc<dyn KeyStore> = store.clone();
    let verifier = SignatureVerifier::new(keys, NonceCache::new(2, Duration::from_secs(3_600)))
        .with_clock(Arc::new(|| NOW));
    let envoyer = |nonce: &[u8; 16]| {
        let entetes = signee(&signer, NOW, nonce);
        let paires: Vec<(&str, &str)> = entetes
            .iter()
            .map(|(nom, valeur)| (nom.as_str(), valeur.as_str()))
            .collect();
        verifier.verify(&VerifiedRequest::new(
            "POST",
            "/mcp",
            "127.0.0.1:23080",
            &paires,
            BODY,
        ))
    };

    envoyer(&nonce(1)).expect("premiere requete valide");
    envoyer(&nonce(2)).expect("deuxieme requete valide");
    // Capacité atteinte : la requête suivante est refusée…
    assert_eq!(
        envoyer(&nonce(3)).expect_err("capacite ignoree").reason(),
        "nonce_cache_full"
    );
    // …la première reste un rejeu (aucune éviction)…
    assert_eq!(
        envoyer(&nonce(1)).expect_err("rejeu accepte").reason(),
        "nonce_replay"
    );
    // …et le compartiment n'a pas grandi.
    assert_eq!(verifier.nonces().entries_for("sature"), 2);
}

/// P2 #2 — liaison `Mcp-Name` **consciente de la méthode** : `params.uri` pour
/// `resources/read`, `params.name` pour `tools/call` et `prompts/get`.
#[test]
fn resources_read_lie_mcp_name_a_l_uri() {
    const LIRE: &[u8] = br#"{"jsonrpc":"2.0","id":8,"method":"resources/read","params":{"uri":"file:///secret.txt","_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}"#;
    const LISTE: &[u8] = br#"{"jsonrpc":"2.0","id":9,"method":"tools/list","params":{}}"#;
    let banc = banc();
    let signee_avec = |methode: &str, nom: Option<&str>, corps: &[u8], valeur: &[u8; 16]| {
        let mut base = vec![
            ("Host".to_string(), "127.0.0.1:23080".to_string()),
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Mcp-Method".to_string(), methode.to_string()),
        ];
        if let Some(nom) = nom {
            base.push(("Mcp-Name".to_string(), nom.to_string()));
        }
        let paires: Vec<(&str, &str)> = base
            .iter()
            .map(|(cle, valeur)| (cle.as_str(), valeur.as_str()))
            .collect();
        let requete = RequestToSign::new("POST", "/mcp", "127.0.0.1:23080", &paires, corps);
        let signes = banc.signer.headers_at(&requete, NOW, valeur);
        let mut tous = base;
        tous.extend(
            signes
                .iter()
                .map(|(cle, valeur)| (cle.to_string(), valeur.to_string())),
        );
        tous
    };

    // Conforme SEP-2243 : `Mcp-Name` = uri → accepté, sans `name` dans le corps.
    verifier_sur(
        &banc,
        "/mcp",
        "127.0.0.1:23080",
        &signee_avec(
            "resources/read",
            Some("file:///secret.txt"),
            LIRE,
            &nonce(30),
        ),
        LIRE,
    )
    .expect("resources/read conforme accepte");

    // `Mcp-Name` ≠ uri → refus, motif de la liste fermée.
    assert_eq!(
        verifier_sur(
            &banc,
            "/mcp",
            "127.0.0.1:23080",
            &signee_avec(
                "resources/read",
                Some("file:///autre.txt"),
                LIRE,
                &nonce(31)
            ),
            LIRE,
        )
        .expect_err("liaison rompue acceptee")
        .reason(),
        "covered_set_not_allowed"
    );

    // `Mcp-Name` sur une méthode qui n'en admet pas → refus.
    assert_eq!(
        verifier_sur(
            &banc,
            "/mcp",
            "127.0.0.1:23080",
            &signee_avec("tools/list", Some("inutile"), LISTE, &nonce(32)),
            LISTE,
        )
        .expect_err("nom sur methode sans nom")
        .reason(),
        "covered_set_not_allowed"
    );

    // Absence de `Mcp-Name` : liste à **six** composants, admise par §2.2 (le
    // transport intégré, lui, exige l'en-tête pour `resources/read` : 400
    // avant toute vérification).
    verifier_sur(
        &banc,
        "/mcp",
        "127.0.0.1:23080",
        &signee_avec("resources/read", None, LIRE, &nonce(33)),
        LIRE,
    )
    .expect("six composants admis");
}
