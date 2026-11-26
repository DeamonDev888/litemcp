//! Audit 2026-11-25 (partie 1, P1) — tests de régression des correctifs B1
//! et B5 sur les en-têtes MCP modernes (SEP-2243).
//!
//! * **B1** — un `Mcp-Name` non-ASCII voyage encodé en `=?base64?…?=` : la
//!   vérification de signature RFC 9421 doit décoder la sentinelle **avant**
//!   de comparer au corps. Avant correctif, toute requête signée visant un
//!   outil, un prompt ou une ressource non-ASCII était refusée en 401
//!   (`covered_set_not_allowed`). La base de signature `@signature-params`
//!   garde la valeur brute : seul le verdict de liaison change.
//! * **B5** — un paramètre `zod::uint().header(…)` publie « integer » sur
//!   `[0, u64::MAX]` : la comparaison `Mcp-Param-*`/corps doit accepter les
//!   u64 au-delà de `i64::MAX` (ex. `9223372036854775808`). Avant correctif,
//!   l'appel échouait en `-32020` (« Mcp-Param-* header value does not match
//!   argument »).
//!
//! Le volet B5 s'exécute **sans aucune feature** (transport seul) ; le volet
//! B1 est neutralisé sans `auth-sign` (convention `cfg` des fichiers
//! existants, ex. `tests/auth_tls_2026_11_15.rs`).

use litemcp::protocol::{META_CLIENT_CAPABILITIES, META_PROTOCOL_VERSION, PROTOCOL_VERSION};
use litemcp::{ProtocolProfile, ServerBuilder, zod};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

fn valid_meta() -> Value {
    json!({
        META_PROTOCOL_VERSION: PROTOCOL_VERSION,
        META_CLIENT_CAPABILITIES: {}
    })
}

/// Serveur strict muni d'un outil `u64_tool` (`zod::uint()` promu en en-tête
/// `Mcp-Param-count`) et d'un outil `i64_tool` (`zod::integer()`, contrôle
/// « comportement inchangé »).
fn spawn_audit_server() -> SocketAddr {
    let bound = ServerBuilder::new("audit-2026-11-25", "1.0.0")
        .protocol_profile(ProtocolProfile::Strict2026_07_28)
        .tool_typed(
            "u64_tool",
            "Unsigned counter promoted to header",
            zod::object().field("count", zod::uint().header("count")),
            |args| Ok(args.clone()),
        )
        .tool_typed(
            "i64_tool",
            "Signed counter promoted to header",
            zod::object().field("count", zod::integer().header("count")),
            |args| Ok(args.clone()),
        )
        .bind("127.0.0.1:0")
        .expect("bind audit server");
    let addr = bound.local_addr().expect("local_addr");
    std::thread::spawn(move || {
        let _ = bound.serve();
    });
    addr
}

fn raw_exchange(addr: SocketAddr, raw_request: &str) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout");
    stream.write_all(raw_request.as_bytes()).expect("write");
    stream.flush().expect("flush");

    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    let s = String::from_utf8_lossy(&buf).to_string();
    let (head, body) = s.split_once("\r\n\r\n").unwrap_or((&s, ""));
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let val: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    (status, val)
}

fn send_audit_post(addr: SocketAddr, headers: &[(&str, &str)], body: &str) -> (u16, Value) {
    let mut req = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    raw_exchange(addr, &req)
}

fn corps_appel(outil: &str, count: Value) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "_meta": valid_meta(),
            "name": outil,
            "arguments": { "count": count }
        }
    })
    .to_string()
}

/// `i64::MAX + 1` : le plus petit u64 hors de portée de `parse::<i64>()`.
const AU_DELA_DE_I64_MAX: u64 = 9223372036854775808;

// ============================================================================
// B5 — Mcp-Param-* entier : u64 au-delà de i64::MAX (sans feature)
// ============================================================================

#[test]
fn b5_entier_u64_au_dela_de_i64_max_dans_le_corps_et_l_en_tete_est_accepte() {
    let addr = spawn_audit_server();
    let corps = corps_appel("u64_tool", json!(AU_DELA_DE_I64_MAX));
    let (status, val) = send_audit_post(
        addr,
        &[
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "u64_tool"),
            ("Mcp-Param-count", "9223372036854775808"),
        ],
        &corps,
    );
    assert_eq!(
        status, 200,
        "un u64 > i64::MAX identique dans le corps et l'en-tête doit être accepté : {val}"
    );
    assert!(val.get("error").is_none(), "l'appel doit réussir : {val}");
    assert_eq!(
        val["result"]["structuredContent"]["count"],
        json!(AU_DELA_DE_I64_MAX),
        "le handler reçoit l'argument exact : {val}"
    );
}

#[test]
fn b5_entier_u64_max_est_accepte() {
    let addr = spawn_audit_server();
    let corps = corps_appel("u64_tool", json!(u64::MAX));
    let (status, val) = send_audit_post(
        addr,
        &[
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "u64_tool"),
            ("Mcp-Param-count", "18446744073709551615"),
        ],
        &corps,
    );
    assert_eq!(
        status, 200,
        "u64::MAX est dans la plage publiée par z.uint() : {val}"
    );
    assert!(val.get("error").is_none(), "l'appel doit réussir : {val}");
}

#[test]
fn b5_entier_u64_en_tete_differente_de_l_argument_est_refuse() {
    let addr = spawn_audit_server();
    let corps = corps_appel("u64_tool", json!(AU_DELA_DE_I64_MAX));

    // En-tête hors de la plage u64 publiable : aucun argument ne peut
    // correspondre.
    let (status, val) = send_audit_post(
        addr,
        &[
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "u64_tool"),
            ("Mcp-Param-count", "18446744073709551616"),
        ],
        &corps,
    );
    assert_eq!(status, 400, "u64::MAX + 1 ne peut pas matcher : {val}");
    assert_eq!(val["error"]["code"], -32020);

    // En-tête u64 différente de l'argument (même plage, valeur divergente) :
    // le nouveau chemin u64 doit refuser la liaison.
    let (status, val) = send_audit_post(
        addr,
        &[
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "u64_tool"),
            ("Mcp-Param-count", "42"),
        ],
        &corps,
    );
    assert_eq!(status, 400, "42 ne matche pas 9223372036854775808 : {val}");
    assert_eq!(val["error"]["code"], -32020);
}

#[test]
fn b5_comportement_inchange_pour_i64_et_les_flottants_exacts() {
    let addr = spawn_audit_server();

    // i64 classique, négatif (outil zod::integer()).
    let (status, val) = send_audit_post(
        addr,
        &[
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "i64_tool"),
            ("Mcp-Param-count", "-7"),
        ],
        &corps_appel("i64_tool", json!(-7)),
    );
    assert_eq!(status, 200, "i64 négatif inchangé : {val}");

    // Flottant exact : 42.0 dans le corps matche « 42 » dans l'en-tête
    // (z.uint() normalise les intégraux flottants en u64).
    let (status, val) = send_audit_post(
        addr,
        &[
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "u64_tool"),
            ("Mcp-Param-count", "42"),
        ],
        &corps_appel("u64_tool", json!(42.0)),
    );
    assert_eq!(status, 200, "flottant exact inchangé : {val}");
    assert_eq!(
        val["result"]["structuredContent"]["count"],
        json!(42),
        "l'intégral flottant est normalisé en u64 : {val}"
    );
}

// ============================================================================
// B1 — Mcp-Name non-ASCII encodé =?base64?…?= et signature RFC 9421
// (feature auth-sign)
// ============================================================================

#[cfg(feature = "auth-sign")]
mod b1_nom_non_ascii_et_signature {
    use litemcp::auth::sign::{
        ClientKey, KeyStore, NonceCache, RequestToSign, SignatureVerifier, Signer, SigningKey,
        TestKeyStore,
    };
    use litemcp::auth::{AuthContext, AuthError, ProofMethod, VerifiedRequest, Verifier};
    use std::sync::Arc;

    /// Graine du vecteur 1 de la RFC 8032 §7.1 (publique : c'est un vecteur).
    const SEED: [u8; 32] = [
        0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c,
        0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae,
        0x7f, 0x60,
    ];
    /// `created=1794700800`, instant du vecteur de `docs/auth.md` §2.5.
    const NOW: u64 = 1_794_700_800;
    /// Nonce du vecteur : les octets `00 01 … 0f`.
    const NONCE: [u8; 16] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f,
    ];

    /// Outil à nom non-ASCII : en profil SEP-2243, ce nom voyage encodé dans
    /// `Mcp-Name`.
    const NOM: &str = "outil-café";
    /// base64 standard de « outil-café » (UTF-8) : `b3V0aWwtY2Fmw6k=`.
    const SENTINELLE: &str = "=?base64?b3V0aWwtY2Fmw6k=?=";

    /// Corps scellé : `params.name` porte le nom **décodé** (`&str` : le
    /// littéral n'est pas limité à l'ASCII, `as_bytes` le scelle octet pour
    /// octet).
    const CORPS: &str = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"outil-café","arguments":{},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}"#;

    struct Banc {
        signer: Signer,
        verifier: SignatureVerifier,
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
        Banc { signer, verifier }
    }

    /// Signe puis vérifie une requête `tools/call` dont l'en-tête `Mcp-Name`
    /// vaut `mcp_name` (valeur brute signée), le corps portant toujours
    /// `params.name = "outil-café"`. La signature porte la valeur brute : si
    /// la vérification échoue, c'est la liaison en-tête/corps qui échoue.
    fn verifier_avec(banc: &Banc, mcp_name: &str) -> Result<AuthContext, AuthError> {
        let entetes = [
            ("Host", "127.0.0.1:23080"),
            ("Content-Type", "application/json"),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", mcp_name),
        ];
        let requete = RequestToSign::new(
            "POST",
            "/mcp",
            "127.0.0.1:23080",
            &entetes,
            CORPS.as_bytes(),
        );
        let signed = banc.signer.headers_at(&requete, NOW, &NONCE);
        let mut recus: Vec<(&str, &str)> = entetes.to_vec();
        recus.extend(signed.iter());
        banc.verifier.verify(&VerifiedRequest::new(
            "POST",
            "/mcp",
            "127.0.0.1:23080",
            &recus,
            CORPS.as_bytes(),
        ))
    }

    #[test]
    fn b1_nom_non_ascii_encode_en_base64_est_accepte() {
        let banc = banc();
        let contexte =
            verifier_avec(&banc, SENTINELLE).expect("la requête signée doit être acceptée");
        assert_eq!(contexte.client(), "client-demo");
        assert!(matches!(
            contexte.proof(),
            ProofMethod::Signature { keyid } if keyid == "client-demo"
        ));
    }

    #[test]
    fn b1_nom_encode_ne_correspondant_pas_au_corps_est_refuse() {
        let banc = banc();
        // base64 de « echo » : la sentinelle décodée ne désigne pas l'outil
        // appelé par le corps.
        let erreur = verifier_avec(&banc, "=?base64?ZWNoby==?=")
            .expect_err("la liaison doit échouer (401 covered_set_not_allowed)");
        assert_eq!(erreur.reason(), "covered_set_not_allowed");
    }

    #[test]
    fn b1_sentinelle_indecodable_est_un_refus_de_liaison() {
        let banc = banc();
        for brute in [
            "=?base64?b3V0aWwtY2Fmw6?=", // longueur non multiple de 4
            "=?base64?ZW!oby==?=",       // caractère hors alphabet
            "=?base64?/w==?=",           // décode en octets non UTF-8
        ] {
            let erreur = verifier_avec(&banc, brute)
                .expect_err("une valeur non décodable est un échec de liaison");
            assert_eq!(
                erreur.reason(),
                "covered_set_not_allowed",
                "sentinelle indécodable acceptée : {brute}"
            );
        }
    }

    #[test]
    fn b1_nom_non_ascii_brut_non_encode_reste_accepte() {
        // Contrôle de non-régression : sans sentinelle, la comparaison
        // directe du nom non-ASCII devait déjà réussir.
        let banc = banc();
        verifier_avec(&banc, NOM).expect("le nom brut non encodé doit rester accepté");
    }
}
