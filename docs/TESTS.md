# Tests — rejouer la suite localement

[← Retour au README](../README.md)

```bash
cargo test --workspace --all-features         # 828 tests : tout (macros, schemars, auth-sign, tls)
cargo test --workspace                        # 649 tests : jeu par défaut (macros)
cargo test --workspace --no-default-features  # 624 tests : noyau strict (serde + serde_json)
```

Comptes mesurés le **2026-11-17** sous **Windows 11**, **rustc 1.99.1** (`cargo 1.99.1`) : **0 échec**
dans les trois configurations, avec **4 tests ignorés** en `--all-features` et **1** dans chacune des
deux autres. Ils remplacent les 825 / 646 / 621 du 2026-11-16, eux-mêmes substitués aux 592 / 532 / 508 du
2026-11-13 : le lot LB (échéance d'écriture effective) a ajouté trois tests, présents dans les
trois configurations.

## Matrice complète

Commandes rejouées telles quelles en local (ce sont celles de la CI et de
`scripts/release.ps1 verify`) :

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
cargo tree -p litemcp --no-default-features -e normal --depth 1   # serde + serde_json seulement
cargo +1.85 check --workspace --all-targets --all-features        # MSRV déclarée
```

## Ce que la CI couvre réellement

Relevé dans `.github/workflows/ci.yml` le 2026-11-16 — cinq jobs :

| Job | Runner | Ce qu'il exécute |
|---|---|---|
| `test` | `ubuntu-latest` | `cargo fmt --check` ; clippy `--all-features` ; clippy `--no-default-features` ; `cargo test --workspace --all-features` ; `cargo test --workspace` (configuration par défaut) ; `cargo test --workspace --no-default-features` ; **chaque feature isolée** (`macros`, `schemars`, `auth-sign`, `tls`) en `cargo check` **et** `cargo test` ; `cargo build --examples --all-features` ; `pwsh scripts/release-tests.ps1` (simulations du pipeline de publication) ; rustdoc strict (`RUSTDOCFLAGS=-D warnings`) |
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
