//! Exemple `auth-sign` — signature et vérification de requêtes MCP (profil
//! RFC 9421 + RFC 9530 de `docs/auth.md`).
//!
//! ```text
//! cargo run --release --features auth-sign --example auth_sign
//! ```
//!
//! Le programme :
//!
//! 1. vérifie le **vecteur de référence** de `docs/auth.md` §2.5 (clé publique
//!    de la RFC 8032, aucune clé privée réelle) ;
//! 2. génère une identité avec l'aléa du système, signe une requête et la fait
//!    vérifier par un `SignatureVerifier` adossé à un `KeyStore` applicatif ;
//! 3. montre cinq refus (motif **côté application**, jamais côté réseau) ;
//! 4. mesure le coût d'une vérification sur 10 000 requêtes.
//!
//! Aucune clé privée n'est affichée : seules l'empreinte publique et le bloc
//! public sortent sur la console.

#[cfg(feature = "auth-sign")]
fn main() {
    use litemcp::auth::sign::{
        ClientKey, Fingerprint, KeyStore, NonceCache, RequestToSign, SignatureVerifier, Signer,
        SigningKey, TestKeyStore, WWW_AUTHENTICATE,
    };
    use litemcp::auth::{AuthContext, AuthError, VerifiedRequest, Verifier};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// Requête MCP du vecteur : corps de 171 octets, `tools/call`.
    const BODY: &[u8] = br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"db.query","arguments":{"sql":"select 1"},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}"#;
    const ENTETES: [(&str, &str); 4] = [
        ("Host", "127.0.0.1:23080"),
        ("Content-Type", "application/json"),
        ("Mcp-Method", "tools/call"),
        ("Mcp-Name", "db.query"),
    ];

    println!("== 1. vecteur de référence (docs/auth.md §2.5) ==");
    let graine: [u8; 32] = [
        0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c,
        0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae,
        0x7f, 0x60,
    ];
    let nonce_vecteur = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f,
    ];
    let vecteur = Signer::new(SigningKey::from_seed(&graine)).with_keyid("client-demo");
    let requete = RequestToSign::new("POST", "/mcp", "127.0.0.1:23080", &ENTETES, BODY);
    let signed = vecteur.headers_at(&requete, 1_794_700_800, &nonce_vecteur);
    let attendue = "sig1=:kOnwfzENaxBF6ubQiJGlXkRfUYUJuELYzHnvqwrzpScvO3JchjFOsREa4j7CsRrdiEZifLmZZZEBBFPJqpsJDA==:";
    println!("  Content-Digest : {}", signed.content_digest());
    println!(
        "  Signature      : {}",
        if signed.signature() == attendue {
            "identique au vecteur"
        } else {
            "DIVERGENTE"
        }
    );

    println!("== 2. identité générée par le système ==");
    let signer = Signer::new(SigningKey::generate()).with_keyid("poste-demo");
    let empreinte: Fingerprint = signer.fingerprint();
    println!("  empreinte      : {}", empreinte.full());
    println!("  forme courte   : {}", empreinte.short());
    println!("  clé publique   :\n{}", signer.verifying_key().export());

    let store = Arc::new(TestKeyStore::new());
    store.insert(
        "poste-demo",
        ClientKey::new("poste-demo", signer.verifying_key().to_bytes())
            .with_label("Poste de démonstration")
            .with_rights(["db.read", "db.write"]),
    );
    // Un `KeyStore` applicatif réel vit dans l'application ; ici le magasin de
    // test du crate suffit à montrer le contrat.
    let keys: Arc<dyn KeyStore> = store.clone();
    let verifier = SignatureVerifier::new(keys, NonceCache::with_default_policy());

    let maintenant = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duree| duree.as_secs())
        .unwrap_or(0);
    let signed = signer.headers(&requete);
    let mut recus: Vec<(String, String)> = ENTETES
        .iter()
        .map(|(nom, valeur)| ((*nom).to_owned(), (*valeur).to_owned()))
        .collect();
    for (nom, valeur) in signed.iter() {
        recus.push((nom.to_owned(), valeur.to_owned()));
        println!("  {nom}: {valeur}");
    }
    let paires: Vec<(&str, &str)> = recus
        .iter()
        .map(|(nom, valeur)| (nom.as_str(), valeur.as_str()))
        .collect();
    let contexte = verifier
        .verify(&VerifiedRequest::new(
            "POST",
            "/mcp",
            "127.0.0.1:23080",
            &paires,
            BODY,
        ))
        .expect("requête valide");
    println!(
        "  identité       : {} (droits {:?}, créée il y a {} s)",
        contexte.client(),
        contexte.rights(),
        maintenant.saturating_sub(
            contexte
                .verified_at()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duree| duree.as_secs())
                .unwrap_or(maintenant)
        )
    );

    /// En-têtes d'une requête, possédés.
    type Entetes = Vec<(String, String)>;
    /// Mutation appliquée à une requête fraîchement signée.
    type Mutateur<'a> = &'a dyn Fn(&mut Entetes);

    println!("== 3. refus (motif côté application) ==");
    // Chaque cas part d'une requête fraîche (nonce neuf) : sans cela, le rejeu
    // du nonce masquerait les autres motifs, et c'est justement l'ordre des
    // contrôles que l'on veut montrer.
    let mut compteur = 1000u64;
    let mut nonce_neuf = || {
        let mut nonce = [0u8; 16];
        nonce[..8].copy_from_slice(&compteur.to_le_bytes());
        compteur += 1;
        nonce
    };
    let mut montrer = |titre: &str,
                       path: &str,
                       authority: &str,
                       body: &[u8],
                       creer: u64,
                       mutateur: Mutateur<'_>| {
        let nonce = nonce_neuf();
        let signee_ici = signer.headers_at(&requete, creer, &nonce);
        let mut entetes: Entetes = ENTETES
            .iter()
            .map(|(nom, valeur)| ((*nom).to_owned(), (*valeur).to_owned()))
            .collect();
        for (nom, valeur) in signee_ici.iter() {
            entetes.push((nom.to_owned(), valeur.to_owned()));
        }
        mutateur(&mut entetes);
        let paires: Vec<(&str, &str)> = entetes
            .iter()
            .map(|(nom, valeur)| (nom.as_str(), valeur.as_str()))
            .collect();
        let refus: Result<AuthContext, AuthError> = verifier.verify(&VerifiedRequest::new(
            "POST", path, authority, &paires, body,
        ));
        match refus {
            Ok(_) => println!("  {titre:<22} accepté (inattendu)"),
            Err(erreur) => println!(
                "  {titre:<22} 401 «{WWW_AUTHENTICATE}» motif : {}",
                erreur.reason()
            ),
        }
    };
    let aucun = |_: &mut Entetes| {};
    montrer(
        "corps modifié",
        "/mcp",
        "127.0.0.1:23080",
        br#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"db.query"}}"#,
        maintenant,
        &aucun,
    );
    montrer(
        "signature altérée",
        "/mcp",
        "127.0.0.1:23080",
        BODY,
        maintenant,
        &|entetes| {
            for (nom, valeur) in entetes.iter_mut() {
                if nom == "Signature" {
                    let substitut = if valeur.as_bytes()[10] == b'A' {
                        'B'
                    } else {
                        'A'
                    };
                    valeur.replace_range(10..11, &substitut.to_string());
                }
            }
        },
    );
    montrer(
        "autre hôte",
        "/mcp",
        "autre-hote.example",
        BODY,
        maintenant,
        &aucun,
    );
    montrer(
        "hors fenêtre d'horloge",
        "/mcp",
        "127.0.0.1:23080",
        BODY,
        maintenant.saturating_sub(3_600),
        &aucun,
    );
    montrer(
        "Mcp-Param-*",
        "/mcp",
        "127.0.0.1:23080",
        BODY,
        maintenant,
        &|entetes| entetes.push(("Mcp-Param-Sql".to_owned(), "select 1".to_owned())),
    );
    // Rejeu : la requête de la section 2 vient d'être acceptée.
    let paires: Vec<(&str, &str)> = recus
        .iter()
        .map(|(nom, valeur)| (nom.as_str(), valeur.as_str()))
        .collect();
    let refus = verifier.verify(&VerifiedRequest::new(
        "POST",
        "/mcp",
        "127.0.0.1:23080",
        &paires,
        BODY,
    ));
    println!(
        "  {:<22} motif : {}",
        "rejeu du nonce",
        refus
            .map(|_| "accepté".to_string())
            .unwrap_or_else(|erreur| erreur.reason().to_string())
    );

    println!("== 4. coût d'une vérification ==");
    let iterations: usize = std::env::var("LITEMCP_AUTH_ITERATIONS")
        .ok()
        .and_then(|texte| texte.parse().ok())
        .unwrap_or(10_000);
    // Cache assez large pour que la mesure ne porte pas sur l'éviction.
    let keys: Arc<dyn KeyStore> = store.clone();
    let verifier = SignatureVerifier::new(
        keys,
        NonceCache::new(iterations * 2, Duration::from_secs(3_600)),
    );
    let mut requetes: Vec<Vec<(String, String)>> = Vec::with_capacity(iterations);
    for index in 0..iterations {
        let mut nonce = [0u8; 16];
        nonce[..8].copy_from_slice(&(index as u64).to_le_bytes());
        let signed = signer.headers_at(&requete, maintenant, &nonce);
        let mut entetes: Vec<(String, String)> = ENTETES
            .iter()
            .map(|(nom, valeur)| ((*nom).to_owned(), (*valeur).to_owned()))
            .collect();
        for (nom, valeur) in signed.iter() {
            entetes.push((nom.to_owned(), valeur.to_owned()));
        }
        requetes.push(entetes);
    }
    let mut total = Duration::ZERO;
    for entetes in &requetes {
        // La projection en `&str` est hors du chronomètre : seule la
        // vérification est mesurée.
        let paires: Vec<(&str, &str)> = entetes
            .iter()
            .map(|(nom, valeur)| (nom.as_str(), valeur.as_str()))
            .collect();
        let debut = Instant::now();
        let resultat = verifier.verify(&VerifiedRequest::new(
            "POST",
            "/mcp",
            "127.0.0.1:23080",
            &paires,
            BODY,
        ));
        total += debut.elapsed();
        assert!(resultat.is_ok(), "requête de mesure refusée");
    }
    let par_requete = total.as_secs_f64() * 1e6 / iterations as f64;
    println!("  {iterations} vérifications : {par_requete:.2} µs/requête (moyenne)");
}

#[cfg(not(feature = "auth-sign"))]
fn main() {
    eprintln!("cet exemple exige la feature : cargo run --features auth-sign --example auth_sign");
}
