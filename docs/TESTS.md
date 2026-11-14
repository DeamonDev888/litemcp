# Tests — rejouer la suite localement

[← Retour au README](../README.md)

```bash
cargo test --workspace --all-features         # 592 tests : noyau + macros + schemars (dont doctests)
cargo test --workspace                        # 532 tests : noyau + macros (jeu par défaut)
cargo test --workspace --no-default-features  # 508 tests : noyau strict, sans les macros
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

Comptes mesurés le **2026-11-13** sous Windows 11 (rustc 1.99.1), 0 échec. Ils remplacent les
547 / 504 / 481 des versions précédentes du README, qui dataient d'avant l'ajout de dix tests.

Deux points que la CI ne couvre pas, et qu'il faut donc lire avant de conclure :

- **Linux uniquement.** `.github/workflows/ci.yml` n'a aucun job Windows/macOS : « vérifié sur les
  deux OS » n'est pas établi par la CI, seulement par des exécutions locales sous Windows 11.
- **Configuration par défaut non testée telle quelle.** La CI lance `--all-features` puis
  `--no-default-features` ; le jeu par défaut (macros seules) n'y est pas exécuté, et les features
  isolées n'y passent qu'un `cargo check`.

Les tests d'intégration ouvrent de vraies sockets (`127.0.0.1:0`), sauf `tests/bind_default.rs`
qui vise la tranche `23080..=23099` : il panique si ces ports sont occupés sur la machine.
