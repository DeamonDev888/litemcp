<div align="center">

# litemcp

**Le serveur MCP le plus léger de l'écosystème Rust.**

Stateless · socket bindé · **Streamable HTTP uniquement** (POST/SSE) · zéro runtime async

![Statut](https://img.shields.io/badge/statut-0.2.0%20publié-green) ![Licence](https://img.shields.io/badge/licence-MIT%20OR%20Apache--2.0-blue) ![MSRV](https://img.shields.io/badge/MSRV-1.85-orange) ![Spec](https://img.shields.io/badge/MCP-2026--07--28-blue)

![crates.io](https://img.shields.io/crates/v/litemcp.svg) ![CI](https://img.shields.io/github/actions/workflow/status/DeamonDev888/litemcp/ci.yml?label=CI) ![Docs](https://img.shields.io/docsrs/litemcp)

[Installation](#installation) · [Démarrage rapide](#démarrage-rapide) · [Concepts](#concepts) · [Protocole](#protocole) · [FAQ](#faq)

</div>

---

## Pourquoi litemcp

**litemcp traite le protocole MCP comme une bibliothèque, pas comme un service.** Le cœur est une pompe JSON-RPC sur des threads bloquants du `std` — pas de runtime async, pas de pile HTTP lourde : rien que `serde`.

- une CLI qui expose trois outils ;
- un test unitaire qui veut un vrai serveur, sans runtime ;
- un binaire embarqué (edge, IoT) où une pile HTTP complète, c'est non ;
- un agent qui spawn cinquante serveurs et paie le coût à chaque fois.

Les principes :

1. **Streamable HTTP uniquement.** Un seul transport : un socket TCP bindé ; chaque message est un `POST /mcp` auto-suffisant, la réponse arrive en `application/json` ou en flux SSE (`text/event-stream`, request-scoped). Pas de session, pas d'`initialize` obligatoire, pas de flux permanent. C'est la spec 2026-07-28, à la lettre.
2. **Zéro runtime async au cœur.** `std::net::TcpListener`, un thread par connexion. Un adaptateur tokio existera un jour — en feature optionnelle, jamais par défaut.
3. **Deux dépendances, pas une de plus.** `serde` et `serde_json` — la dérivation JSON Schema et la validation sont faites maison, dans le module [`zod`](#outils--schémas-à-la-zod).
4. **Mesuré ou rien.** Chaque release publie RSS, taille de binaire et latence p99. Jamais d'adjectif sans chiffre.

## Fonctions phares

| | |
|---|---|
| ⚡ **Minuscule** | 2 dépendances, binaire minimal, démarrage instantané |
| 🚀 **Sans runtime** | threads `std`, un par connexion — pas de tokio au cœur |
| 🧩 **Schémas à la Zod** | `inputSchema` dérivé du schéma, publié dans `tools/list` |
| ✅ **Validation avant exécution** | un appel hors schéma ne touche jamais votre handler |
| 🦀 **Handlers typés** | arguments désérialisés dans vos structs `Deserialize` |
| 🛡️ **Sûr par défaut** | `#![forbid(unsafe_code)]`, garde DNS-rebinding, corps limité |

## Installation

```toml
[dependencies]
litemcp = "0.2"
```

Ou directement depuis GitHub :

```toml
[dependencies]
litemcp = { git = "https://github.com/DeamonDev888/litemcp" }
```

## Démarrage rapide

```rust
use litemcp::prelude::*;

fn main() -> std::io::Result<()> {
    ServerBuilder::new("calc", "1.0.0")
        .tool("add", "Additionne deux nombres", |args| {
            let a = args["a"].as_i64().ok_or("a manquant")?;
            let b = args["b"].as_i64().ok_or("b manquant")?;
            Ok(serde_json::json!({ "somme": a + b }))
        })
        .bind("127.0.0.1:8080")?   // socket bindé, un thread par connexion
        .serve()                    // bloque : POST /mcp → JSON ou SSE
}
```

C'est tout — votre serveur MCP écoute. Aucune session, aucun handshake, aucun runtime.

Vérifiez-le avec curl :

```bash
curl -s http://127.0.0.1:8080/mcp \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"add","arguments":{"a":5,"b":7}}}'
```

```json
{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"{\"somme\":12}"}],"structuredContent":{"somme":12}}}
```

Ou avec MCP Inspector en mode URL : `npx @modelcontextprotocol/inspector` → `http://127.0.0.1:8080/mcp`.

Un exemple complet (les trois registres d'outils + annotations) : `cargo run --example hello`.

## Concepts

### Outils — schémas à la Zod

On déclare un schéma, l'`inputSchema` de `tools/list` en est **dérivé**, chaque `tools/call` est **validé** avant exécution et le handler reçoit des arguments **typés**. Un appel hors schéma ne touche jamais votre code : il reçoit `-32602` avec la liste des problèmes dans `error.data.issues`.

```rust
use litemcp::prelude::*;
use serde::Deserialize;

#[derive(Deserialize)]
struct SearchArgs {
    query: String,       // requis
    limit: Option<u32>,  // optionnel
}

ServerBuilder::new("recherche", "1.0.0")
    .tool_parse(
        "search",
        "Recherche indexée",
        zod::object()
            .field("query", zod::string().min(2).max(64))
            .opt("limit", zod::integer().min(1).max(100))
            .strict(), // refuse les champs inconnus
        |args: SearchArgs| {
            Ok(serde_json::json!({ "hits": search(&args.query, args.limit) }))
        },
    )
    .annotate("search", ToolAnnotations::new().title("Recherche").read_only(true))
    .bind("127.0.0.1:8080")?
    .serve()?;
```

Trois registres, du plus direct au plus confortable :

| API | Handler reçoit | `inputSchema` | Validation |
|---|---|---|---|
| `tool` | `&Value` brut | générique (`{"type":"object"}`) | à votre charge |
| `tool_typed` | `&Value` (déjà validé) | dérivé du schéma | automatique |
| `tool_parse` | struct `Deserialize` | dérivé du schéma | automatique |

Schémas disponibles : `object().field(..)/.opt(..)/.strict()`, `string().min/.max`, `integer().min/.max`, `number().min/.max`, `boolean()`, `enumeration(&[..])`, `array(s).min/.max`. Les contraintes deviennent `minLength`/`maxLength`/`minimum`/`maximum`/`minItems`/`maxItems`/`enum` dans le JSON Schema publié à `tools/list`.

### Annotations

Métadonnées de la spec 2026-07-28, publiées dans `tools/list` : `.title()`, `.read_only()`, `.destructive()`, `.idempotent()`, `.open_world()`. Un client sait ainsi qu'un outil ne modifie rien avant de l'appeler.

### Erreurs

- **Hors schéma** → `-32602` avant exécution ; `error.data.issues` liste chaque problème (champ manquant, type, borne, champ inconnu) ;
- **Erreur d'outil** (le handler a tourné et a échoué) → `CallToolResult::isError` — le client affiche le message ;
- **Erreur de protocole** (mauvaise requête, outil inconnu, JSON invalide) → erreur JSON-RPC typée (-32700, -32602, -32601…), rendue de façon opaque.

La valeur de retour d'un outil devient `structuredContent` ; une `Err(msg)` devient `isError: true` avec le message en contenu texte.

## Configuration

| Méthode | Défaut | Rôle |
|---|---|---|
| `.route(path)` | `/mcp` | chemin unique du serveur |
| `.allow_origins(&[..])` | vide | origines `Origin` autorisées (clients navigateur) — toute requête avec `Origin` non listée est refusée (garde DNS-rebinding) ; les clients CLI n'en envoient pas |
| Corps de requête | 1 Mio | au-delà → `413` |

## Protocole

| Comportement | Réponse |
|---|---|
| `POST /mcp` valide | `200 application/json` — réponse JSON-RPC |
| `POST` avec `Accept: text/event-stream` seul | `200 text/event-stream` — un événement `data:` (SSE request-scoped) |
| Notification (sans `id`) | `202 Accepted` sans corps |
| `GET` | `405 Method Not Allowed` (pas de listener SSE permanent en stateless) |
| Route inconnue | `404` · `Content-Length` absent : `411` · corps > 1 Mio : `413` |
| En-tête `Origin` non autorisé | `403` (garde DNS-rebinding ; CLI sans `Origin` : libre) |

## Benchmarks

> Règle du projet : **jamais d'adjectif sans chiffre.** Les cibles ci-dessous sont des hypothèses de départ ; la première campagne les ajuste une fois, puis elles sont figées comme SLO et republiées à chaque release.

| Métrique | Cible initiale | Mesuré |
|---|---|---|
| RSS serveur hello-tool (HTTP, au repos) | < 2 Mo | — (à venir) |
| Binaire release stripped | < 1,5 Mo | — (à venir) |
| `tools/call` p99 local | < 1 ms | — (à venir) |
| Démarrage à froid | < 10 ms | — (à venir) |

## Tests

```bash
cargo test                          # 32 tests : unitaires + intégration (vraies sockets)
cargo clippy -- -D warnings         # CI stricte
```

## Journal des versions

L'historique complet et détaillé vit dans [CHANGELOG.md](CHANGELOG.md) — section **[Unreleased]** en tête : chaque modification y est consignée au fil de l'eau, et une version n'est coupée que pour un lot cohérent (voir la [politique de versioning](CHANGELOG.md#politique-de-versioning)).

## Feuille de route

| Phase | Contenu | État |
|---|---|---|
| 0 | Fondations (repo, Cargo, CI, licences) | ✅ |
| 1 | Noyau JSON-RPC + dispatch MCP | ✅ |
| 2 | Transport HTTP stateless (bind, POST/SSE) | ✅ |
| 3 | Durcissement HTTP (fuzz, timeouts, Inspector réel) | ⏳ |
| 4 | Schémas Zod + outils typés + annotations | ✅ (macros → 0.3) |
| 5 | Publication crates.io | ✅ |
| 6 | Conformance officielle + benchmarks → 1.0 | ⏳ |

Le détail — jalons, SLO, risques, journal : [docs/ROADMAP.md](docs/ROADMAP.md).

## FAQ

**Pourquoi un seul transport ?**
Parce qu'un serveur lancé comme sous-processus d'un client meurt avec lui et n'accepte qu'un seul pair — c'est le modèle du client, pas du serveur. litemcp fait le choix inverse : **le serveur est un démon Streamable HTTP stateless**, bindé sur un socket TCP, que plusieurs agents peuvent joindre, qui survit aux clients et se déploie derrière nginx. Le MCP local en sous-processus est hors périmètre, par conception.

**Pourquoi un module de schémas intégré ?**
Un serveur MCP sans schéma n'est qu'un routeur JSON : les clients ont besoin de l'`inputSchema` pour présenter vos outils, et vos handlers ont besoin d'arguments garantis. Le module `zod` couvre les deux — dérivation JSON Schema + validation runtime — sans dépendance de plus : il pèse moins que les bibliothèques de schémas génériques qu'on embarquerait sinon.

**Pourquoi pas de tokio ?**
Un serveur MCP mono-outil n'a pas besoin d'un runtime async complet pour lire des requêtes et répondre du JSON. Le thread par connexion suffit jusqu'à des centaines de connexions — et l'adaptateur tokio restera disponible pour les autres cas.

**Pourquoi réécrire HTTP ?**
Le périmètre est volontairement minuscule : une route POST, `Content-Length`, keep-alive, un writer SSE mono-événement. Pas de chunked entrant, pas de TLS, pas de compression. Si le périmètre s'avère piégeux, le repli est documenté — la décision sera tranchée par les tests, pas par le dogme.

**Comment sont choisies les versions ?**
Pacement volontaire : toute modification atterrit d'abord dans la section `[Unreleased]` du [CHANGELOG](CHANGELOG.md) ; une version n'est coupée que pour un lot cohérent (fonction, correctif, doc majeure). Le `0.x` court jusqu'à la conformance officielle MCP verte ; la mention « 1.0 » attend ce jalon.

## Licence

`MIT OR Apache-2.0` — [LICENSE-MIT](LICENSE-MIT) · [LICENSE-APACHE](LICENSE-APACHE)
