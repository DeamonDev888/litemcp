# litemcp

> ⚠️ Pré-alpha — Phase 0 (fondations). API instable, ne pas publier d'applications critiques dessus.

**litemcp** vise à être le framework MCP (Model Context Protocol) **le plus rapide et le plus léger en mémoire** pour Rust :

- **Zéro runtime async au cœur** — threads bloquants (`std`), pas de tokio, pas de hyper ;
- **2 dépendances au core** : `serde` + `serde_json`, rien d'autre ;
- **Spec MCP 2026-07-28** (Streamable HTTP stateless : un POST unique → JSON ou SSE) en cible primaire ;
- **stdio natif** (framing newline-delimited JSON-RPC, identique au transport standard) ;
- **`#![forbid(unsafe_code)]`**, MSRV fixé (1.85, edition 2024) ;
- **Benchmarks mémoire (RSS) et binaires mesurés à chaque release**, comparés publiquement à rmcp.

## Pourquoi ?

| | rmcp (officiel) | fastmcp_rust | **litemcp** |
|---|---|---|---|
| Runtime | tokio + hyper + tower | asupersync (maison) | **aucun au core** |
| Toolchain | stable | nightly épinglé | **stable** |
| Deps core | lourdes | maison | **serde + serde_json** |
| Spec | 2026-07-28 | en cours, non vérifié | 2026-07-28 stateless-first |

Le créneau visé : **un serveur MCP qui tient dans quelques centaines de Ko de RAM** — CLI, edge, IoT, tests unitaires, processus à durée de vie courte.

## État

Voir la roadmap détaillée : [docs/ROADMAP.md](docs/ROADMAP.md).

| Phase | Contenu | État |
|---|---|---|
| 0 | Fondations (repo, Cargo, CI, licences) | 🚧 en cours |
| 1 | Noyau protocole JSON-RPC + types MCP | ⏳ |
| 2 | Transport stdio + premières mesures | ⏳ |
| 3 | Streamable HTTP stateless (socket bindé) | ⏳ |
| 4 | Macros `#[tool]` + schémas maison | ⏳ |
| 5 | Client + compat legacy 2025-11-25 | ⏳ |
| 6 | Publication crates.io + conformance | ⏳ |

## Licence

Dual `MIT OR Apache-2.0` (voir [LICENSE-MIT](LICENSE-MIT) et [LICENSE-APACHE](LICENSE-APACHE)).
