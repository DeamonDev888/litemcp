<div align="center">

# litemcp

**La lib MCP la plus légère de l'écosystème Rust : votre serveur MCP en quelques lignes.**

Stateless · socket bindé · **Streamable HTTP uniquement** (POST/SSE) · zéro runtime async

![Statut](https://img.shields.io/badge/statut-0.3.3%20publié-green) ![Licence](https://img.shields.io/badge/licence-MIT%20OR%20Apache--2.0-blue) ![MSRV](https://img.shields.io/badge/MSRV-1.85-orange) ![Spec](https://img.shields.io/badge/MCP-2026--07--28-blue)

![crates.io](https://img.shields.io/crates/v/litemcp.svg) ![CI](https://img.shields.io/github/actions/workflow/status/DeamonDev888/litemcp/ci.yml?label=CI) ![Docs](https://img.shields.io/docsrs/litemcp)

[Installation](#installation) · [Démarrage rapide](#démarrage-rapide) · [Concepts](#concepts) · [Configuration](#configuration) · [Sécurité](#sécurité) · [Protocole](#protocole) · [FAQ](#faq)

</div>

---

## Pourquoi litemcp

La plupart des serveurs MCP sont des **services** : un programme qui s'installe, charge un gros framework et tourne à côté de votre application. **litemcp fait l'inverse : c'est une bibliothèque Rust.** Vous écrivez quelques lignes, vous appelez `.serve()`, et un vrai serveur MCP tourne **dans** votre programme — avec les threads standards du langage, sans runtime async, sans framework HTTP. Seule dépendance pour le JSON : `serde`.

**Pour qui ?**

- **Une CLI** qui veut exposer deux ou trois outils, sans embarquer un framework web ;
- **Un test unitaire** qui a besoin d'un vrai serveur MCP qui écoute sur un port — sans installer tokio ;
- **Un binaire embarqué** (edge, IoT), où chaque mégaoctet et chaque dépendance compte ;
- **Un agent** qui lance cinquante serveurs MCP en même temps : chaque mégaoctet de RAM et chaque demi-seconde de démarrage se multiplie par cinquante.

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
| 🧩 **Schémas à la Zod** | `inputSchema` dérivé du schéma, publié dans `tools/list` ; `outputSchema` **exécuteur** (sortie validée avant envoi) |
| ✅ **Validation avant exécution** | un appel hors schéma ne touche jamais votre handler |
| 🦀 **Handlers typés** | arguments désérialisés dans vos structs `Deserialize` ; macro `#[tool]` (feature `macros`) qui dérive le schéma des types des paramètres |
| 📚 **Resources & Prompts** | `resources/list`·`read`, `prompts/list`·`get` — mêmes indices de cache que `tools/list` (SEP-2549) |
| ⏱️ **Handlers bornés** | `.tool_timeout(duration)` : un handler qui dépasse rend une erreur, la connexion n'est plus retenue indéfiniment |
| 🛑 **Arrêt propre** | `bound.handle()` puis `handle.shutdown()` : `serve()` rend `Ok(())`, les connexions ferment entre deux requêtes |
| 🛡️ **Sûr par défaut** | `#![forbid(unsafe_code)]`, garde DNS-rebinding, corps limité |

## Installation

```toml
[dependencies]
litemcp = "0.4"

# pour la macro #[tool] (deps de compilation seulement) :
# litemcp = { version = "0.3", features = ["macros"] }
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

### Macros `#[tool]` · `#[resource]` · `#[prompt]` (feature `macros`)

Le schéma est **dérivé des types des paramètres** d'une fonction ordinaire — plus de double déclaration schéma/struct. Chaque macro génère `register_<fonction>`, à enchaîner avec `.with_tool(…)` :

```rust
use litemcp::macros::{prompt, resource, tool}; // features = ["macros"]

#[tool(name = "add", description = "Additionne deux entiers")]
fn add(a: i64, b: i64) -> litemcp::ToolResult {
    Ok(serde_json::json!({ "somme": a + b }))
}

#[resource(uri = "mem://doc", name = "Doc", description = "La documentation")]
fn doc() -> Result<litemcp::protocol::ResourceContents, String> {
    Ok(litemcp::protocol::ResourceContents {
        uri: "mem://doc".into(),
        mime_type: Some("text/markdown".into()),
        text: include_str!("../README.md").into(),
    })
}

#[prompt(description = "Demande une revue de code")] // `name` optionnel (défaut : nom de la fonction)
fn revue(fichier: String, severite: Option<String>) -> Result<Vec<litemcp::protocol::PromptMessage>, String> {
    Ok(vec![litemcp::protocol::PromptMessage::user(format!("Relis {fichier}"))])
}

ServerBuilder::new("calc", "1.0.0")
    .with_tool(register_add)
    .with_tool(register_doc)
    .with_tool(register_revue)
    .bind("127.0.0.1:8080")?
    .serve()?;
```

`#[tool]` et `#[prompt]` valident les arguments via le schéma dérivé (`#[prompt]` passe par `.prompt_typed(…)`, la variante validée de `.prompt()`) ; `#[resource]` prend une fonction **sans paramètre** (`uri` requise). Types supportés : entiers, `f32`/`f64`, `bool`, `String`, `Option<T>` (champ optionnel), `Vec<T>`. Le cœur du crate reste à **2 dépendances** — `syn`/`quote` ne sont que des deps de compilation, activées seulement par la feature.

### Resources & Prompts

Mêmes registres que les outils, mêmes indices de cache (`ttlMs`/`cacheScope`, SEP-2549) :

```rust
use litemcp::prelude::*;

ServerBuilder::new("doc", "1.0.0")
    .resource("mem://readme", "README", "La documentation du service", || {
        Ok(litemcp::protocol::ResourceContents {
            uri: "mem://readme".into(),
            mime_type: Some("text/markdown".into()),
            text: include_str!("../README.md").into(),
        })
    })
    .prompt("revue", "Demande une revue de code", |args| {
        let fichier = args.get("fichier").and_then(|v| v.as_str()).unwrap_or("src/main.rs");
        Ok(vec![PromptMessage::user(format!("Relis {fichier} et liste les risques."))])
    })
    .bind("127.0.0.1:8080")?
    .serve()?;
```

`resources/read` prend `{"uri": …}` ; `prompts/get` prend `{"name": …, "arguments": {…}}` (objet exigé, sinon `-32602`). Les capacités `resources`/`prompts` sont annoncées dans `server/discover` seulement quand les registres ne sont pas vides.

### Annotations

Métadonnées de la spec 2026-07-28, publiées dans `tools/list` : `.title()`, `.read_only()`, `.destructive()`, `.idempotent()`, `.open_world()`. Un client sait ainsi qu'un outil ne modifie rien avant de l'appeler.

### Erreurs

- **Hors schéma** → `-32602` avant exécution ; `error.data.issues` liste chaque problème (champ manquant, type, borne, champ inconnu) ;
- **Erreur d'outil** (le handler a tourné et a échoué) → `CallToolResult::isError` — le client affiche le message ;
- **Erreur de protocole** (mauvaise requête, outil inconnu, JSON invalide) → erreur JSON-RPC typée (-32700, -32602, -32601…), rendue de façon opaque.

La valeur de retour d'un outil devient `structuredContent` — les objets JSON passent tels quels, toute autre valeur (scalaire, tableau, `null`) est enveloppée dans `{"result": …}` (**choix d'API** : `structuredContent` toujours objet, contrat simple pour les clients) ; une `Err(msg)` devient `isError: true` avec le message en contenu texte. Un `outputSchema` attaché via `.output_schema("outil", schéma)` valide cette sortie **avant envoi** : non conforme → résultat `isError`.

## Configuration

| Méthode | Défaut | Rôle |
|---|---|---|
| `.route(path)` | `/mcp` | chemin unique du serveur — **validé au bind** : doit commencer par `/`, sans query, et ne pas masquer `/health` (sinon erreur `InvalidInput`, pas de serveur silencieusement injoignable) |
| `.allow_origins(&[..])` | vide | origines `Origin` autorisées (clients navigateur) — toute requête avec `Origin` non listée est refusée (garde DNS-rebinding) ; les clients CLI n'en envoient pas. Format : l'origine **normalisée navigateur** — schéma + hôte en minuscules, **sans port par défaut** (un navigateur écrit `https://a.example`, jamais `https://a.example:443`) ; comparaison exacte insensible à la casse |
| `.read_timeout(duration)` | 30 s | timeout de lecture par opération (anti-slowloris) — un client trop lent est déconnecté sans réponse |
| `.max_connections(n)` | 256 (par serveur) | plafond de connexions simultanées — au-delà, `503` immédiat, pas de spawn |
| `.connection_ttl(duration)` | 5 min | durée de vie maximale d'une connexion keep-alive — borne le temps d'occupation d'un slot (sans elle, un client qui drip-feed des requêtes valides retient son slot indéfiniment) ; `0` désactive le TTL |
| `.tool_timeout(duration)` | illimité | délai maximal d'exécution des handlers (outils, ressources, prompts) — au-delà : résultat `isError` pour un outil, `-32000` pour une ressource/prompt ; les arguments d'un `tools/call` sont clonés une fois par appel quand il est actif |
| `bound.handle()` → `ServerHandle` | — | poignée d'arrêt cloneable : `handle.shutdown()` rend `serve()` → `Ok(())`, les connexions ferment entre deux requêtes (une lecture bloquée en cours ferme au `read_timeout`) |
| Sonde `GET /health` | `/health` (inclus) | `200` « ok » sans JSON-RPC — probes k8s/systemd/équilibreur (`HEAD` accepté, sans corps) |
| Corps de requête | 1 Mio | au-delà → `413` — lu **par tranches** (buffer 64 Ko) : la mémoire engagée est proportionnelle aux octets réellement reçus |
| En-têtes | 64 max, lignes 16 Kio max | au-delà → `431` |

## Sécurité

**Modèle de menace assumé : litemcp est une bibliothèque réseau, pas un service exposé.** Quiconque peut joindre le socket peut appeler vos outils — il n'y a ni authentification ni TLS intégrés (choix architectural : voir le comparatif ViteMCP, « OAuth 2.1 intégré » est écarté volontairement). Le durcissement intégré couvre le protocole ; la périphérie est à votre charge :

- **Liaison** : bind `127.0.0.1:8080` (comme dans tous les exemples) si le serveur ne parle qu'à des processus locaux ; sinon, exposez derrière un **reverse proxy** qui apporte TLS, authentification (par ex. OAuth 2.1) et rate-limiting **par IP** — le plafond de connexions de litemcp est **global au serveur** (256 par défaut) : 256 clients lents peuvent l'occuper et renvoyer `503` aux autres, c'est à ça que sert le quota par IP du proxy (le TTL de connexion de 5 min borne par ailleurs la durée d'occupation d'un slot) ;
- **Navigateurs** : garde DNS-rebinding intégrée — toute requête portant un `Origin` non listé dans `.allow_origins` est refusée (`403`), et les préflights CORS échouent par défaut (`OPTIONS` → `405`) ;
- **Handlers bloquants** : par défaut il n'y a pas de timeout d'exécution — un handler qui bloque retient son thread et son slot de connexion. `.tool_timeout(duration)` borne la **réponse** (résultat `isError`/`-32000` passé le délai) mais ne peut pas tuer le thread du handler (Rust n'a pas d'annulation de thread) : il termine en arrière-plan et son résultat est ignoré. Pour des effets de bord annulables, encadrez vos appels lents vous-même ;
- **Validation** : privilégiez `tool_typed`/`tool_parse` (schéma appliqué **avant** votre code) ; `tool` brut vous laisse la validation des arguments — c'est là que naissent les bugs d'injection côté handler, pas dans le transport. Le schéma publié est **exécuteur** : ce qu'il annonce est exactement ce que la validation applique.

Le durcissement transport est intégré et vérifié par la suite de tests : limites (corps 1 Mio, 64 en-têtes, lignes 16 Kio, 256 connexions, TTL 5 min), timeouts (lecture 30 s, deadline 60 s — slowloris coupé), anti request-smuggling (`Transfer-Encoding` ou `Content-Length` dupliqué → `400` sur **toutes** les routes, corps consommé partout pour éviter toute désynchronisation de file), parsing strict (ligne d'en-tête sans `:` ou espace avant `:` → `400`, séparateurs SP uniquement, CRLF strict, version sensible à la casse, `Accept`/`Expect` comparés par jeton exact), `Host` requis, obs-fold refusé, en-têtes à valeur unique **dupliqués** (`Origin`, `Content-Type`, `Mcp-Method`, `Mcp-Name`) refusés (`400` — le verdict ne doit pas dépendre de l'ordre), `Content-Type` vérifié (`415`), paniques de handlers contenues (`-32603` **avec l'`id` de la requête**, le serveur continue). Le transport a fait l'objet d'un audit complet (statique + dynamique sur serveur réel) ; les correctifs qui en découlent sont consignés dans le [CHANGELOG](CHANGELOG.md).

## Protocole

**Spec MCP `2026-07-28`** (stateless, sans handshake) — implémentée intégralement :

| Comportement | Réponse |
|---|---|
| `POST /mcp` valide | `200 application/json` — réponse JSON-RPC |
| `POST` avec `Accept: text/event-stream` seul | `200 text/event-stream` — un événement `data:` (SSE request-scoped) + en-têtes anti-tampon |
| Notification (sans `id`) | `202 Accepted` sans corps |
| `server/discover` (SEP-2575) | `200` — versions supportées, capacités, `serverInfo` dans `_meta` |
| Version de protocole dans `_meta` | `2026-07-28` acceptée · version inconnue : `-32022` (`UnsupportedProtocolVersionError`) · **absence tolérée** (compatibilité `2025-11-25`) |
| `resultType` (SEP-2322) | présent dans **chaque** résultat (`"complete"`) |
| `serverInfo` (SEP-2575) | le serveur s'identifie sous `_meta` de **chaque** résultat |
| `tools/list` — `ttlMs` / `cacheScope` (SEP-2549) | `60000` / `"private"` |
| `outputSchema` | publié dans `tools/list` **et exécuteur** : une sortie `structuredContent` non conforme devient un résultat `isError` (`.output_schema("outil", schéma)`) |
| `resources/list` / `resources/read` | `200` — catalogue et lecture, `ttlMs`/`cacheScope` comme `tools/list` (SEP-2549) ; uri inconnue/manquante : `-32602` ; handler en échec : `-32000` |
| `prompts/list` / `prompts/get` | `200` — catalogue et rendu (messages `user`/`assistant`) ; nom inconnu : `-32602` ; `arguments` non-objet : `-32602` |
| Capacités `resources` / `prompts` | annoncées dans `server/discover`/`initialize` **seulement** quand leur registre n'est pas vide |
| `Mcp-Method` / `Mcp-Name` (SEP-2243) | acceptés ; **incohérents avec le corps** → `-32020` (`HeaderMismatch`) |
| `initialize` / `ping` | tolérés (méthodes legacy `2025-11-25`, retirées de la spec `2026-07-28`) |
| Sonde `GET /health` | `200` « ok » (sans JSON-RPC ; probes k8s/systemd) · `HEAD /health` : mêmes en-têtes, sans corps · **un corps annoncé est refusé** (`400`, la sonde n'en accepte aucun) · autre méthode : `405` (`Allow: GET, HEAD`) |
| `GET` sur `/mcp` | `405 Method Not Allowed` (pas de listener SSE permanent en stateless) |
| Version HTTP ≠ `HTTP/1.0`/`HTTP/1.1` exactes (sensible à la casse) · ligne de requête malformée | `505` · `400` |
| HTTP/1.1 sans `Host` (ou `Host` dupliqué) · ligne obs-fold · en-tête sans `:` · espace avant `:` · HTAB dans la ligne de requête · fin de ligne LF seul | `400` |
| Route inconnue | `404` · `Content-Length` absent (POST) : `411` · malformé : `400` · corps > 1 Mio : `413` |
| `Content-Type` ≠ `application/json` | `415 Unsupported Media Type` (absent : accepté) |
| `Transfer-Encoding` présent (toute route) · `Content-Length` dupliqué | `400` (anti request-smuggling) |
| `Content-Type` / `Mcp-Method` / `Mcp-Name` dupliqués | `400` (en-têtes à valeur unique — un proxy combine ou retient la première valeur : le verdict ne doit pas dépendre de l'ordre) |
| En-tête `Origin` non autorisé | `403` (garde DNS-rebinding ; CLI sans `Origin` : libre) |
| Toute erreur JSON-RPC | l'`id` de la requête est **repris** dans la réponse (JSON-RPC 2.0 §5) |

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

[`@vitemcp/server`](https://github.com/vitemcp/server) est le framework TS qui cible la **même spec** stateless 2026-07-28 — le comparatif naturel. Même serveur démo (4 outils identiques) des deux côtés, **un seul client de bench neutre** (Rust, `litemcp-it/examples/bench_remote`), Windows 11, loopback, passes alternées (2026-10-22).

**Résumé express :**

| | litemcp | ViteMCP (TS) |
|---|---|---|
| ⚡ Temps de réponse moyen | **0,09 ms** | 1,43 ms |
| 😰 Le pire cas (p99) | **0,18 ms** | 5,4 ms |
| 🔌 Connexions réutilisées | **2000/2000** | 0/2000 |
| 📊 Nouvelles connexions par seconde | **~1000** | ~400 |
| 🧠 Mémoire après 2300 requêtes | **5,5 Mo** | ~417 Mo |

Détail complet (plages, passes alternées) :

| Métrique | **litemcp** | @vitemcp/server | Verdict |
|---|---|---|---|
| keep-alive (moyenne) | **0,09–0,10 ms** | 1,43–1,73 ms | **~15–18× plus rapide** |
| p99 keep-alive | **0,18–0,22 ms** | 5,4–5,9 ms | ~25× plus resserré |
| Connexions réutilisées | **2000/2000** | 0/2000 | flux SSE maintenu ouvert côté ViteMCP → reconnect à chaque appel |
| Nouvelles connexions acceptées **(connexions/seconde)** | **848–1257 connexions/s** | 354–498 connexions/s | **~2–3× plus de nouveaux clients par seconde** |
| Mémoire après 2 300 requêtes | **5,5 Mo** (0,9 Mo privé, reste = DLL partagées) | ~417 Mo (3 processus Node : 47 + 63 + 306) | **~75× moins de mémoire** |

L'écart structurel : litemcp délimite ses réponses par `Content-Length` et clôt le flux après l'événement — la connexion TCP reste proprement réutilisable ; ViteMCP répond en **SSE encadré** qu'il maintient ouvert, le client doit donc rouvrir une connexion par appel séquentiel. (Conditions : tsx/Node 24 côté TS ; les deux serveurs complétaient les mêmes requêtes JSON-RPC.)

## Hébergement VPS & proxies (le piège du tamponnage SSE)

Derrière beaucoup d'hébergements, un proxy (nginx, Cloudflare…) met les réponses en tampon et n'envoie les flux SSE que lorsqu'il a accumulé ~8 Ko — les événements « s'empilent ». Deux lignes de défense :

- **litemcp est structuré pour ça** : son SSE est *request-scoped* — un événement, `Connection: close`, `Content-Length` explicite. Un proxy tamponne les flux *en cours* ; une réponse terminée est toujours poussée. Le symptôme ne peut pas se produire ;
- **les réponses SSE portent les en-têtes anti-tampon** `X-Accel-Buffering: no` (honoré par nginx **par réponse**, même avec `proxy_buffering on` global) et `Cache-Control: no-cache, no-transform`.

Si vous contrôlez le proxy, la configuration propre :

```nginx
location /mcp {
    proxy_pass http://127.0.0.1:8080;
    proxy_http_version 1.1;
    proxy_buffering off;        # optionnel : X-Accel-Buffering: no suffit
    proxy_read_timeout 3600s;
}
```

**Repli officiel si l'hébergeur est hostile au SSE** : le client négocie `Accept: application/json` et reçoit une réponse JSON sèche — aucun flux, rien à tamponner. Les clients MCP gèrent cette négociation automatiquement.

**Choix architectural : pas de SSE longue durée.** Le mode stateless ne tient aucun flux ouvert (pas de progress ni de notifications serveur→client) : un POST, un événement, clôture. C'est ce choix qui immunise le transport contre le tamponnage des proxies et le rend trivial à déployer derrière nginx. Un flux permanent exigerait des sessions — hors périmètre, réévalué seulement si un besoin concret l'exige.

## Tests

```bash
cargo test                          # 109 tests : unitaires + intégration (vraies sockets) + doctests
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
| 3 | Durcissement HTTP (timeouts, limites, 100-continue, keep-alive) | ✅ (+ audits complets : Host/505/obs-fold/CL malformé, plafond d'issues ; durcissement 2026-10-23 appliqué ; fuzz + Inspector → 0.4) |
| 4 | Schémas Zod + outils typés + annotations | ✅ (macros → 0.4) |
| 5 | Publication crates.io | ✅ |
| 6 | Conformance officielle + benchmarks → 1.0 | ⏳ |

Le détail — jalons, SLO, risques, journal : [docs/ROADMAP.md](docs/ROADMAP.md).

## FAQ

**Pourquoi un seul transport ?**
Parce qu'un serveur lancé comme sous-processus d'un client meurt avec lui et n'accepte qu'un seul pair — c'est le modèle du client, pas de la lib. litemcp fait le choix inverse : **votre programme devient un démon Streamable HTTP stateless**, bindé sur un socket TCP, que plusieurs agents peuvent joindre, qui survit aux clients et se déploie derrière nginx. Le MCP local en sous-processus est hors périmètre, par conception.

**Pourquoi un module de schémas intégré ?**
Une lib MCP sans schéma n'est qu'un routeur JSON : les clients ont besoin de l'`inputSchema` pour présenter vos outils, et vos handlers ont besoin d'arguments garantis. Le module `zod` couvre les deux — dérivation JSON Schema + validation runtime — sans dépendance de plus : il pèse moins que les bibliothèques de schémas génériques qu'on embarquerait sinon.

**Pourquoi pas de tokio ?**
Une lib MCP mono-outil n'a pas besoin d'un runtime async complet pour lire des requêtes et répondre du JSON. Le thread par connexion suffit jusqu'à des centaines de connexions — et l'adaptateur tokio restera disponible pour les autres cas.

**Pourquoi réécrire HTTP ?**
Le périmètre est volontairement minuscule : une route POST, `Content-Length`, keep-alive, un writer SSE mono-événement. Pas de chunked entrant, pas de TLS, pas de compression. Ce choix est **tranché par les tests** : le bench maison contre le backend alternatif testé (tiny_http) montre la route maison ~1,85× plus rapide en keep-alive et 0 dépendance de plus (voir [Benchmarks](#benchmarks)) — et elle embarque son durcissement (timeouts, limites d'en-têtes, `Expect: 100-continue`).

**Pourquoi pas de WebSocket ?**
Choix architectural, pas un oubli : WS n'économiserait que le re-parsing des en-têtes HTTP (~1–2 % du trajet — JSON, syscalls et noyau réseau dominent), ne passe pas bien derrière les proxies d'hébergement, et traînerait sessions et reconnexions — les trois choses que le mode stateless évite. La spec 2026-07-28 est HTTP-only ; le keep-alive one-write de litemcp est déjà au plancher de TCP.

**Pourquoi pas de SSE longue durée ?**
Le mode stateless n'a aucun flux ouvert : chaque POST est auto-suffisant (un événement `data:`, clôture). C'est ce choix qui immunise le transport contre le tamponnage des proxies de VPS et le rend trivial à déployer derrière nginx — voir [Hébergement VPS & proxies](#hébergement-vps--proxies-le-piège-du-tamponnage-sse). Un flux permanent (progress, notifications serveur→client) exigerait des sessions : hors périmètre, réévalué si un besoin concret l'exige.

**Comment sont choisies les versions ?**
Pacement volontaire : toute modification atterrit d'abord dans la section `[Unreleased]` du [CHANGELOG](CHANGELOG.md) ; une version n'est coupée que pour un lot cohérent (fonction, correctif, doc majeure). Le `0.x` court jusqu'à la conformance officielle MCP verte ; la mention « 1.0 » attend ce jalon.

## Licence

`MIT OR Apache-2.0` — [LICENSE-MIT](LICENSE-MIT) · [LICENSE-APACHE](LICENSE-APACHE)
