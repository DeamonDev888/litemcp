<div align="center">

# litemcp

**Le framework MCP le plus rapide et le plus léger au monde — écrit en Rust.**

Stateless · socket bindé · **Streamable HTTP uniquement** (POST/SSE) · zéro runtime async

![Statut](https://img.shields.io/badge/statut-0.2.0%20publié-green) ![Licence](https://img.shields.io/badge/licence-MIT%20OR%20Apache--2.0-blue) ![MSRV](https://img.shields.io/badge/MSRV-1.85-orange) ![Spec](https://img.shields.io/badge/MCP-2026--07--28-blue)

![crates.io](https://img.shields.io/crates/v/litemcp.svg) ![CI](https://github.com/DeamonDev888/litemcp/actions/workflows/ci.yml/badge.svg) ![Docs](https://img.shields.io/docsrs/litemcp)

</div>

> [!IMPORTANT]
> **0.2.0 — publié sur [crates.io](https://crates.io/crates/litemcp).** Serveur Streamable HTTP stateless (socket bindé + POST → JSON/SSE) avec **schémas à la Zod** : `inputSchema` dérivé, arguments validés avant exécution, handlers typés (`tool_typed` / `tool_parse`), annotations MCP. 32 tests sur vraies sockets. Macros `#[tool]` visées en 0.3 ; conformance officielle MCP pour 1.0.

## Sommaire

- [Pourquoi litemcp](#pourquoi-litemcp)
- [Quand choisir litemcp plutôt que rmcp](#quand-choisir-litemcp-plutôt-que-rmcp)
- [Installation](#installation)
- [Démarrage rapide](#démarrage-rapide)
- [Tester](#tester)
- [Concepts](#concepts)
- [Protocole](#protocole)
- [Benchmarks](#benchmarks)
- [Tests](#tests)
- [FAQ](#faq)
- [Feuille de route](#feuille-de-route)
- [Licence](#licence)

## Pourquoi litemcp

Un serveur MCP en Rust, aujourd'hui, c'est un choix entre deux poids lourds : **rmcp**, l'officiel — excellent, mais il embarque tokio, hyper, tower et schemars — et **fastmcp_rust**, qui exige un nightly épinglé et un runtime maison. Les deux sont conçus pour des serveurs de production riches.

litemcp prend le problème par l'autre bout : **un serveur MCP qui ne coûte presque rien**.

- une CLI qui expose trois outils ;
- un test unitaire qui veut un vrai serveur, sans runtime ;
- un binaire embarqué (edge, IoT) où une pile HTTP complète, c'est non ;
- un agent qui spawn cinquante serveurs et paie le coût à chaque fois.

**litemcp traite le protocole comme une bibliothèque, pas comme un service.** Le cœur est une pompe JSON-RPC sur des threads bloquants du `std` — pas de tokio, pas de hyper, pas de schemars : rien que `serde`.

Les principes :

1. **Streamable HTTP uniquement.** Un seul transport : un socket TCP bindé ; chaque message est un `POST /mcp` auto-suffisant, la réponse arrive en `application/json` ou en flux SSE (`text/event-stream`, request-scoped). Pas de session, pas d'`initialize` obligatoire, pas de flux permanent. C'est la spec 2026-07-28, à la lettre.
2. **Zéro runtime async au cœur.** `std::net::TcpListener`, un thread par connexion. Un adaptateur tokio existera un jour — en feature optionnelle, jamais par défaut.
3. **Deux dépendances, pas une de plus.** `serde` et `serde_json`. Le JSON Schema sera dérivé à la maison (0.2).
4. **Mesuré ou rien.** Chaque release publie RSS, taille de binaire et latence p99 — avec rmcp mesuré à plat à côté. Jamais d'adjectif sans chiffre.

## Quand choisir litemcp plutôt que rmcp

| Votre besoin | Choix |
|---|---|
| Un serveur MCP dans une CLI, une TUI, un test | **litemcp** |
| Budget RAM serré (edge, IoT, embarqué) | **litemcp** |
| Démarrage à froid < 10 ms, binaire minimal | **litemcp** |
| Client MCP **local lancé en sous-processus** | rmcp |
| OAuth 2.1, tasks, elicitation, écosystème officiel | rmcp |
| Serveur de production riche, multi-features | rmcp |

rmcp est excellent — litemcp ne cherche pas à le remplacer partout. litemcp occupe le créneau que la pile officielle ne peut pas occuper sans casser sa compatibilité tokio : **le serveur MCP stateless, minuscule et mesuré**.

## Installation

```toml
[dependencies]
litemcp = "0.2"
```

Alternativement, directement depuis GitHub :

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

## Tester

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

## Concepts

### Outils — schéma à la Zod, comme fastmcp

L'API type : on déclare un schéma, l'`inputSchema` de `tools/list` en est **dérivé**, chaque `tools/call` est **validé** avant exécution et le handler reçoit des arguments **typés**. Un appel hors schéma ne touche jamais votre code : il reçoit `-32602` avec la liste des problèmes dans `error.data.issues` — l'équivalent d'une `ZodError`.

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

Trois registres, du plus rapide au plus confortable :

| API | Handler reçoit | `inputSchema` | Validation |
|---|---|---|---|
| `tool` | `&Value` brut | générique (`{"type":"object"}`) | à votre charge |
| `tool_typed` | `&Value` (déjà validé) | dérivé du schéma | automatique |
| `tool_parse` | struct `Deserialize` | dérivé du schéma | automatique |

Schémas : `object().field(..)/.opt(..)/.strict()`, `string().min/.max`, `integer().min/.max`, `number().min/.max`, `boolean()`, `enumeration(&[..])`, `array(s).min/.max`. Les contraintes deviennent `minLength`/`maxLength`/`minimum`/`maximum`/`minItems`/`maxItems`/`enum` dans le JSON Schema publié à `tools/list`.

### Annotations

Métadonnées de la spec 2026-07-28, publiées dans `tools/list` : `.title()`, `.read_only()`, `.destructive()`, `.idempotent()`, `.open_world()`. Un client peut ainsi savoir qu'un outil ne modifie rien avant de l'appeler.

### Erreurs

- **Hors schéma** → `-32602` avant exécution ; `error.data.issues` liste chaque problème (champ manquant, type, borne, champ inconnu) ;
- **Erreur d'outil** (le handler a tourné et a échoué) → `CallToolResult::isError` — le client affiche le message ;
- **Erreur de protocole** (mauvaise requête, outil inconnu, JSON invalide) → erreur JSON-RPC typée (-32700, -32602, -32601…), rendue de façon opaque.

La valeur de retour d'un outil devient `structuredContent` ; une `Err(msg)` devient `isError: true` avec le message en contenu texte.

La macro `#[tool]` (0.3) remplacera la déclaration par une simple fonction annotée, schéma dérivé du type Rust.

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

> Règle du projet : **jamais d'adjectif sans chiffre.** Les cibles ci-dessous sont des hypothèses de départ ; la première campagne (0.2) les ajuste une fois, puis elles sont figées comme SLO et republiées à chaque release, avec rmcp mesuré dans les mêmes conditions.

| Métrique | Cible initiale | Mesuré |
|---|---|---|
| RSS serveur hello-tool (HTTP, au repos) | < 2 Mo | — (0.2) |
| Binaire release stripped | < 1,5 Mo | — (0.2) |
| `tools/call` p99 local | < 1 ms | — (0.2) |
| Démarrage à froid | < 10 ms | — (0.2) |

## Tests

```bash
cargo test                          # 32 tests : unitaires + intégration (vraies sockets)
cargo clippy -- -D warnings         # CI stricte
```

## FAQ

**Pourquoi un seul transport ?**
Parce qu'un serveur lancé comme sous-processus d'un client meurt avec lui et n'accepte qu'un seul pair — c'est le modèle du client, pas du serveur. litemcp fait le choix inverse : **le serveur est un démon Streamable HTTP stateless**, bindé sur un socket TCP, que plusieurs agents peuvent joindre, qui survit aux clients et se déploie derrière nginx. Pour du MCP local en sous-processus, utilisez rmcp.

**Zod ? En Rust ?**
Le duo de fastmcp TypeScript — schéma déclaré, `inputSchema` dérivé, arguments validés avant `execute` — existe ici nativement : le module `zod` fait dérivation JSON Schema + validation runtime sans aucune dépendance de plus (schemars pèse plus lourd que le serveur lui-même). La macro `#[tool]` dérivera en plus le schéma du type Rust en 0.3.

**Pourquoi pas de tokio ?**
Un serveur MCP mono-outil n'a pas besoin d'un runtime async complet pour lire des requêtes et répondre du JSON. Le thread par connexion suffit jusqu'à des centaines de connexions — et l'adaptateur tokio restera disponible pour les autres cas.

**Pourquoi ne pas contribuer à rmcp ?**
rmcp est le SDK officiel : il doit couvrir toute la spec pour tout le monde, sur tokio, tous transports confondus. Cette contrainte impose son poids. litemcp explore la question inverse : que reste-t-il quand on retire tout ce qui n'est pas strictement nécessaire à un serveur HTTP stateless ?

**Pourquoi réécrire HTTP ?**
Le périmètre est volontairement minuscule : une route POST, `Content-Length`, keep-alive, un writer SSE mono-événement. Pas de chunked entrant, pas de TLS, pas de compression. Si le périmètre s'avère piégeux, le repli documenté est `tiny_http` — la décision sera tranchée par les tests, pas par le dogme.

**Quand sur crates.io ?**
Maintenant : `0.1.0` est publié comme socle. La mention « 1.0 » attendra la conformance officielle MCP verte.

## Feuille de route

| Phase | Contenu | État |
|---|---|---|
| 0 | Fondations (repo, Cargo, CI, licences) | ✅ |
| 1 | Noyau JSON-RPC + dispatch MCP | ✅ |
| 2 | Transport HTTP stateless (bind, POST/SSE) | ✅ |
| 3 | Durcissement HTTP (fuzz, timeouts, Inspector réel) | ⏳ |
| 4 | Schémas Zod + outils typés + annotations | ✅ (macros → 0.3) |
| 5 | Publication crates.io 0.1.0 | ✅ |
| 6 | Conformance officielle + benchmarks → 1.0 | ⏳ |

Le détail — jalons, SLO, risques, journal : [docs/ROADMAP.md](docs/ROADMAP.md).

## Licence

`MIT OR Apache-2.0` — [LICENSE-MIT](LICENSE-MIT) · [LICENSE-APACHE](LICENSE-APACHE)
