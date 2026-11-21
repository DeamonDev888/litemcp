# Tests — rejouer la suite localement

[← Retour au README](../README.md)

```bash
cargo test --workspace --all-features         # 851 tests : tout (macros, schemars, auth-sign, tls)
cargo test --workspace                        # 672 tests : jeu par défaut (macros)
cargo test --workspace --no-default-features  # 647 tests : noyau strict (serde, serde_json, getrandom)
```

Comptes mesurés le **2026-11-20** sous **Windows 11**, **rustc 1.99.1** (`cargo 1.99.1`) : **0 échec**
dans les trois configurations, avec **4 tests ignorés** en `--all-features` et **1** dans chacune des
deux autres. Ils remplacent les 828 / 649 / 624 du 2026-11-17 — **vingt-trois tests de plus** dans
chaque configuration (correctifs de l'audit du 2026-11-20 : liaison du `requestState`, file de
handlers, registre et sweep, budget d'écriture SSE) —, eux-mêmes substitués aux 592 / 532 / 508 du
2026-11-13. Sous Linux (rust 1.85, conteneur), `--all-features` le même jour : **235** tests de
bibliothèque (+3 pour `litemcp-macros`), `tests/antizombie.rs` 12, `tests/registry.rs` 3,
`tests/registre_vie.rs` 8, `tests/mcp_progress_channel.rs` 9 — tout vert (c'est là que tournent les
tests `#[cfg(unix)]` du registre et du budget d'écriture).

## Matrice complète

Commandes rejouées telles quelles en local (ce sont celles de la CI et du
gate `verify` de `release-tool`) :

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo clippy --workspace --all-targets --no-default-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
cargo test --workspace --all-features
cargo test --workspace
cargo test --workspace --no-default-features
cargo test -p litemcp --no-default-features --features macros
cargo test -p litemcp --no-default-features --features schemars
cargo test -p litemcp --no-default-features --features auth-sign
cargo test -p litemcp --no-default-features --features tls
cargo build --examples --all-features
cargo tree -p litemcp --no-default-features -e normal --depth 1   # 3 dépendances directes : serde, serde_json, getrandom
cargo +1.85 check --workspace --all-targets --all-features        # MSRV déclarée
```

## Ce que la CI couvre réellement

Relevé dans `.github/workflows/ci.yml` le 2026-11-16 — cinq jobs :

| Job | Runner | Ce qu'il exécute |
|---|---|---|
| `test` | `ubuntu-latest` | `cargo fmt --check` ; clippy `--all-features` ; clippy `--no-default-features` ; `cargo test --workspace --all-features` ; `cargo test --workspace` (configuration par défaut) ; `cargo test --workspace --no-default-features` ; **chaque feature isolée** (`macros`, `schemars`, `auth-sign`, `tls`) en `cargo check` **et** `cargo test` ; `cargo build --examples --all-features` ; simulations du pipeline de publication (dans `cargo test --workspace`, crate `release-tool`) ; rustdoc strict (`RUSTDOCFLAGS=-D warnings`) |
| `msrv` | `ubuntu-latest`, toolchain 1.85 | `cargo check --workspace --all-targets --all-features` |
| `windows` | `windows-latest` | `cargo test --workspace` **et** `cargo test --workspace --all-features` |
| `macos` | `macos-latest` | `cargo test --workspace` **et** `cargo test --workspace --all-features` |
| `advisories` | `ubuntu-latest` | `cargo deny check advisories` |

Trois affirmations périmées, corrigées ici :

- **La CI n'est pas Linux seule.** `windows-latest` et `macos-latest` exécutent la suite depuis
  l'ajout des jobs ; ils lancent désormais **la configuration par défaut et `--all-features`**.
- **La configuration par défaut est testée** (job `test`, étape « Tests (configuration par
  defaut) » : `cargo test --workspace`), et les features isolées ne se limitent plus à un `cargo
  check` : chacune a aussi son `cargo test`.
- **`tests/bind_default.rs` ne panique plus** quand la tranche `23080..=23099` est occupée : il
  photographie la tranche en réservant lui-même les ports libres (`try_hold`) et attend le premier
  port libre restant ; si les vingt ports sont pris, il vérifie seulement le repli sur un port
  éphémère, hors tranche et sur loopback.

Ce que la CI **ne** couvre pas : ni clippy ni rustdoc sur Windows/macOS (seuls les tests y
tournent), ni les features isolées hors Linux, ni la conformance MCP
(`node .github/conformance/check_baseline.js`), ni les campagnes de mesure de `docs/bench/`.

## Sockets réelles

Les tests d'intégration ouvrent de vraies sockets sur `127.0.0.1:0` (port éphémère choisi par l'OS),
sauf `tests/bind_default.rs` qui vise la tranche `23080..=23099` avec le repli décrit ci-dessus.
Aucun test n'exige un port fixe libre : la suite peut tourner sur une machine où un serveur écoute
déjà sur `23080`.
