//! Audit 2026-11-25 — B10 : la visibilité du `register_<nom>` généré par
//! `#[tool]` et `#[resource]` recopie celle de la fonction annotée, au lieu
//! d'un `pub` inconditionnel.
//!
//! Le `#![deny(unreachable_pub)]` ci-dessous reproduit le constat de
//! l'audit : avant correction, les `pub fn register_somme_privee` et
//! `pub fn register_note_interne` générés à côté de fonctions privées,
//! dans un module privé, déclenchaient le lint chez les utilisateurs qui
//! l'activent et cette cible ne compilait pas ; après correction, elle
//! compile et tous les tests passent.

#![cfg(feature = "macros")]
#![deny(unreachable_pub)]

use litemcp::ServerBuilder;
use litemcp::macros::{resource, tool};
use serde_json::{Value, json};

/// Fonction **publique** : son `register_somme_publique` généré reste `pub`.
#[tool(
    name = "somme_publique",
    description = "Additionne deux entiers (publique)"
)]
pub fn somme_publique(a: i64, b: i64) -> litemcp::ToolResult {
    Ok(json!({ "somme": a + b }))
}

/// Fonction **`pub(crate)`** : son `register_plus_un` généré est
/// `pub(crate)` — enchaînable depuis n'importe quel module de la crate.
#[tool(name = "plus_un", description = "Incrémente un entier (pub(crate))")]
pub(crate) fn plus_un(n: i64) -> litemcp::ToolResult {
    Ok(json!({ "resultat": n + 1 }))
}

/// Fonction **privée** : son `register_somme_privee` généré est privé —
/// utilisable dans ce fichier, pas depuis une autre crate.
#[tool(
    name = "somme_privee",
    description = "Additionne deux entiers (privée)"
)]
fn somme_privee(a: i64, b: i64) -> litemcp::ToolResult {
    Ok(json!({ "somme": a + b }))
}

mod source_privee {
    use super::*;

    /// Ressource **privée** : la même règle s'applique au
    /// `register_note_interne` généré par `#[resource]`.
    #[resource(
        uri = "mem://interne",
        name = "Note",
        description = "Note interne (privée)"
    )]
    fn note_interne() -> Result<litemcp::protocol::ResourceContents, String> {
        Ok(litemcp::protocol::ResourceContents {
            uri: "mem://interne".to_string(),
            mime_type: None,
            text: "Interne".to_string(),
        })
    }

    /// Cas privé du brief : le `register_<nom>` généré d'une fonction privée
    /// reste utilisable DANS son module.
    #[test]
    fn register_prive_est_utilisable_dans_son_module() {
        let core = ServerBuilder::new("b10", "0.1.0")
            .with_tool(register_somme_privee)
            .with_tool(register_note_interne)
            .core();

        let outils = tools_list(&core);
        assert_eq!(
            outils,
            json!(["somme_privee"]),
            "l'outil privé doit répondre : {outils}"
        );
        let ressources = resources_list(&core);
        assert_eq!(
            ressources,
            json!(["mem://interne"]),
            "la ressource privée doit répondre : {ressources}"
        );
    }
}

mod consommateur_crate {
    use super::call;
    use crate::ServerBuilder;
    // `register_plus_un` est `pub(crate)` comme `plus_un` : importable depuis
    // n'importe quel module de la crate, pas seulement le module déclarant.
    use crate::register_plus_un;

    /// Cas `pub(crate)` : le `register_<nom>` généré suit la visibilité
    /// restreinte de sa fonction source.
    #[test]
    fn register_pub_crate_est_utilisable_dans_toute_la_crate() {
        let core = ServerBuilder::new("b10", "0.1.0")
            .with_tool(register_plus_un)
            .core();

        let v = call(
            &core,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"plus_un","arguments":{"n":41}}}"#,
        );
        assert_eq!(v["result"]["structuredContent"]["resultat"], 42, "{v}");
    }
}

/// Cas pub du brief : le `register_<nom>` d'une fonction publique continue
/// de fonctionner, enchaîné depuis la racine — comme avant la correction.
#[test]
fn register_pub_continu_de_fonctionner() {
    let core = ServerBuilder::new("b10", "0.1.0")
        .with_tool(register_somme_publique)
        .with_tool(register_plus_un)
        .core();

    let v = call(
        &core,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"somme_publique","arguments":{"a":2,"b":3}}}"#,
    );
    assert_eq!(v["result"]["structuredContent"]["somme"], 5, "{v}");

    let v = call(
        &core,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"plus_un","arguments":{"n":41}}}"#,
    );
    assert_eq!(v["result"]["structuredContent"]["resultat"], 42, "{v}");
}

fn call(core: &litemcp::ServerCore, body: &str) -> Value {
    let reply = core.dispatch_unchecked(body.as_bytes()).unwrap();
    serde_json::from_slice(&reply).unwrap()
}

fn tools_list(core: &litemcp::ServerCore) -> Value {
    let v = call(core, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|outil| outil["name"].clone())
        .collect::<Vec<_>>()
        .into()
}

fn resources_list(core: &litemcp::ServerCore) -> Value {
    let v = call(
        core,
        r#"{"jsonrpc":"2.0","id":1,"method":"resources/list"}"#,
    );
    v["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entree| entree["uri"].clone())
        .collect::<Vec<_>>()
        .into()
}
