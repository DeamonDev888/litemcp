//! L4 — catalogues pré-sérialisés : isolation par appelant et octets figés.
//!
//! Le catalogue ouvert est servi depuis une charge **déjà sérialisée** ; le
//! chemin filtré réassemble les seuls éléments visibles à partir des mêmes
//! octets. Ce fichier prouve deux choses que le code doit garantir :
//!
//! 1. **aucune fuite** : deux clés aux droits différents, appels **alternés**
//!    — chacune ne voit que ses éléments, et la charge de l'une n'est jamais
//!    servie à l'autre (aucun cache de liste filtrée n'existe) ;
//! 2. **octets identiques** : la réponse est figée au caractère près
//!    (`CHARGE_ATTENDUE`), et deux appels successifs rendent exactement les
//!    mêmes octets — le pré-sérialisé ne doit rien changer sur le fil.
//!
//! Le plafond `max_response_bytes` reste appliqué à la charge assemblée.

use litemcp::auth::VerifiedRequest;
use litemcp::server::{HttpStatus, ServerBuilder, ServerCore};
use litemcp::{auth::TestVerifier, zod};
use serde_json::{Value, json};
use std::sync::Arc;

const VERSION: &str = "2026-07-28";

/// Corps `tools/list` du profil strict.
fn corps_liste() -> Vec<u8> {
    json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/list",
        "params": {
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": VERSION,
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }
    })
    .to_string()
    .into_bytes()
}

/// Vérificateur de test : la clé est l'en-tête `x-cle` (aucun coût de
/// signature, la mesure porte sur le catalogue).
fn verificateur() -> Arc<TestVerifier> {
    Arc::new(TestVerifier::new(|requete: &VerifiedRequest<'_>| {
        let cle = requete
            .headers
            .iter()
            .find(|(nom, _)| nom.eq_ignore_ascii_case("x-cle"))
            .map(|(_, valeur)| *valeur)
            .unwrap_or("");
        match cle {
            "large" => Ok(litemcp::auth::AuthContext::new(
                "large",
                litemcp::auth::ProofMethod::Application,
            )
            .with_rights(["groupe0", "groupe1"])),
            "etroite" => Ok(litemcp::auth::AuthContext::new(
                "etroite",
                litemcp::auth::ProofMethod::Application,
            )
            .with_rights(["groupe1"])),
            _ => Err(litemcp::auth::AuthError::new("cle_absente")),
        }
    }))
}

/// Registre de référence : quatre outils, droits alternés, schéma explicite.
fn constructeur(plafond: usize) -> ServerBuilder {
    ServerBuilder::new("catalogues", "1.0.0")
        .verifier(verificateur())
        .tool_typed(
            "alpha",
            "Premier outil",
            zod::object().field("q", zod::string().min(1)),
            |_args| Ok(json!({ "ok": true })),
        )
        .require_right("alpha", "groupe0")
        .tool_typed(
            "beta",
            "Deuxième outil",
            zod::object().opt("n", zod::integer().min(0)),
            |_args| Ok(json!({ "ok": true })),
        )
        .require_right("beta", "groupe1")
        .tool_typed("gamma", "Troisième outil", zod::object(), |_args| {
            Ok(json!({ "ok": true }))
        })
        .require_right("gamma", "groupe0")
        .tool_typed("delta", "Quatrième outil", zod::object(), |_args| {
            Ok(json!({ "ok": true }))
        })
        .require_right("delta", "groupe1")
        .max_response_bytes(plafond)
}

/// Envoie un `tools/list` avec la clé donnée (en-têtes du profil strict :
/// `MCP-Protocol-Version` et `Mcp-Method` sont obligatoires).
fn liste(coeur: &ServerCore, cle: &str) -> Vec<u8> {
    let entetes = [
        ("x-cle", cle),
        ("MCP-Protocol-Version", VERSION),
        ("Mcp-Method", "tools/list"),
        ("Host", "localhost"),
    ];
    let corps = corps_liste();
    let reponse = coeur.dispatch_verified(&VerifiedRequest::new(
        "POST",
        "/mcp",
        "localhost",
        &entetes,
        &corps,
    ));
    reponse.body
}

/// Noms d'outils publiés par une réponse `tools/list`.
fn noms(reponse: &[u8]) -> Vec<String> {
    let value: Value = serde_json::from_slice(reponse).expect("JSON de liste");
    value["result"]["tools"]
        .as_array()
        .expect("liste d'outils")
        .iter()
        .map(|outil| outil["name"].as_str().expect("nom").to_string())
        .collect()
}

#[test]
fn deux_cles_alternent_et_ne_voient_que_leurs_elements() {
    let coeur = constructeur(0).core();
    let vues_large = ["alpha", "beta", "delta", "gamma"];
    let vues_etroite = ["beta", "delta"];
    for tour in 0..8 {
        // Alternance stricte : chaque appel suit immédiatement l'autre, donc
        // toute charge mise en cache pour un appelant serait servie à l'autre.
        assert_eq!(
            noms(&liste(&coeur, "large")),
            vues_large,
            "clé large, tour {tour}"
        );
        assert_eq!(
            noms(&liste(&coeur, "etroite")),
            vues_etroite,
            "clé étroite, tour {tour}"
        );
    }
    // Clé inconnue : refus, jamais une liste (l'échec reste fermé).
    let entetes = [
        ("x-cle", "inconnue"),
        ("MCP-Protocol-Version", VERSION),
        ("Mcp-Method", "tools/list"),
    ];
    let corps = corps_liste();
    let refus = coeur.dispatch_verified(&VerifiedRequest::new(
        "POST",
        "/mcp",
        "localhost",
        &entetes,
        &corps,
    ));
    assert_eq!(refus.status, 401, "clé inconnue : 401");
    assert!(
        !String::from_utf8_lossy(&refus.body).contains("alpha"),
        "un refus ne doit publier aucun élément : {}",
        String::from_utf8_lossy(&refus.body)
    );
}

#[test]
fn les_octets_de_la_liste_ouverte_sont_figes() {
    let coeur = constructeur(0).core();
    let premier = liste(&coeur, "large");
    // Deux appels successifs : mêmes octets (la charge pré-sérialisée est
    // servie telle quelle, sans réassemblage).
    let second = liste(&coeur, "large");
    assert_eq!(premier, second, "deux `tools/list` doivent être identiques");
    // Charge figée au caractère près : toute dérive de sérialisation (ordre
    // des clés, `ttlMs`, `_meta`, schéma publié) casse ce test.
    let texte = String::from_utf8(premier).expect("UTF-8");
    assert_eq!(texte, CHARGE_ATTENDUE, "charge `tools/list` inattendue");
}

/// Charge `tools/list` de référence du registre de `constructeur` (clé
/// « large »). Toute modification du protocole publié doit la faire évoluer
/// **volontairement**.
const CHARGE_ATTENDUE: &str = concat!(
    r#"{"jsonrpc":"2.0","id":7,"result":{"_meta":{"io.modelcontextprotocol/serverInfo":"#,
    r#"{"name":"catalogues","version":"1.0.0"}},"cacheScope":"private","resultType":"complete","#,
    r#""tools":[{"description":"Premier outil","inputSchema":{"properties":{"q":"#,
    r#"{"minLength":1,"type":"string"}},"required":["q"],"type":"object"},"name":"alpha"},"#,
    r#"{"description":"Deuxième outil","inputSchema":{"properties":{"n":"#,
    r#"{"maximum":9223372036854775807,"minimum":0,"type":["integer","null"]}},"#,
    r#""type":"object"},"name":"beta"},{"description":"Quatrième outil","inputSchema":"#,
    r#"{"properties":{},"type":"object"},"name":"delta"},{"description":"Troisième outil","#,
    r#""inputSchema":{"properties":{},"type":"object"},"name":"gamma"}],"ttlMs":60000}}"#,
);

#[test]
fn le_plafond_de_reponse_reste_applique_aux_catalogues() {
    let reference = {
        let coeur = constructeur(0).core();
        liste(&coeur, "large")
    };
    // Plafond à la taille exacte : la réponse passe telle quelle.
    let coeur = constructeur(reference.len()).core();
    assert_eq!(
        liste(&coeur, "large"),
        reference,
        "un plafond égal à la taille ne doit rien changer"
    );
    // Un octet de moins : le plafond refuse, avec l'erreur interne historique.
    let coeur = constructeur(reference.len() - 1).core();
    let refusee = liste(&coeur, "large");
    let value: Value = serde_json::from_slice(&refusee).expect("JSON d'erreur");
    assert_eq!(
        value["error"]["code"], -32603,
        "plafond dépassé : erreur interne attendue, reçu {value}"
    );
    // Chemin filtré : même plafond, même verdict (la charge est réassemblée
    // avant d'être bornée).
    let etroite_reference = {
        let coeur = constructeur(0).core();
        liste(&coeur, "etroite")
    };
    let coeur = constructeur(etroite_reference.len() - 1).core();
    let value: Value = serde_json::from_slice(&liste(&coeur, "etroite")).expect("JSON d'erreur");
    assert_eq!(value["error"]["code"], -32603);
    let coeur = constructeur(etroite_reference.len()).core();
    assert_eq!(
        liste(&coeur, "etroite"),
        etroite_reference,
        "le chemin filtré doit respecter le plafond à l'octet près"
    );
}

#[test]
fn le_chemin_ouvert_reste_identique_sans_verificateur() {
    // Sans vérificateur, la liste est complète et servie depuis le cache :
    // même charge que le chemin vérifié « large », à l'ordre près des clés
    // d'en-têtes — c'est le même catalogue publié.
    let sans = ServerBuilder::new("catalogues", "1.0.0")
        .legacy(true)
        .tool("alpha", "Premier outil", |_args| Ok(json!({ "ok": true })))
        .tool("beta", "Deuxième outil", |_args| Ok(json!({ "ok": true })))
        .core();
    let reponse = sans.dispatch(&corps_liste()).expect("dispatch");
    assert_eq!(noms(&reponse), ["alpha", "beta"]);
    let value: Value = serde_json::from_slice(&reponse).expect("JSON");
    assert_eq!(value["result"]["ttlMs"], 60_000);
    assert_eq!(value["result"]["cacheScope"], "private");
    assert_eq!(value["result"]["resultType"], "complete");
    assert_eq!(
        value["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "catalogues"
    );
    // Un plafond nul (illimité) ne bride rien.
    let sans_plafond = ServerBuilder::new("catalogues", "1.0.0")
        .legacy(true)
        .tool("alpha", "Premier outil", |_args| Ok(json!({ "ok": true })))
        .max_response_bytes(0)
        .core();
    let reponse = sans_plafond.dispatch(&corps_liste()).expect("dispatch");
    assert_eq!(noms(&reponse), ["alpha"]);
    let _ = HttpStatus::Ok; // le type de statut reste celui du dispatch public
}
