<div align="center">

# litemcp

**Le framework MCP le plus rapide et le plus léger pour Rust.**

Zéro runtime async · 2 dépendances · MCP 2026-07-28 stateless · `forbid(unsafe_code)`

![Statut](https://img.shields.io/badge/statut-pr%C3%A9--alpha-orange) ![Licence](https://img.shields.io/badge/licence-MIT%20OR%20Apache--2.0-blue) ![MSRV](https://img.shields.io/badge/MSRV-1.85-orange) ![Spec](https://img.shields.io/badge/MCP-2026--07--28-blue)

<!-- Badge CI à activer après le push GitHub :
![CI](https://github.com/DeamonDev888/litemcp/actions/workflows/ci.yml/badge.svg)
-->

</div>

> [!IMPORTANT]
> **Pré-alpha — Phase 0.** litemcp est en fondation. Le code ci-dessous est l'**API cible** : elle se construit phase par phase ([roadmap](docs/ROADMAP.md)). Rien ne sera publié sur crates.io avant la conformance MCP officielle (Phase 6).

## Sommaire

- [Pourquoi litemcp](#pourquoi-litemcp)
- [Quand choisir litemcp plutôt que rmcp](#quand-choisir-litemcp-plutôt-que-rmcp)
- [Installation](#installation)
- [Démarrage rapide](#démarrage-rapide)
- [Concepts](#concepts)
- [Transports](#transports)
- [Benchmarks](#benchmarks)
- [Tests](#tests)
- [FAQ](#faq)
- [Feuille de route](#feuille-de-route)
- [Licence](#licence)

## Pourquoi litemcp

Un serveur MCP en Rust aujourd'hui, c'est un choix entre deux poids lourds : **rmcp**, l'officiel — excellent, mais il embarque tokio, hyper, tower et schemars — et **fastmcp_rust**, qui exige un nightly épinglé et un runtime maison. Les deux sont conçus pour des serveurs de production riches.

Il existe un autre créneau : **le serveur MCP qui ne coûte presque rien**.

- une CLI qui expose trois outils ;
- un test unitaire qui veut un vrai serveur, sans runtime ;
- un binaire embarqué (edge, IoT) où une pile HTTP complète, c'est non ;
- un agent qui spawn cinquante sous-processus et paie le coût à chaque fois.

**litemcp traite le protocole comme une bibliothèque, pas comme un service.** Le cœur est une pompe JSON-RPC sur des threads bloquants du `std` — pas de tokio, pas de hyper, pas de schemars.

Les principes :

1. **Zéro runtime async au cœur.** Threads `std`, un thread par connexion HTTP. Un adaptateur tokio existera un jour — en feature optionnelle, jamais par défaut.
2. **Deux dépendances, pas une de plus.** `serde` et `serde_json`. Le JSON Schema est dérivé à la maison (Phase 4).
3. **La spec d'abord.** MCP 2026-07-28 (stateless) comme cible primaire ; le legacy 2025-11-25 en feature `legacy`, jamais dans le chemin par défaut.
4. **Mesuré ou rien.** Chaque release publie RSS, taille de binaire et latence p99 — avec les chiffres de rmcp mesurés à plat à côté. Jamais d'adjectif sans chiffre.

## Quand choisir litemcp plutôt que rmcp

| Votre besoin | Choix |
|---|---|
| Un serveur MCP dans une CLI, une TUI, un test | **litemcp** |
| Budget RAM serré (edge, IoT, embarqué) | **litemcp** |
| Démarrage à froid < 10 ms, binaire minimal | **litemcp** |
| OAuth 2.1, tasks, elicitation, écosystème officiel | rmcp |
| Serveur de production riche, multi-features | rmcp |

rmcp est excellent — litemcp ne cherche pas à le remplacer partout. litemcp occupe le créneau que la pile officielle ne peut pas occuper sans casser sa compatibilité tokio.

## Installation

```toml
# Phase 6 — pas encore publié sur crates.io
[dependencies]
litemcp = "0.1"
```

En attendant la publication, la dépendance se prend par git :

```toml
[dependencies]
litemcp = { git = "https://github.com/DeamonDev888/litemcp" }
```

## Démarrage rapide

### Serveur stdio — Phase 2 (API cible)

```rust
use litemcp::prelude::*;

#[tool(description = "Additionne deux nombres")]
fn add(a: i64, b: i64) -> String {
    (a + b).to_string()
}

fn main() -> std::io::Result<()> {
    ServerBuilder::new("calc", "1.0.0")
        .tool(add)
        .serve_stdio()      // câblé sur stdin/stdout, thread unique
}
```

C'est tout — ajoutez la commande dans Claude Desktop ou Cursor (`mcpServers`) et le serveur répond.

### Serveur Streamable HTTP stateless — Phase 3 (API cible)

```rust
fn main() -> std::io::Result<()> {
    ServerBuilder::new("calc", "1.0.0")
        .tool(add)
        .bind("127.0.0.1:8080")?   // TcpListener, un thread par connexion
        .serve()                   // POST /mcp → JSON ou SSE
}
```

Une seule route, pas de session, chaque POST auto-suffisant : c'est la spec 2026-07-28. Derrière nginx pour le TLS, et le serveur tient dans une poignée de Ko.

Testez avec MCP Inspector (mode URL) : `npx @modelcontextprotocol/inspector` → `http://127.0.0.1:8080/mcp`.

## Concepts

### Outils

La macro `#[tool]` génère le handler et le JSON Schema depuis la signature Rust — sans schemars :

```rust
#[tool(description = "Recherche un dossier (référence générique)")]
fn search(query: String, limit: Option<u32>) -> Vec<Hit> { /* … */ }
```

`Option<T>` → champ optionnel du schéma ; `Result<T, E>` → erreur JSON-RPC typée ; le retour sérialisé → `structuredContent`.

### Ressources et prompts

```rust
#[resource(uri = "config://app")]
fn config() -> String { /* … */ }

#[prompt(description = "Modèle de compte rendu")]
fn compte_rendu(date: String) -> Vec<Message> { /* … */ }
```

### Erreurs

Un `Result` rustique partout, avec la séparation de la spec : une erreur applicative devient une erreur JSON-RPC propre ; une erreur d'exécution d'outil devient un `CallToolResult::isError` que le client affiche. Jamais les deux mélangés.

## Transports

| Transport | Statut | Détail |
|---|---|---|
| **stdio** | Phase 2 | JSON-RPC délimité par newlines sur n'importe quel `Read + Write` — le framing canonique de la spec |
| **Streamable HTTP** | Phase 3 | `POST /mcp` unique → `application/json` ou `text/event-stream` request-scoped ; `GET` → 405 ; garde DNS-rebinding sur `Origin`/`Host` |
| Adaptateur tokio/hyper | Plus tard | Feature optionnelle, jamais par défaut |

## Benchmarks

> Règle du projet : **jamais d'adjectif sans chiffre.** Les cibles ci-dessous sont des hypothèses de départ ; la première mesure (Phase 2) les ajuste une fois, puis elles sont figées comme SLO et republiées à chaque release, avec rmcp mesuré dans les mêmes conditions.

| Métrique | Cible initiale | Mesuré |
|---|---|---|
| RSS hello-tool (stdio) | < 2 Mo | — (Phase 2) |
| Binaire release stripped | < 1,5 Mo | — (Phase 2) |
| `tools/call` p99 local | < 1 ms | — (Phase 3) |
| Démarrage à froid (stdio) | < 10 ms | — (Phase 2) |

## Tests

```bash
cargo test                          # unitaires + intégration
cargo clippy -- -D warnings         # CI stricte
```

Le corpus de messages vient de la spec (golden files), le parseur HTTP est fuzzé (Phase 3) et chaque transport est testé de bout en bout contre MCP Inspector.

## FAQ

**Pourquoi pas de tokio ?**
Parce qu'un serveur MCP mono-outil n'a pas besoin d'un runtime async complet pour lire des lignes et répondre du JSON. Le thread par connexion suffit jusqu'à des centaines de connexions — et l'adaptateur tokio restera disponible pour les autres cas.

**Pourquoi ne pas contribuer à rmcp ?**
rmcp est le SDK officiel : il doit couvrir toute la spec pour tout le monde, sur tokio. Cette contrainte impose son poids. litemcp explore la question inverse : que reste-t-il quand on retire tout ce qui n'est pas strictement nécessaire ?

**Pourquoi réécrire HTTP ?**
Le périmètre est volontairement minuscule : une route POST, `Content-Length`, keep-alive, un writer SSE. Pas de chunked entrant, pas de TLS, pas de compression. Si le périmètre s'avère piégeux, le repli documenté est `tiny_http` — la décision sera tranchée par les tests, pas par le dogme.

**Quand sur crates.io ?**
Après la conformance officielle MCP verte (Phase 6). Publier un crate « le plus léger » non conforme serait le meilleur moyen de le rendre inutile.

## Feuille de route

| Phase | Contenu | État |
|---|---|---|
| 0 | Fondations (repo, Cargo, CI, licences) | ✅ |
| 1 | Noyau protocole JSON-RPC + types MCP | ⏳ |
| 2 | Transport stdio + premières mesures | ⏳ |
| 3 | Streamable HTTP stateless | ⏳ |
| 4 | Macros `#[tool]` + schémas maison | ⏳ |
| 5 | Client + compat legacy 2025-11-25 | ⏳ |
| 6 | crates.io + conformance | ⏳ |

Le détail — jalons, SLO, risques, journal : [docs/ROADMAP.md](docs/ROADMAP.md).

## Licence

`MIT OR Apache-2.0` — [LICENSE-MIT](LICENSE-MIT) · [LICENSE-APACHE](LICENSE-APACHE)
