<div align="center">

# litemcp

**Le serveur MCP le plus léger de l'écosystème Rust.**

Stateless · socket bindé · **Streamable HTTP uniquement** (POST/SSE) · zéro runtime async

![Statut](https://img.shields.io/badge/statut-0.3.0%20publié-green) ![Licence](https://img.shields.io/badge/licence-MIT%20OR%20Apache--2.0-blue) ![MSRV](https://img.shields.io/badge/MSRV-1.85-orange) ![Spec](https://img.shields.io/badge/MCP-2026--07--28-blue)

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
litemcp = "0.3"
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

La valeur de retour d'un outil devient `structuredContent` — les objets JSON passent tels quels, toute autre valeur (scalaire, tableau, `null`) est enveloppée dans `{"result": …}` pour rester conforme à la spec ; une `Err(msg)` devient `isError: true` avec le message en contenu texte.

## Configuration

| Méthode | Défaut | Rôle |
|---|---|---|
| `.route(path)` | `/mcp` | chemin unique du serveur |
| `.allow_origins(&[..])` | vide | origines `Origin` autorisées (clients navigateur) — toute requête avec `Origin` non listée est refusée (garde DNS-rebinding) ; les clients CLI n'en envoient pas |
| `.read_timeout(duration)` | 30 s | timeout de lecture par opération (anti-slowloris) — un client trop lent est déconnecté sans réponse |
| `.max_connections(n)` | 256 (par serveur) | plafond de connexions simultanées — au-delà, `503` immédiat, pas de spawn |
| Corps de requête | 1 Mio | au-delà → `413` |
| En-têtes | 64 max, lignes 16 Kio max | au-delà → `431` |

## Protocole

| Comportement | Réponse |
|---|---|
| `POST /mcp` valide | `200 application/json` — réponse JSON-RPC |
| `POST` avec `Accept: text/event-stream` seul | `200 text/event-stream` — un événement `data:` (SSE request-scoped) |
| Notification (sans `id`) | `202 Accepted` sans corps |
| Sonde `GET /health` | `200` « ok » (sans JSON-RPC ; probes k8s/systemd) · autre méthode : `405` |
| `GET` sur `/mcp` | `405 Method Not Allowed` (pas de listener SSE permanent en stateless) |
| Version HTTP ≠ 1.0/1.1 · ligne de requête malformée | `505` · `400` |
| HTTP/1.1 sans `Host` (ou `Host` dupliqué) · ligne obs-fold | `400` |
| Route inconnue | `404` · `Content-Length` absent : `411` · malformé : `400` · corps > 1 Mio : `413` |
| En-tête `Origin` non autorisé | `403` (garde DNS-rebinding ; CLI sans `Origin` : libre) |

## Benchmarks

> Règle du projet : **jamais d'adjectif sans chiffre.** Première campagne mesurée (Windows 11, release, loopback) — les conditions seront figées et republiées à chaque release.

**Route maison vs backend alternatif `tiny_http`** (bench A/B exécuté le 2026-10-22, `examples/bench_backends`) :

| Métrique | Route maison | tiny_http | Verdict |
|---|---|---|---|
| `tools/call` keep-alive | **0,10–0,11 ms/req** | 0,19–0,20 ms/req | maison ~1,85× plus rapide |
| Connexions neuves | 1 750–2 000 conn/s | 2 080–2 180 conn/s | tiny +10–25 % |
| Coût binaire | — | +360 Ko (+80 %) | maison plus légère |
| Dépendances ajoutées | 0 | +4 (ascii, chunked_transfer, httpdate, log) | maison plus légère |

**Décision tranchée par les chiffres** : la route maison reste le seul backend — plus rapide en régime keep-alive (le cas d'un agent qui parle à ses serveurs), plus légère en deps et en binaire. Le backend alternatif a été **retiré du dépôt** : le crate garde exactement deux dépendances (`serde`, `serde_json`).

**Optimisations « léger & rapide »** (A/B dos à dos, même machine et même session, `examples/bench_backends`) :

| Métrique | 0.3.0 | Lot perf en cours | Verdict |
|---|---|---|---|
| `tools/call` keep-alive (moyenne) | 0,097–0,111 ms | **0,065–0,070 ms** | **~35 % plus rapide** |
| p50 / p99 keep-alive | 0,085–0,112 / 0,157–0,166 ms | **0,061 / 0,095–0,123 ms** | plus serré |
| Binaire release stripped (hello) | 483 Ko | **385 Ko** | **−18,5 %** (LTO fat + `codegen-units = 1`) |
| Mémoire de lecture par connexion | 8 Ko | **4 Ko** | 1 Mio au plafond de 256 connexions |

Les leviers : réponse HTTP assemblée dans un seul buffer (**un seul write** par réponse — avant : deux syscalls, deux paquets avec nodelay), parsing d'en-têtes **zéro allocation** (`Origin`/`Host`/`Accept`/`Connection` réduits à des verdicts, sans chaînes ni `Vec`), buffers de lecture réutilisés sur toute la connexion keep-alive, **zéro clone des arguments** dans le dispatch (`params` déplacé hors du JSON parsé, handler et validation travaillent par référence), `route`/`allow_origins` partagés par `Arc`. Dépendances inchangées : toujours exactement deux (`serde`, `serde_json`).

**SLO (conditions : Windows 11, release, loopback — mesures Linux/`VmRSS` à la prochaine campagne) :**

| Métrique | Cible | Mesuré |
|---|---|---|
| Binaire release stripped (hello) | < 1,5 Mo | **385 Ko** ✅ |
| `tools/call` keep-alive (moyenne) | < 1 ms | **0,065–0,070 ms** ✅ (p99 < 0,13 ms, bruit Windows ; campagne figée Linux à venir) |
| RSS serveur hello (au repos) | < 2 Mo | 4,7 Mo (WorkingSet Windows, DLL partagées incluses — mesure `VmRSS` Linux à venir) |
| Démarrage à froid | < 10 ms | ~130 ms via shell Windows (overhead de spawn inclus ; mesure in-process à venir) |

## Comparatif — ViteMCP (TypeScript)

[`@vitemcp/server`](https://github.com/vitemcp/server) est le framework TS qui cible la **même spec** stateless 2026-07-28 — le comparatif naturel. Même serveur démo (4 outils identiques) des deux côtés, **un seul client de bench neutre** (Rust, `litemcp-it/examples/bench_remote`), Windows 11, loopback, passes alternées (2026-10-22) :

| Métrique | **litemcp** | @vitemcp/server | Verdict |
|---|---|---|---|
| keep-alive (moyenne) | **0,09–0,10 ms** | 1,43–1,73 ms | **~15–18× plus rapide** |
| p99 keep-alive | **0,18–0,22 ms** | 5,4–5,9 ms | ~25× plus resserré |
| Connexions réutilisées | **2000/2000** | 0/2000 | flux SSE maintenu ouvert côté ViteMCP → reconnect à chaque appel |
| Nouvelles connexions acceptées **(connexions/seconde)** | **848–1257 connexions/s** | 354–498 connexions/s | **~2–3× plus de nouveaux clients par seconde** |
| Mémoire après 2 300 requêtes | **5,5 Mo** (0,9 Mo privé, reste = DLL partagées) | ~417 Mo (3 processus Node : 47 + 63 + 306) | **~75× moins de mémoire** |

L'écart structurel : litemcp délimite ses réponses par `Content-Length` et clôt le flux après l'événement — la connexion TCP reste proprement réutilisable ; ViteMCP répond en **SSE encadré** qu'il maintient ouvert, le client doit donc rouvrir une connexion par appel séquentiel. (Conditions : tsx/Node 24 côté TS ; les deux serveurs complétaient les mêmes requêtes JSON-RPC.)

**Points forts de ViteMCP** — bilan :

- **Adopté** : sonde `GET /health` (`200` « ok », compatible keep-alive) ;
- **Candidat 0.4** : `outputSchema` (validation de la sortie structurée) ;
- **Écartés volontairement** (hors périmètre minimaliste, réévalués si besoin) : OAuth 2.1 intégré, OpenAPI→outils, routes HTTP personnalisées, edge runtime, CLI dev/inspect, élaboration/sampling bidirectionnels.

## Tests

```bash
cargo test                          # 70 tests : unitaires + intégration (vraies sockets) + doctests
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
| 3 | Durcissement HTTP (timeouts, limites, 100-continue, keep-alive) | ✅ (+ audit complet : Host/505/obs-fold/CL malformé, plafond d'issues ; fuzz + Inspector → 0.4) |
| 4 | Schémas Zod + outils typés + annotations | ✅ (macros → 0.4) |
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
Le périmètre est volontairement minuscule : une route POST, `Content-Length`, keep-alive, un writer SSE mono-événement. Pas de chunked entrant, pas de TLS, pas de compression. Ce choix est **tranché par les tests** : le bench maison contre le backend alternatif testé (tiny_http) montre la route maison ~1,85× plus rapide en keep-alive et 0 dépendance de plus (voir [Benchmarks](#benchmarks)) — et elle embarque son durcissement (timeouts, limites d'en-têtes, `Expect: 100-continue`).

**Comment sont choisies les versions ?**
Pacement volontaire : toute modification atterrit d'abord dans la section `[Unreleased]` du [CHANGELOG](CHANGELOG.md) ; une version n'est coupée que pour un lot cohérent (fonction, correctif, doc majeure). Le `0.x` court jusqu'à la conformance officielle MCP verte ; la mention « 1.0 » attend ce jalon.

## Licence

`MIT OR Apache-2.0` — [LICENSE-MIT](LICENSE-MIT) · [LICENSE-APACHE](LICENSE-APACHE)
