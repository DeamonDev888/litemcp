#![cfg(feature = "auth-sign")]
//! L2 — robustesse du parseur d'en-têtes (feature `auth-sign`) : aucune entrée
//! venue du réseau ne doit paniquer, tout refus doit porter un motif de la
//! liste fermée de `docs/auth.md` §2.7.
//!
//! Les stratégies couvrent : valeurs arbitraires (Unicode, contrôles),
//! troncatures, en-têtes géants (au moins 64 Kio), champs structurés imbriqués
//! et malformés. Chaque cas part d'une requête **valide** et n'altère qu'un
//! champ.

use litemcp::auth::sign::{
    ClientKey, KeyStore, NonceCache, RequestToSign, SignatureVerifier, Signer, SigningKey,
    TestKeyStore,
};
use litemcp::auth::{AuthContext, AuthError, VerifiedRequest, Verifier};
use proptest::prelude::*;
use std::sync::Arc;

const SEED: [u8; 32] = [
    0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c, 0xc4,
    0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae, 0x7f, 0x60,
];
const NOW: u64 = 1_794_700_800;
const BODY: &[u8] =
    br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"db.query"}}"#;
const ENTETES: [(&str, &str); 4] = [
    ("Host", "127.0.0.1:23080"),
    ("Content-Type", "application/json"),
    ("Mcp-Method", "tools/call"),
    ("Mcp-Name", "db.query"),
];

/// Liste fermée de `docs/auth.md` §2.7 (motifs de vérification).
const MOTIFS: [&str; 19] = [
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
    "verifier_panicked",
];

struct Banc {
    verifier: SignatureVerifier,
    entetes: Vec<(String, String)>,
}

fn banc() -> Banc {
    let store = Arc::new(TestKeyStore::new());
    let signer = Signer::new(SigningKey::from_seed(&SEED)).with_keyid("client-demo");
    store.insert(
        "client-demo",
        ClientKey::new("client-demo", signer.verifying_key().to_bytes()),
    );
    let keys: Arc<dyn KeyStore> = store;
    let verifier = SignatureVerifier::new(keys, NonceCache::with_default_policy())
        .with_clock(Arc::new(|| NOW));
    let requete = RequestToSign::new("POST", "/mcp", "127.0.0.1:23080", &ENTETES, BODY);
    let signed = signer.headers_at(&requete, NOW, &[7u8; 16]);
    let mut entetes: Vec<(String, String)> = ENTETES
        .iter()
        .map(|(nom, valeur)| ((*nom).to_owned(), (*valeur).to_owned()))
        .collect();
    for (nom, valeur) in signed.iter() {
        entetes.push((nom.to_owned(), valeur.to_owned()));
    }
    Banc { verifier, entetes }
}

/// Vérifie avec un en-tête `nom` remplacé par `valeur`.
fn refus_avec(banc: &Banc, nom: &str, valeur: &str) -> Result<AuthContext, AuthError> {
    let mut entetes = banc.entetes.clone();
    for (cle, contenu) in entetes.iter_mut() {
        if cle.eq_ignore_ascii_case(nom) {
            *contenu = valeur.to_owned();
        }
    }
    let paires: Vec<(&str, &str)> = entetes
        .iter()
        .map(|(cle, contenu)| (cle.as_str(), contenu.as_str()))
        .collect();
    banc.verifier.verify(&VerifiedRequest::new(
        "POST",
        "/mcp",
        "127.0.0.1:23080",
        &paires,
        BODY,
    ))
}

/// Un refus doit porter un motif de la liste fermée ; `Ok` n'est admis que
/// pour la valeur d'origine (mutation sans effet).
fn motif_attendu(banc: &Banc, nom: &str, mauvais: &str, origine: &str) {
    if mauvais == origine {
        return;
    }
    match refus_avec(banc, nom, mauvais) {
        Ok(contexte) => panic!(
            "{nom} accepte avec une valeur alteree (client {})",
            contexte.client()
        ),
        Err(erreur) => assert!(
            MOTIFS.contains(&erreur.reason()),
            "motif hors liste pour {nom} : {}",
            erreur.reason()
        ),
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 192, ..ProptestConfig::default() })]

    /// `Signature-Input` arbitraire : jamais de panique, toujours un refus.
    #[test]
    fn signature_input_arbitraire(mauvais in ".{0,256}") {
        let banc = banc();
        let valeur = format!("sig1={mauvais}");
        motif_attendu(&banc, "Signature-Input", &valeur, "");
    }

    /// `Signature` arbitraire.
    #[test]
    fn signature_arbitraire(mauvais in ".{0,256}") {
        let banc = banc();
        let valeur = format!("sig1={mauvais}");
        motif_attendu(&banc, "Signature", &valeur, "");
    }

    /// `Content-Digest` arbitraire.
    #[test]
    fn content_digest_arbitraire(mauvais in ".{0,256}") {
        let banc = banc();
        motif_attendu(&banc, "Content-Digest", &mauvais, "");
    }

    /// `Mcp-Method` et `Mcp-Name` arbitraires : la liaison au corps tranche.
    #[test]
    fn mcp_method_ou_name_arbitraire(mauvais in ".{0,128}") {
        let banc = banc();
        motif_attendu(&banc, "Mcp-Method", &mauvais, "tools/call");
        motif_attendu(&banc, "Mcp-Name", &mauvais, "db.query");
    }

    /// Octets bruts, y compris non UTF-8.
    #[test]
    fn octets_bruts_dans_les_champs_structures(octets in proptest::collection::vec(any::<u8>(), 0..512)) {
        let banc = banc();
        let mauvais = String::from_utf8_lossy(&octets).into_owned();
        for nom in ["Signature-Input", "Signature", "Content-Digest"] {
            motif_attendu(&banc, nom, &mauvais, "");
        }
    }

    /// Imbrication et ponctuation structurée.
    #[test]
    fn champs_structures_imbriques(mauvais in r#"[()";,:=\\@ a-z0-9.-]{0,200}"#) {
        let banc = banc();
        for nom in ["Signature-Input", "Signature", "Content-Digest"] {
            motif_attendu(&banc, nom, &mauvais, "");
        }
    }
}

#[test]
fn une_troncature_de_la_signature_input_est_toujours_refusee() {
    let banc = banc();
    let complete = banc
        .entetes
        .iter()
        .find(|(nom, _)| nom == "Signature-Input")
        .map(|(_, valeur)| valeur.clone())
        .expect("en-tete present");
    for coupe in 0..complete.len() {
        if let Ok(contexte) = refus_avec(&banc, "Signature-Input", &complete[..coupe]) {
            panic!("troncature acceptee a {coupe} : {}", contexte.client());
        }
    }
    let signature = banc
        .entetes
        .iter()
        .find(|(nom, _)| nom == "Signature")
        .map(|(_, valeur)| valeur.clone())
        .expect("en-tete present");
    for coupe in 0..signature.len() {
        assert!(refus_avec(&banc, "Signature", &signature[..coupe]).is_err());
    }
}

#[test]
fn un_en_tete_geant_est_refuse_en_temps_borne() {
    let banc = banc();
    for taille in [64 * 1024, 1024 * 1024] {
        let geant = format!("sig1=(\"@method\" {})", "\"a\" ".repeat(taille / 4));
        assert!(geant.len() >= taille);
        for nom in ["Signature-Input", "Signature", "Content-Digest", "Mcp-Name"] {
            let resultat = refus_avec(&banc, nom, &geant);
            assert!(resultat.is_err(), "{nom} : {taille} octets acceptes");
            if let Err(erreur) = resultat {
                assert!(
                    MOTIFS.contains(&erreur.reason()),
                    "motif hors liste : {}",
                    erreur.reason()
                );
            }
        }
    }
    let plat = "a".repeat(1024 * 1024);
    assert!(refus_avec(&banc, "Signature-Input", &plat).is_err());
    assert!(refus_avec(&banc, "Signature", &plat).is_err());
}

#[test]
fn les_caracteres_de_controle_sont_refuses() {
    let banc = banc();
    for mauvais in [
        "sig1=(\"@method\")\r\nX-Injecte: 1",
        "sig1=(\"@method\")\u{0}",
        "sig1=(\"@method\")\t",
        "sig1=(\"@method\")\u{7f}",
        "sig1=(\"@method\")\u{1f600}",
    ] {
        assert!(
            refus_avec(&banc, "Signature-Input", mauvais).is_err(),
            "{mauvais:?} accepte"
        );
    }
}

#[test]
fn la_requete_temoin_est_valide() {
    // Sans ce témoin, un harnais cassé rendrait tous les refus « normaux ».
    let banc = banc();
    let contexte = refus_avec(&banc, "Mcp-Name", "db.query").expect("temoin accepte");
    assert_eq!(contexte.client(), "client-demo");
}
