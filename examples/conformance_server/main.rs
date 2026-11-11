//! Binaire de la fixture de conformance officielle MCP **2026-07-28** — les
//! objets `test_*` attendus par la suite `modelcontextprotocol/conformance`
//! (SEP-1730). La construction du serveur vit dans [`fixture`] (module
//! partagé avec le test de régression `tests/conformance_fixture.rs`).
//!
//! Cette fixture est **dédiée** : elle ne remplace pas `examples/hello` et un
//! résultat obtenu contre `hello` n'est pas un score de conformité (hello ne
//! porte pas les objets `test_*`).
//!
//! Lancement :
//!
//! ```text
//! cargo run --release --example conformance_server -- 127.0.0.1:23080
//! # puis, contre la fixture démarrée :
//! npx -y @modelcontextprotocol/conformance@0.2.0-alpha.12 server \
//!   --url http://127.0.0.1:23080/mcp --requirements 2026-07-28 \
//!   --output-dir <dossier d'artefacts>
//! ```
//!
//! Les outils MRTR (`test_input_required_result_*`, SEP-2322) ne sont pas
//! enregistrés : l'API de handlers de LiteMCP ne permet pas encore de
//! produire un résultat `input_required` (sujet séparé de la feuille de
//! route) — les scénarios correspondants échouent et sont portés, avec
//! justification datée, par la baseline `.github/conformance/`.

mod fixture;

fn main() -> std::io::Result<()> {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:23080".to_string());

    let bound = fixture::build_fixture(&addr)?;

    eprintln!(
        "fixture de conformance LiteMCP sur http://{}/mcp",
        bound.local_addr()?
    );
    bound.serve()
}
