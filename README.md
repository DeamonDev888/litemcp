<div align="center">

# LiteMCP

**La lib MCP la plus légère en mémoire et la plus rapide au monde.**

Stateless · socket bindé · **Streamable HTTP uniquement** (POST/SSE) · zéro runtime async

![Statut](https://img.shields.io/badge/statut-0.10.x%20publié-green) ![Licence](https://img.shields.io/badge/licence-MIT%20OR%20Apache--2.0-blue) ![MSRV](https://img.shields.io/badge/MSRV-1.85-orange) ![Spec](https://img.shields.io/badge/MCP-2026--07--28-blue)

[![crates.io](https://img.shields.io/crates/v/litemcp.svg)](https://crates.io/crates/litemcp) [![CI](https://img.shields.io/github/actions/workflow/status/DeamonDev888/litemcp/ci.yml?label=CI)](https://github.com/DeamonDev888/litemcp/actions/workflows/ci.yml) [![Docs](https://img.shields.io/docsrs/litemcp)](https://docs.rs/litemcp)

📦 **Crate officiel : [crates.io/crates/litemcp](https://crates.io/crates/litemcp)** · 📖 **Docs : [docs.rs/litemcp](https://docs.rs/litemcp)**

[Installation](#installation) · [Intégration](#intégration) · [Tutoriel](https://deamondev888.github.io/litemcp-tuto/) · [Stack](#stack-par-défaut-et-options) · [Concepts](#concepts) · [Configuration](#configuration) · [Sécurité](#sécurité) · [Protocole](#protocole) · [FAQ](#faq) · [MCP Control](#mcp-control--client-gateway-et-gestionnaire)

</div>

---

## Pourquoi LiteMCP

La plupart des serveurs MCP sont des **services** : un programme qui s'installe, charge un gros framework et tourne à côté de votre application. **LiteMCP fait l'inverse : c'est une bibliothèque Rust.** Vous écrivez quelques lignes, vous appelez `.serve()`, et un vrai serveur MCP tourne **dans** votre programme — avec les threads standards du langage, sans runtime async, sans framework HTTP. Seule dépendance pour le JSON : `serde`.

**Pour qui ?**

- **Une CLI** qui veut exposer deux ou trois outils, sans embarquer un framework web ;
- **Un test unitaire** qui a besoin d'un vrai serveur MCP qui écoute sur un port — sans installer tokio ;
- **Un binaire embarqué** (edge, IoT), où chaque mégaoctet et chaque dépendance compte ;
- **Un agent** qui lance cinquante serveurs MCP en même temps : chaque mégaoctet de RAM et chaque demi-seconde de démarrage se multiplient par cinquante.

Et pour piloter ces serveurs : [**MCP Control**](https://github.com/DeamonDev888/mcp-control), le projet sibling — **client, gateway et gestionnaire** MCP en Rust natif : superviseur multi-machines avec UI, et agrégation de tous vos serveurs (LiteMCP reconnus nativement) derrière **un seul endpoint MCP**. Détails en [fin de README](#mcp-control--client-gateway-et-gestionnaire).

Les principes :

1. **Streamable HTTP uniquement.** Un seul transport : un socket TCP bindé ; chaque message est un `POST /mcp` auto-suffisant, la réponse arrive en `application/json` ou en flux SSE (`text/event-stream`, request-scoped). Pas de session, pas d'`initialize` obligatoire, pas de flux permanent. C'est la spec 2026-07-28, à la lettre.
2. **Zéro runtime async au cœur.** `std::net::TcpListener`, un thread par connexion. Aucun adaptateur tokio n'est prévu : le périmètre de la lib est volontairement minimal.
3. **Deux dépendances externes, pas une de plus.** `serde` et `serde_json` — la dérivation JSON Schema et la validation sont faites maison, dans le module [`zod`](#outils--schémas-à-la-zod). Les macros `#[tool]` sont **incluses par défaut** mais ne tirent **aucune** dépendance externe (pur `proc_macro` standard) ; la feature `schemars` en ajoute d'autres, mais **uniquement** si vous l'activez.
4. **Mesuré ou rien.** Chaque release publie taille de binaire, latence et mémoire, avec les conditions de mesure. Jamais d'adjectif sans chiffre.

### Le coût réel des features

| Jeu de features | Crates (projet consommateur) | Binaire release (LTO fat + strip) |
|---|---|---|
| **défaut** (macros incluses) | **13** | **320 Kio** |
| minimal (`default-features = false`) | 12 (−1) | 263 Kio (−57 Kio) |
| `+ schemars` | 67 (+54) | 6,1 Mio (+5,8 Mio) |

*Mesuré le 2026-10-28 — Windows 11, rustc 1.99.1, release + `lto = "fat"` + `codegen-units = 1` + `strip`. Méthode : un projet consommateur externe dépendant de LiteMCP (un serveur, un outil), comparé avec et sans chaque feature — **la feature étant réellement utilisée**, sinon le linker élimine tout et le binaire ne change pas.*

**Le cœur ne tire que deux dépendances externes.** Le build par défaut compile `serde` + `serde_json` — les deux seules dépendances externes — plus la crate **locale** `litemcp-macros` (**zéro dépendance externe** : pur `proc_macro` standard, ni `syn`, ni `quote`, ni `proc-macro2`), incluse par défaut pour que `#[tool]` marche d'emblée (+57 Kio de binaire, mesuré). Pour le binaire strictement minimal : `litemcp = { version = "0.10.1", default-features = false }` — 263 Kio.

- **`macros`** (défaut) ajoute **une** crate locale (`litemcp-macros`, **zéro dépendance externe**) — désactivable sans rien perdre du noyau ;
- **`schemars`** est la feature à surveiller : elle tire `schemars` + `jsonschema` et **+54 crates transitives** (`regex`, `fancy-regex`, `num`, `parking_lot`, `ahash`…). Si vous l'utilisez réellement, le binaire passe de **320 Kio à 6,1 Mio** ; si vous l'activez sans l'utiliser, le linker élimine tout et le binaire ne bouge pas ;
- **Si vous voulez rester minimal**, utilisez le module `zod` intégré : il couvre le même terrain — validation, formats, défauts, dictionnaires, énumérations — **sans aucune dépendance de plus**, et gardez `default-features = false` si vous n'écrivez pas vos outils avec `#[tool]`.

> Les dépendances **externes** du cœur restent à **2** quoi que vous activiez : `macros` n'ajoute qu'une crate locale sans deps, et `schemars` n'est qu'un adaptateur *optionnel* pour les types `#[derive(JsonSchema)]`.

## Stack par défaut et options

Le tableau suivant fixe le contrat : ce qui est **toujours** là (stack par défaut) et ce qui ne l'est que si vous activez la feature Cargo correspondante (**options**).

| Élément | Stack **par défaut** | **Options** (features Cargo) |
|---|---|---|
| Sérialisation JSON | `serde` + `serde_json` — **les deux seules dépendances externes** | — |
| Exécution | **zéro runtime async** : threads `std`, un par connexion | — |
| Transport | HTTP/1.1 maison sur `std::net::TcpListener` (POST / SSE request-scoped, keep-alive) | — |
| Sûreté mémoire | `#![forbid(unsafe_code)]` sur tout le crate | — |
| MSRV | **1.85** (`rust-version = "1.85"`, edition 2024) | — |
| Macros `#[tool]` · `#[resource]` · `#[prompt]` | **incluses par défaut** : `litemcp-macros`, **zéro dépendance externe** (pur `proc_macro` standard, sans `syn`/`quote`) — +1 crate locale, +57 Kio (mesuré ci-dessus) | désactivables via `default-features = false` (binaire minimal 263 Kio) |
| Schémas & validation | module [`zod`](#outils--schémas-à-la-zod) intégré : dérivation JSON Schema + validation exécuteur, **sans dépendance de plus** | feature **`schemars`** : adaptateur `#[derive(JsonSchema)]` (`schemars`) + validateur `jsonschema` avec **formats appliqués** — +54 crates transitives, +5,8 Mio de binaire (mesuré ci-dessus) |

Les features sont **additives** : les dépendances **externes** du cœur sont toujours `serde` + `serde_json` — désactiver aussi les macros (`default-features = false`) restitue exactement la stack minimale du tableau (le code des options n'existe alors même pas au build). Activer une feature ne renchérit qu'elle-même (coûts mesurés dans [Le coût réel des features](#le-coût-réel-des-features)). Le noyau — JSON-RPC, transport, registres, schémas `zod`, erreurs structurées, observabilité — n'a jamais besoin d'option.

## Fonctions phares

| | |
|---|---|
| ⚡ **Minuscule** | 2 dépendances, binaire minimal **263 Kio** (mesuré, release strippé), démarrage en quelques ms |
| 🚀 **Sans runtime** | threads `std`, un par connexion — pas de tokio au cœur |
| 🧩 **Schémas à la Zod** | `inputSchema` dérivé du schéma, publié dans `tools/list` ; `outputSchema` **exécuteur** (sortie validée avant envoi) |
| ✅ **Validation avant exécution** | un appel hors schéma ne touche jamais votre handler |
| 🦀 **Handlers typés** | arguments désérialisés dans vos structs `Deserialize` ; macro `#[tool]` (incluse par défaut) qui dérive le schéma des types des paramètres |
| 📚 **Resources & Prompts** | `resources/list`·`read`·`templates/list`, `prompts/list`·`get` — mêmes indices de cache que `tools/list` (SEP-2549) ; lectures **binaires** (`blob` base64), gabarits RFC 6570 et `completion/complete` inclus |
| 🔁 **Multi round-trip (MRTR)** | un handler d'outil/prompt/ressource demande une entrée client (`resultType: "input_required"`) : élicitation, échantillonnage, racines — état opaque signé HMAC-SHA256, rejeu avec `inputResponses` sous un id différent |
| ⏱️ **Handlers bornés** | `.tool_timeout(duration)` : un handler qui dépasse rend une erreur, la connexion n'est plus retenue indéfiniment |
| 🩺 **Erreurs structurées & observabilité** | `ToolError` (`rate_limited`, `timeout`, `upstream`… avec `retryable`/`retryAfterMs`) + `.on_call(hook)` pour métriques et latences |
| 🔐 **Verrou de requête** | `.on_request(hook)` : verdict par requête **avant la lecture du corps** — auth par token, rate-limit par source ; rejet `401`/`403`/`429` + `Retry-After`, paniques contenues |
| 🛑 **Arrêt propre** | `bound.handle()` puis `handle.shutdown()` : `serve()` rend `Ok(())`, les connexions ferment entre deux requêtes |
| 🧟 **Anti-zombie** | sweep du registre d'annonce au bind — **ne détruit qu'en preuve positive** : une annonce usurpée (le port répond avec une identité d'instance différente de celle publiée) est retirée ; un port sans réponse ou une identité invérifiable = doute → **conservée** (un refus TCP n'est pas une preuve de mort) ; drain borné des connexions à l'arrêt, bilan vérifiable (`handle.zombie_report()`) — rien ne reste accroché |
| 🔀 **Gateways dynamiques** | `Gateway` : la surface d'outils change **pendant que le serveur sert** — un serveur détecté apparaît/disparaît de `tools/list` sans redémarrage, les requêtes en vol finissent sur leur snapshot |
| 🛡️ **Sûr par défaut** | `#![forbid(unsafe_code)]`, garde DNS-rebinding, corps limité |

## Installation

```toml
[dependencies]
litemcp = "0.10.1"   # macros #[tool]/#[resource]/#[prompt] incluses

# binaire strictement minimal (sans les macros) :
# litemcp = { version = "0.10.1", default-features = false }
# pour l'adaptateur schemars (#[derive(JsonSchema)] en schéma exécuteur) :
# litemcp = { version = "0.10.1", features = ["schemars"] }
```

**Le jeu par défaut est complet.** Sans rien configurer, vous obtenez tout le noyau — transport HTTP durci, JSON-RPC, module `zod` (schémas + validation), erreurs structurées, hooks `on_call`/`on_request`, gateway dynamique, anti-zombie, annonces — **plus les macros `#[tool]`** (une crate locale, zéro dépendance externe). Les dépendances externes restent **exactement deux** (`serde`, `serde_json`). La seule autre option est `schemars` — un extra opt-in qui n'ajoute que son propre coût (mesuré dans [Le coût réel des features](#le-coût-réel-des-features)) et rien n'est compilé tant qu'elle est désactivée. Le tableau [Stack par défaut et options](#stack-par-défaut-et-options) fixe le contrat complet.

## Intégration

LiteMCP est une **bibliothèque**, pas un serveur à lancer : rien à installer, rien à démarrer — vous ajoutez la dépendance à un projet existant et **votre programme** devient un serveur MCP. La preuve, dans un `main` déjà à vous :

```rust
use litemcp::prelude::*;

fn main() -> std::io::Result<()> {
    let bound = ServerBuilder::new("calc", "1.0.0")
        .tool("add", "Additionne deux nombres", |args| {
            let a = args["a"].as_i64().ok_or("a manquant")?;
            let b = args["b"].as_i64().ok_or("b manquant")?;
            Ok(serde_json::json!({ "somme": a + b }))
        })
        .bind_default()?;          // premier port libre de la tranche 23080–23099, repli éphémère
    println!("MCP sur http://{}", bound.local_addr()?);
    bound.serve()                   // bloque : POST /mcp → JSON ou SSE
}
```

C'est tout — votre binaire expose maintenant ses outils en MCP, dans son propre process. Aucune session, aucun handshake, aucun runtime.

**La lib n'affiche rien, par design** : aucun log, aucun banner — stdout et stderr vous appartiennent. C'est pourquoi l'exemple capture `bound` et affiche lui-même l'adresse : ce `println!` est du code utilisateur, pas de la lib. `bind_default()` prend **le premier port libre** de la tranche LiteMCP (`23080..=23099`) : le deuxième serveur LiteMCP de la machine démarre sans coordination ni collision, repli sur un port éphémère si la tranche est pleine. Un port explicite reste possible : `.bind("127.0.0.1:23080")`.

Vérifiez-le avec curl (23080 dans le cas nominal — aucun autre serveur LiteMCP déjà lancé ; sinon, le port effectif est celui affiché par votre `main`) :

```bash
curl -s http://127.0.0.1:23080/mcp \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"add","arguments":{"a":5,"b":7}}}'
```

```json
{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","content":[{"type":"text","text":"{\"somme\":12}"}],"structuredContent":{"somme":12},"_meta":{"io.modelcontextprotocol/serverInfo":{"name":"calc","version":"1.0.0"}}}}
```

*Cette réponse est vérifiée octet pour octet par la suite de tests ([`tests/readme_parity.rs`](tests/readme_parity.rs)) : si le code change et que le README ne suit pas, un test échoue avant publication.*

Ou avec MCP Inspector en mode URL : `npx @modelcontextprotocol/inspector` → `http://127.0.0.1:23080/mcp` (le port effectif est celui affiché par votre `main`).

Un exemple complet (les trois façons d'enregistrer un outil + annotations) : `cargo run --example hello`.

> 📖 **Envie d'être guidé ?** Le [tutoriel pas à pas](https://deamondev888.github.io/litemcp-tuto/) construit un serveur MCP complet avec LiteMCP — du premier outil aux schémas, ressources et prompts.

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
    .bind("127.0.0.1:23080")?
    .serve()?;
```

Trois registres, du plus direct au plus confortable :

| API | Handler reçoit | `inputSchema` | Validation |
|---|---|---|---|
| `tool` | `&Value` brut | générique (`{"type":"object"}`) | à votre charge |
| `tool_typed` | `&Value` (déjà validé) | dérivé du schéma | automatique |
| `tool_parse` | struct `Deserialize` | dérivé du schéma | automatique |

Schémas disponibles : `object().field(..)/.opt(..)/.strict()`, `string().min/.max` **et formats exécutés** `.email()/.uuid()/.url()/.datetime()`, `integer().min/.max`, `number().min/.max`, `boolean()`, `enumeration(&[..])` (chaînes) et `integer_enumeration(&[..])` (entiers), `array(s).min/.max`, `record(valeur)` (objet à valeurs typées), plus `nullable(schéma)` (accepte `null`, publié `anyOf`) et `describe("…", schéma)` (description publiée). Les contraintes deviennent `minLength`/`maxLength`/`minimum`/`maximum`/`minItems`/`maxItems`/`enum`/`format` dans le JSON Schema publié à `tools/list` — **appliquées** à l'identique par la validation. Des bornes incohérentes (`min > max`), non finies (`NaN`/±∞) ou une `enumeration` vide/doublonnée paniquent au démarrage (erreur de programmation, jamais publiées).

**Valeurs par défaut** — `.default(v)` attache une valeur par défaut à n'importe quel schéma. Elle est **publiée** (mot-clé `default` du JSON Schema) **et appliquée** : le champ devient optionnel, et la valeur est **injectée** dans les arguments avant l'exécution du handler (comme Zod).

```rust
zod::object()
    .field("query", zod::string())
    .opt("limit", zod::integer().default(10))   // publié `default: 10`, injecté si absent
```

**Dictionnaires** — `record(valeur)` valide **chaque valeur** d'un objet (les clés sont libres) ; publié `{"type": "object", "additionalProperties": <schéma>}`.

```rust
zod::object().field("labels", zod::record(zod::string()))
// {"a": "x"} → valide ; {"a": 1} → refusé (chemin `arguments.labels.a`)
```

`Box<S>`, `Arc<S>` et `&S` implémentent `Schema` : un **registre de schémas hétérogènes** (issus d'une config, d'une base, d'un registre dynamique) s'utilise directement, sans wrapper manuel.

### Référence des combinators zod

Chaque combinator du module `zod` publie une forme JSON Schema (draft 2020-12) et applique **exactement** cette sémantique à la validation — le schéma publié est exécuteur, sans dérogation :

| Combinator | Publié dans l'`inputSchema` | Validation appliquée |
|---|---|---|
| `object().field(n, s)` | propriété `n` + `n` listé dans `required` | présent et valide, sinon issue (`arguments.n is required`) ; doublon de nom → panique de développement |
| `object().opt(n, s)` | propriété `n`, hors `required` | absent → valide ; présent → validé par `s` |
| `.strict()` | `"additionalProperties": false` | tout champ inconnu → issue « unknown field (strict object) » |
| `string()` | `{"type": "string"}` | type chaîne |
| `string().min(n)` / `.max(n)` | `minLength` / `maxLength` | longueur en **caractères** (points de code, comme JSON Schema — pas en octets) ; `min > max` → panique au démarrage |
| `string().email()` | `format: "email"` | **appliqué, profil plus strict que la référence** : `local@domaine` — bornes RFC 5321 (≤ 254 octets, `local` ≤ 64, labels ≤ 63 sans `-` en bord), domaine avec **au moins un point** (`user@localhost` refusé — le validateur de référence l'accepte), ni espaces ni caractères de contrôle |
| `string().uuid()` | `format: "uuid"` | **appliqué** : forme canonique 8-4-4-4-12 hexadécimale |
| `string().url()` | `format: "uri"` | **appliqué, profil plus strict que la référence** : URL **avec autorité** `schéma://hôte…` (IPv6 entre crochets acceptée, port 1–65535, aucun espace, contrôle ou antislash) — une URI sans autorité (`mailto:`, `urn:`, acceptés par la référence) est refusée |
| `string().datetime()` | `format: "date-time"` | **appliqué** : **RFC 3339 §5.6 + §5.7, notes incluses** — calendrier vérifié (jours par mois, années bissextiles), fraction optionnelle, fuseau **obligatoire** (`Z`/`z` ou `±HH:MM`), séparateur `T`, `t` **ou espace** (NOTE RFC 3339 — le validateur de référence refuse l'espace : divergence publiée), seconde 60 (intercalaire) acceptée **au seul point UTC 23:59, décalé par le fuseau** comme l'exige §5.7 (`01:59:60+02:00` = `23:59:60Z` accepté ; `23:59:60+02:00` = `21:59:60Z` refusé — la référence rend les mêmes verdicts) ; aucun espace superflu (tête/fin/doublé), refusé des deux côtés |
| `integer()` | `{"type": "integer", "minimum": i64::MIN, "maximum": i64::MAX}` | **sémantiques JSON Schema (draft 2020-12)** : tout nombre **mathématiquement intégral** est accepté (`3.0`, `1e2`, `-0.0`) et **normalisé en entier exact** avant le handler — `as_i64()` réussit **toujours** quand le schéma passe ; un non-intégral (`3.5`) est refusé ; un intégral hors de la plage 64 bits **publiée** (`1e300`, `u64::MAX`) viole les bornes publiées — jamais de conversion silencieuse (vérifié en différentiel contre le validateur de référence : `tests/zod_diff_jsonschema.rs`) |
| `integer().min(n)` / `.max(n)` | `minimum` / `maximum` — **les bornes publiées sont exactement celles appliquées** | hors borne (incluse) → issue ; `min > max` → panique |
| `uint()` | `{"type": "integer", "minimum": 0, "maximum": u64::MAX}` | **entier non signé exact** — même sémantique JSON Schema que `integer()` : tout intégral (`3.0`, `1e19`) est accepté et **normalisé en u64 exact** avant le handler (`as_u64()` réussit **toujours** quand le schéma passe) ; un négatif est refusé — **jamais** transformé en grand entier ; `min`/`max` en u64 (`min > max` → panique) |
| `number()` | `{"type": "number"}` (+ `minimum`/`maximum` si bornes) | tout nombre JSON, bornes incluses ; borne non finie (NaN/±∞) → panique (serde_json la publierait `null` et elle ne serait jamais appliquée) |
| `boolean()` | `{"type": "boolean"}` | type booléen |
| `enumeration(&["a", "b"])` | `{"type": "string", "enum": ["a", "b"]}` | appartenance stricte (l'issue cite les valeurs autorisées) ; liste vide ou doublon → panique |
| `integer_enumeration(&[1, 2])` | `{"type": "integer", "enum": [1, 2]}` | appartenance en **égalité mathématique** (JSON Schema 2020-12) : un flottant intégral égal à un membre passe (`3.0` → `3`, `2e2` → `200`, `-0.0` → `0` — le validateur de référence rend le même verdict) et est **normalisé en l'entier exact du membre** avant le handler (`as_i64()` réussit toujours quand le schéma passe) ; non-intégral → « expected an integer », entier hors énumération → « must be one of … » ; liste vide ou doublon → panique |
| `array(s).min(n)` / `.max(n)` | `{"type": "array", "items": s}` + `minItems` / `maxItems` | taille puis chaque élément validé ; chemin d'issue indexé (`arguments.list[1]`) |
| `record(s)` | `{"type": "object", "additionalProperties": s}` | chaque **valeur** validée par `s` (clés libres) ; le chemin d'issue pointe la clé fautive (`arguments.labels.a`) ; aucune clé n'est inventée par les défauts |
| `nullable(s)` | `{"anyOf": [s, {"type": "null"}]}` | `null` accepté ; sinon délégation à `s` — la cardinalité ne change pas (avec `.field`, le champ reste requis) |
| `describe("…", s)` | `s` + `"description": "…"` | délégation intégrale à `s` |
| `<tout schéma>.default(v)` | mot-clé `default: v` ; le champ **sort de `required`** | absent → `v` **injecté** dans les arguments avant le handler (récursivement : un objet imbriqué porte ses propres défauts) ; le défaut lui-même est validé — un défaut incohérent fait échouer la validation |

Bornes communes : profondeur d'imbrication plafonnée (`MAX_DEPTH = 128` — issue « maximum nesting depth ») et collecteur d'issues borné (`MAX_ISSUES = 64` — au-delà, les problèmes sont comptés et résumés par « … and N more issue(s) omitted » : un tableau géant d'éléments invalides ne peut pas faire grossir la mémoire). Les paniques signalées ci-dessus sont des **erreurs de programmation**, levées à la construction du schéma (donc au démarrage) — jamais des entrées utilisateur.

Hors DSL (feature `schemars`) : [`from_schemars::<T>()`](#adaptateur-schemars-feature-schemars) et `from_json_schema(document)` branchent le JSON Schema de votre choix avec la même promesse — **publié tel quel**, appliqué par le validateur `jsonschema` compilé avec les `format` **activés** (les formats inconnus restent ignorés ; leurs profils sont ceux de la référence, parfois moins stricts que le DSL `zod` — voir le tableau ci-dessus). Les entiers intégraux (`3.0`) y sont eux aussi **normalisés en entiers exacts** avant le handler (marche guidée par le document publié, `$ref` locaux résolus) : un champ `u32`/`i64` dérivé reçoit un entier désérialisable même si le client envoie `3.0`. Les `default` publiés — dérivés par `schemars` (`#[serde(default)]`) ou écrits à la main dans un document brut — y sont **injectés** dans les arguments absents exactement comme dans le DSL : récursivement (objets, tableaux, branches `allOf`/`anyOf`/`oneOf`), `$ref` locaux chaînés résolus (cycles détectés, parcours borné), la détection et l'injection voyant exactement les mêmes sous-schémas.

### Macros `#[tool]` · `#[resource]` · `#[prompt]` (incluses par défaut)

Le schéma est **dérivé des types des paramètres** d'une fonction ordinaire — plus de double déclaration schéma/struct. Chaque macro génère `register_<fonction>`, à enchaîner avec `.with_tool(…)` :

```rust
use litemcp::macros::{prompt, resource, tool}; // incluses par défaut

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
    .bind("127.0.0.1:23080")?
    .serve()?;
```

`#[tool]` et `#[prompt]` valident les arguments via le schéma dérivé (`#[prompt]` passe par `.prompt_typed(…)`, la variante validée de `.prompt()`) ; `#[resource]` prend une fonction **sans paramètre** (`uri` requise). Types supportés : entiers (`i8`…`i64`, `isize`, `u8`…`u64`, `usize` — **bornes exactes publiées et appliquées** pour chacun : les non signés passent par [`zod::uint()`](#référence-des-combinators-zod), plage complète `[0, u64::MAX]` pour `u64` et `[0, usize::MAX]` pour `usize` selon la cible ; un négatif est refusé, jamais transformé en grand entier), `f32`/`f64`, `bool`, `String`, `Option<T>` (champ optionnel), `Vec<T>`. Les valeurs par défaut des paramètres entiers sont publiées **exactement** (un littéral `u64::MAX` ou au-delà de `i64::MAX`, avec ou sans suffixe, est accepté et publié en u64 ; au-delà des plages i64/u64, erreur de compilation). Le crate reste à **2 dépendances externes** — et `litemcp-macros` elle-même n'a **aucune dépendance externe** (pur `proc_macro` standard, sans `syn` ni `quote`).

**Valeurs par défaut** — `default(nom = valeur, …)` déclaré sur la macro (Rust interdit les attribute macros sur les paramètres). Le champ devient optionnel, la valeur est **publiée** dans l'`inputSchema` **et injectée** dans les arguments si elle est absente.

```rust
#[tool(description = "Recherche", default(limit = 10, mode = "fast"))]
fn recherche(query: String, limit: u64, mode: String) -> litemcp::ToolResult {
    Ok(serde_json::json!({ "query": query, "limit": limit, "mode": mode }))
}
// sans "limit" dans les arguments → limit = 10 ; "limit": 42 → 42
```

### Adaptateur `schemars` (feature `schemars`)

Un type `#[derive(JsonSchema)]` devient un schéma exécuteur — le JSON Schema publié est celui que schemars dérive, et la validation applique **le même document** via le validateur `jsonschema` :

```rust
use litemcp::schemars::from_schemars; // features = ["schemars"]
use schemars::JsonSchema;

#[derive(JsonSchema)]
struct SearchArgs { query: String, limit: Option<u8> }

ServerBuilder::new("recherche", "1.0.0")
    .tool_typed("search", "Recherche indexée", from_schemars::<SearchArgs>(), |args| {
        // arguments validés par le schéma dérivé du struct
        Ok(serde_json::json!({}))
    })
    .bind("127.0.0.1:23080")?
    .serve()?;
```

Utilisable partout où l'API attend `impl zod::Schema` (`tool_typed`, `tool_parse`, `prompt_typed`, `output_schema`). Sémantiques JSON Schema de bout en bout (le validateur officiel accepte par ex. `3.0` pour `integer` — et litemcp **normalise** ces intégraux en entiers exacts avant le handler, comme pour `zod::integer()` : `as_i64()`/désérialisation serde réussissent toujours). Les `format` (`email`, `uuid`, `date-time`, `uri`…) sont **appliqués** selon les validateurs de la référence `jsonschema` — parfois moins stricts que le DSL `zod` (voir le tableau « Formats »). Deux deps de plus, uniquement si la feature est activée.

**Schéma JSON brut** — pour les schémas que le DSL n'exprime pas (`oneOf`, `not`, `patternProperties`, `if`/`then`/`else`, `$ref` internes…), `from_json_schema` prend un document écrit à la main : il est **publié tel quel** et **appliqué** par le même validateur. Un booléen JSON Schema reste un document valide, mais l'`inputSchema` **racine** d'un outil MCP doit être un objet : l'enregistrement (`tool_typed`/`tool_parse`) refuse une racine `true`/`false` — les schémas booléens restent acceptés partout où JSON Schema les autorise (`properties.*`, `items`…).

```rust
use litemcp::schemars::from_json_schema;
use serde_json::json;

let schema = from_json_schema(json!({
    "type": "object",
    "properties": { "value": { "oneOf": [{ "type": "string" }, { "type": "integer" }] } },
    "required": ["value"]
}));

ServerBuilder::new("app", "1.0.0")
    .tool_typed("t", "t", schema, |args| Ok(args.clone()))
    .bind("127.0.0.1:23080")?
    .serve()?;
```

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
    .bind("127.0.0.1:23080")?
    .serve()?;
```

`resources/read` prend `{"uri": …}` ; `prompts/get` prend `{"name": …, "arguments": {…}}` (objet exigé, sinon `-32602`). Les capacités `tools`, `resources` et `prompts` sont **toujours** annoncées dans `server/discover` et `initialize` — leurs méthodes sont servies même à registre vide (catalogue vide), et un `Gateway` peut peupler les registres après le démarrage. Aucune sous-capacité non servie (`listChanged`, `subscribe`) n'est déclarée.

**Contenus riches — la forme `ContentBlock` complète de la spec.** Un outil qui rend une image, un audio, un lien de ressource, une ressource embarquée — ou plusieurs blocs mélangés — s'enregistre avec `.tool_result` : le handler construit le `CallToolResult` complet, qui part tel quel (l'API `Ok(Value)` historique reste inchangée et garde sa forme fil) :

```rust
use litemcp::prelude::*;

ServerBuilder::new("ecran", "1.0.0")
    .tool_result("screenshot", "Capture d'écran annotée", |_args| {
        Ok(CallToolResult::from_content([
            ContentBlock::text("Voici la capture :"),
            ContentBlock::image_bytes(&[0x89, 0x50, 0x4E, 0x47], "image/png"), // base64 réel
            ContentBlock::resource_link("app://doc", "Doc"),
        ]))
    })
    .prompt("visuel", "Prompt avec image", |_args| {
        Ok(vec![
            PromptMessage::user_content(ContentBlock::image_bytes(&[0xFF, 0xD8, 0xFF], "image/jpeg")),
            PromptMessage::user("Décris cette image."),
        ])
    })
    .bind("127.0.0.1:23080")?
    .serve()?;
```

Les données binaires (`image`/`audio`/`blob`) sont **base64 encodées par la lib** depuis les octets réels (`from_bytes`/`binary`) — jamais sérialisées en texte JSON pour simuler le support.

**Ressource binaire** — `.resource_data` laisse le handler choisir texte ou binaire (`blob`) à chaque lecture :

```rust
use litemcp::prelude::*;

ServerBuilder::new("assets", "1.0.0")
    .resource_data("app://icon", "Icône", "Icône PNG de l'app", || {
        Ok(ResourceData::binary("app://icon", Some("image/png".into()), &PNG_BYTES))
    })
    .bind("127.0.0.1:23080")?
    .serve()?;
```

**Gabarits et complétion** — `.resource_template(uri, nom, description)` publie un gabarit RFC 6570 via `resources/templates/list` (indices de cache SEP-2549, couvert par la capacité `resources`) ; `.completions(f)` branche un fournisseur de suggestions pour `completion/complete` — la capacité `completions` n'est annoncée que quand un fournisseur existe :

```rust
use litemcp::prelude::*;

ServerBuilder::new("calc", "1.0.0")
    .resource_template("calc://history/{date}", "Historique", "Historique par date")
    .completions(|request: &CompletionRequest| match &request.reference {
        CompleteReference::Prompt { name, .. } if name == "revue" => {
            Ok(Completion::new(vec!["src/main.rs".into()]))
        }
        _ => Ok(Completion::new(vec![])),
    })
    .bind("127.0.0.1:23080")?
    .serve()?;
```

La borne de la spec (100 suggestions) est appliquée par le noyau : au-delà, troncature + `hasMore: true`. La **lecture** d'une ressource reste un match exact sur les URIs enregistrées — la substitution de gabarit côté noyau n'est pas implémentée (inscrivez les URIs concrètes correspondantes).

### Multi round-trip (MRTR)

Un handler qui a besoin d'une entrée client (formulaire d'élicitation, échantillonnage, racines) renvoie `ToolOutcome::InputRequired` / `PromptOutcome::InputRequired` / `ResourceOutcome::InputRequired` depuis un handler enregistré avec `.tool_mrtr` / `.prompt_mrtr` / `.resource_mrtr` — il est **réappelé** au rejeu avec les réponses du client (non fiables) et l'état vérifié ([`InputRound`] : `turn`, `input_responses`, `state`, `.accepted(clé)`) :

```rust
use litemcp::prelude::*;

ServerBuilder::new("booking", "1.0.0")
    .tool_mrtr("reserve", "Réservation à confirmation", |_args, round| {
        if round.turn == 1 {
            Ok(ToolOutcome::InputRequired(
                InputRequiredSpec::new()
                    .elicit("confirm", "Confirmer ?", serde_json::json!({"type": "object"}))
                    .with_state(serde_json::json!({ "etape": 1 })),
            ))
        } else {
            Ok(ToolOutcome::Value(serde_json::json!({
                "confirme": round.accepted("confirm").is_some()
            })))
        }
    })
    .bind("127.0.0.1:23080")?
    .serve()?;
```

Le noyau fait le reste : validation des requêtes embarquées (`elicitation/create`, `sampling/createMessage`, `roots/list` — une autre méthode ou une forme fil invalide est un bogue de handler rendu `-32603` **sans rien émettre**), exigence de la capacité client déclarée correspondante (`-32021` sinon), `requestState` **opaque signé** (HMAC-SHA256, TTL 10 min par défaut — `.request_state_ttl`, clé partagée en flotte — `.request_state_key`, borne d'état 8 Kio), liaison du jeton à la méthode et aux paramètres saillants. Le client réémet la requête **sous un id différent** avec `inputResponses` et l'écho exact du jeton ; un jeton altéré, expiré, malformé ou lié à une autre requête est refusé en `-32602` avant tout handler. Le jeton est **porteur et non consommé** (imposez la consommation unique dans le handler si nécessaire) ; les champs MRTR sont ignorés sur toute autre méthode et **aucune capacité serveur supplémentaire n'est annoncée** — rien n'est servi au-delà des résultats `input_required`. Aucune dépendance nouvelle : SHA-256, HMAC-SHA256 et base64url sont implémentés localement.

### Annotations

Métadonnées de la spec 2026-07-28, publiées dans `tools/list` : `.title()`, `.read_only()`, `.destructive()`, `.idempotent()`, `.open_world()`. Un client sait ainsi qu'un outil ne modifie rien avant de l'appeler.

### Erreurs

- **Hors schéma** → `-32602` avant exécution ; `error.data.issues` liste chaque problème en anglais (champ manquant, type, borne, champ inconnu) ;
- **Erreur d'outil** (`ToolError`) → `CallToolResult::isError` avec le message en texte **et** un objet machine-lisible sous `structuredContent.error` (`code`, `message`, `retryable`, `retryAfterMs` optionnel via `ToolError::rate_limited` / `timeout` / `upstream` / `invalid_input` / `unauthorized` / `not_found` / `internal`). Les erreurs texte (`String`, `&str`) restent acceptées via `?` (`From<String>` / `From<&str>`, classées `unclassified`) ;
- **Erreur de protocole** (mauvaise requête, outil inconnu, JSON invalide) → erreur JSON-RPC typée (-32700, -32602, -32601…), rendue de façon opaque.

La valeur de retour d'un outil devient `structuredContent` — un objet JSON passe tel quel ; toute autre valeur (scalaire, tableau, `null`) est enveloppée dans `{"result": …}` (**choix d'API** : `structuredContent` toujours objet, contrat simple pour les clients) — **sauf** si un `outputSchema` de racine non-objet est attaché à l'outil : la valeur brute est alors envoyée telle quelle (la spec MCP 2026-07-28 autorise toute valeur JSON dans `structuredContent`, et l'enveloppe serait elle-même non conforme au schéma publié). Une `Err(msg)` devient `isError: true` avec le message en contenu texte. Un `outputSchema` attaché via `.output_schema("outil", schéma)` valide cette sortie **avant envoi** : non conforme → résultat `isError`.

## Configuration

**Toutes les méthodes du builder sont optionnelles** : chacune pose un défaut sûr documenté ci-dessous (colonne « Défaut »), et `ServerBuilder::new(nom, version)` + un outil + un bind suffisent à servir. Appelez uniquement les méthodes dont vous changez le comportement — l'ordre d'appel est libre (chaînage), et les méthodes `annotate`/`output_schema` fonctionnent avant **ou** après l'enregistrement de l'outil concerné.

| Méthode | Défaut | Rôle |
|---|---|---|
| `.route(path)` | `/mcp` | chemin unique du serveur — **validé au bind** : doit commencer par `/`, sans query, et ne pas masquer `/health` (sinon erreur `InvalidInput`, pas de serveur silencieusement injoignable) |
| `.bind_default()` | premier libre dans `23080..=23099` ([`DEFAULT_PORT_RANGE`](#intégration)) | auto-attribution sur `127.0.0.1` : le premier port libre de la tranche LiteMCP gagne (le 2ᵉ, 10ᵉ… process démarre sans collision) ; tranche pleine → port éphémère OS ; port effectif via `local_addr()` et publié par `.announce()`. `.bind(addr)` reste le bind explicite |
| `.allow_origins(&[..])` | vide | origines `Origin` autorisées (clients navigateur) — toute requête avec `Origin` non listée est refusée (garde DNS-rebinding), **sauf** l'exception loopback par défaut : sur un bind loopback spécifique (`127.0.0.0/8`, `::1`), une origine `http://` strictement identique au `Host` reçu et au littéral `ip:port` du listener est acceptée (client navigateur même-origine) ; les clients CLI n'en envoient pas. Format : l'origine **normalisée navigateur** — schéma + hôte en minuscules, **sans port par défaut** (un navigateur écrit `https://a.example`, jamais `https://a.example:443`) ; comparaison exacte insensible à la casse |
| `.read_timeout(duration)` | 30 s | timeout de lecture par opération (anti-slowloris) — un client trop lent est déconnecté sans réponse |
| `.max_connections(n)` | 256 (par serveur) | plafond de connexions simultanées — au-delà, `503` immédiat, pas de spawn |
| `.connection_ttl(duration)` | 5 min | durée de vie maximale d'une connexion keep-alive — borne le temps d'occupation d'un slot (sans elle, un client qui drip-feed des requêtes valides retient son slot indéfiniment) ; `0` désactive le TTL |
| `.tool_timeout(duration)` | illimité | délai maximal d'exécution des handlers (outils, ressources, prompts) — au-delà : résultat `isError` (`ErrorCode::Timeout`, retentable) pour un outil, erreur `-32603` avec data `timeout` pour une ressource/prompt/complétion ; les arguments d'un `tools/call` sont clonés une fois par appel quand il est actif |
| `.on_call(hook)` | aucun | callback d'observabilité appelé après chaque message JSON-RPC (`CallEvent { method, name, duration, status }`) ; cumulable, paniques du hook contenues |
| `.on_request(hook)` | aucun | **verrou d'accès par requête** — le hook reçoit un `RequestContext` (méthode, chemin, query, **adresse du pair**, en-têtes via `ctx.header(nom)` insensible à la casse) et rend `Ok(())` ou un `RequestRejection` : `unauthorized` (401), `forbidden` (403), `rate_limited(retry_after)` (429 + `Retry-After`), `new(status, msg)` (hors 4xx/5xx → 400), `.with_header("WWW-Authenticate", …)`. Rejet = statut + corps JSON + **fermeture de connexion**, avant la lecture du corps ; jamais appelé sur `/health` ni sur une route inconnue ; cumulable (premier rejet gagne), paniques contenues (`500`). Voir [Sécurité](#sécurité) |
| `.announce()` | désactivée | publie une annonce JSON dans le dossier temporaire partagé (`%TEMP%/litemcp-registry/<nom>.json`) au bind réussi — **adresse effective connectable** (`addr`), identité d'instance vérifiable (`instance`, exposée aussi par `/health` en en-tête `X-Instance-Id`), port, PID, exe, args, cwd, ts — retirée à la fin de vie du serveur ; intégration native avec le [MCP Control](#mcp-control--client-gateway-et-gestionnaire) (voir ce qui suit) |
| `.anti_zombie()` | désactivée | **anti-zombie** : au bind, le registre d'annonce partagé est nettoyé (annonces **usurpées** retirées sur preuve d'identité, débris d'écriture enlevés, doute conservé — un refus TCP n'est pas une preuve de mort — et instances vivantes vérifiées respectées) ; à la fin de vie, les connexions sont drainées (attente bornée) et le bilan `ZombieReport` devient lisible via `handle.zombie_report()` — `sweep_registry()`/`sweep_dir()` utilisables seuls |
| `.drain_timeout(duration)` | 2 s | durée maximale du drain anti-zombie à la fin de vie (les threads ne sont jamais tués — au-delà, le bilan signale `drained: false`) ; `0` : bilan immédiat ; sans effet si `.anti_zombie()` n'est pas actif |
| Threads de handlers (`MAX_INFLIGHT_HANDLERS`) | 256 | plafond de threads de handlers simultanés (actifs ou détachés après un timeout de `tool_timeout`) — au-delà : `rate_limited` pour un outil, `-32603` avec data `overloaded` pour une ressource/prompt ; le slot n'est rendu qu'à la terminaison effective du thread |
| Réponse JSON-RPC (`MAX_RESPONSE_BYTES`) | 16 Mio | une réponse sérialisée qui dépasse ce plafond est remplacée par `-32603` (« response size … exceeds maximum … ») — un handler ne peut pas pousser un corps illimité |
| `bound.handle()` → `ServerHandle` | — | poignée d'arrêt cloneable : `handle.shutdown()` rend `serve()` → `Ok(())`, les connexions ferment entre deux requêtes (une lecture bloquée en cours ferme au `read_timeout`) |
| Sonde `GET /health` | `/health` (inclus) | `200` « ok » sans JSON-RPC — probes k8s/systemd/équilibreur (`HEAD` accepté, sans corps) |
| Corps de requête | 1 Mio | au-delà → `413` — lu **par tranches** (buffer 64 Ko) : la mémoire engagée est proportionnelle aux octets réellement reçus |
| En-têtes | 64 max, lignes 16 Kio max | au-delà → `431` |

## Sécurité

**Modèle de menace assumé : LiteMCP est une bibliothèque réseau, pas un service exposé.** Quiconque peut joindre le socket peut appeler vos outils — sauf si vous posez un **verrou de requête** [`on_request`](#configuration) : un verdict par requête, avant même la lecture du corps (auth par token, rate-limit par source ou par clé), rejet `401`/`403`/`429` avec en-têtes libres (`WWW-Authenticate`, `Retry-After`) :

```rust
use litemcp::prelude::*;

const TOKEN: &str = "s3cret"; // token « quick fix » — l'OIDC/SAML reste au gateway

ServerBuilder::new("calc", "1.0.0")
    .on_request(|ctx| {
        match ctx.header("Authorization").and_then(|v| v.strip_prefix("Bearer ")) {
            Some(t) if t == TOKEN => Ok(()),
            _ => Err(RequestRejection::unauthorized("missing or invalid bearer token")
                .with_header("WWW-Authenticate", "Bearer")),
        }
    })
    .bind("127.0.0.1:23080")?
    .serve()?;
```

Le **TLS**, lui, n'est pas implémenté (choix architectural : voir le comparatif ViteMCP, « OAuth 2.1 intégré » est écarté volontairement) — il vit à la périphérie, exactement comme pour tout service interne d'un SI. Le durcissement intégré couvre le protocole ; l'identité et le chiffrement sont à votre charge :

- **Liaison** : bind `127.0.0.1:23080` (comme dans tous les exemples) si le serveur ne parle qu'à des processus locaux ; sinon, exposez derrière un **reverse proxy** qui apporte TLS, l'authentification forte (OIDC/SAML, OAuth 2.1 — l'OIDC intégré au serveur reste écarté volontairement : c'est le métier du gateway) et le rate-limiting **par identité/IP** — pour l'auth token « quick fix » ou un rate-limit maison, le verrou `.on_request` suffit en process (derrière un proxy, `ctx.peer` est l'adresse du proxy : ne croyez `X-Forwarded-For` qu'après avoir vérifié le pair). Le plafond de connexions de LiteMCP est **global au serveur** (256 par défaut) : 256 clients lents peuvent l'occuper et renvoyer `503` aux autres, c'est à ça que sert le quota par IP du proxy (le TTL de connexion de 5 min borne par ailleurs la durée d'occupation d'un slot) ;
- **Navigateurs** : garde DNS-rebinding intégrée — toute requête portant un `Origin` non listé dans `.allow_origins` est refusée (`403`), sauf l'exception loopback par défaut (origine `http://` strictement égale au `Host` et au listener sur un bind loopback : client même-origine) ; `Origin: null` et `Sec-Fetch-Site: cross-site` restent toujours refusés, et les préflights CORS échouent par défaut (`OPTIONS` → `405`) ;
- **Handlers bloquants** : par défaut il n'y a pas de timeout d'exécution — un handler qui bloque retient son thread et son slot de connexion. `.tool_timeout(duration)` borne la **réponse** (résultat `isError` pour un outil, `-32603` avec data `timeout` pour une ressource/prompt passé le délai) mais ne peut pas tuer le thread du handler (Rust n'a pas d'annulation de thread) : il termine en arrière-plan et son résultat est ignoré. Pour des effets de bord annulables, encadrez vos appels lents vous-même. La **déconnexion du client** pendant l'exécution ne change rien non plus : le flux de réponse n'est ouvert qu'après calcul complet, la fermeture n'est détectée qu'à l'écriture finale (une seule écriture — aucune écriture ultérieure) et **aucun jeton d'annulation n'est exposé aux handlers** ;
- **Validation** : privilégiez `tool_typed`/`tool_parse` (schéma appliqué **avant** votre code) ; `tool` brut vous laisse la validation des arguments — c'est là que naissent les bugs d'injection côté handler, pas dans le transport. Le schéma publié est **exécuteur** : ce qu'il annonce est exactement ce que la validation applique.

Le durcissement transport est intégré et vérifié par la suite de tests : limites (corps 1 Mio, 64 en-têtes, lignes 16 Kio, 256 connexions, TTL 5 min), timeouts (lecture 30 s, deadline 60 s — slowloris coupé), anti request-smuggling (`Transfer-Encoding` ou `Content-Length` dupliqué → `400` sur **toutes** les routes, corps consommé partout pour éviter toute désynchronisation de file), parsing strict (ligne d'en-tête sans `:` ou espace avant `:` → `400`, séparateurs SP uniquement, CRLF strict, version sensible à la casse, `Accept`/`Expect` comparés par jeton exact), `Host` requis, obs-fold refusé, en-têtes à valeur unique **dupliqués** (`Origin`, `Content-Type`, `Mcp-Method`, `Mcp-Name`) refusés (`400` — le verdict ne doit pas dépendre de l'ordre), `Content-Type` vérifié (`415`), paniques de handlers contenues (`-32603` **avec l'`id` de la requête**, le serveur continue). Le transport a fait l'objet d'un audit complet (statique + dynamique sur serveur réel) ; les correctifs qui en découlent sont consignés dans le [CHANGELOG](CHANGELOG.md).

## Protocole

**Spec MCP `2026-07-28`** (stateless, sans handshake) — implémentée intégralement :

| Comportement | Réponse |
|---|---|
| `POST /mcp` valide | `200 application/json` — réponse JSON-RPC |
| `POST` avec `Accept: text/event-stream` seul | `200 text/event-stream` — un événement `data:` (SSE request-scoped) + en-têtes anti-tampon |
| Notification de protocole (`notifications/*`, sans `id`) | `202 Accepted` sans corps (acceptée et ignorée) |
| Notification d'action (`tools/call`, `resources/read`, `prompts/get`, `completion/complete` sans `id`) | `-32600` **« notification refusée »** — le mode stateless ne peut pas livrer le résultat : le handler **ne tourne pas** et le client le sait |
| `server/discover` (SEP-2575) | `200` — versions supportées, capacités, `serverInfo` dans `_meta` |
| Version de protocole dans `_meta` | `2026-07-28` acceptée · version inconnue : `-32022` (`UnsupportedProtocolVersionError`) · **absence tolérée** (compatibilité `2025-11-25`) |
| `resultType` (SEP-2322) | présent dans **chaque** résultat (`"complete"`) |
| `serverInfo` (SEP-2575) | le serveur s'identifie sous `_meta` de **chaque** résultat |
| `tools/list` — `ttlMs` / `cacheScope` (SEP-2549) | `60000` / `"private"` |
| `outputSchema` | publié dans `tools/list` **et exécuteur** : une sortie `structuredContent` non conforme devient un résultat `isError` (`.output_schema("outil", schéma)`) |
| `resources/list` / `resources/read` | `200` — catalogue et lecture, `ttlMs`/`cacheScope` comme `tools/list` (SEP-2549) ; uri inconnue/manquante : `-32602` ; handler en échec : `-32603` (data `handler_failed`) ; lecture **binaire** : `contents[].blob` (base64 — jamais un texte qui simule le binaire) |
| `resources/templates/list` | `200` — catalogue des gabarits (URI templates RFC 6570), `ttlMs`/`cacheScope` comme `resources/list` ; servi même sans gabarit (liste vide) |
| `prompts/list` / `prompts/get` | `200` — catalogue et rendu (messages `user`/`assistant`, `content` = bloc `ContentBlock` complet : texte, image, audio, `resource_link`, ressource embarquée) ; nom inconnu : `-32602` ; `arguments` non-objet : `-32602` |
| `completion/complete` | `200` — suggestions (`completion.values`, ≤ 100 : au-delà tronquées + `hasMore: true`) ; fournisseur absent ou params invalides (`ref`, `argument`) : `-32602` ; échec du fournisseur : `-32603` (data `handler_failed`) ; capacité `completions` annoncée seulement quand un fournisseur existe |
| Résultat `input_required` (MRTR) | `200` — `{ resultType: "input_required", inputRequests?, requestState, _meta }` ; rejeu (id **différent**) avec `inputResponses` + écho du jeton ; état signé HMAC-SHA256 (TTL 10 min, liaison méthode/paramètres saillants) : altéré/expiré/malformé/autre clé/requête modifiée → `-32602` avant tout handler ; requête embarquée dont la capacité client n'est pas déclarée → `-32021` ; requête embarquée invalide ou état > 8 Kio → `-32603` (bogue de handler, rien n'est émis) ; `inputResponses` sans `requestState` → `-32602` ; champs MRTR ignorés sur toute autre méthode |
| Capacités `tools` / `resources` / `prompts` | **toujours** annoncées dans `server/discover`/`initialize` — les méthodes correspondantes sont servies même à registre vide (aucune sous-capacité non servie déclarée) |
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
| En-tête `Origin` non autorisé | `403` (garde DNS-rebinding ; exception par défaut : origine loopback strictement égale au `Host` et au listener sur un bind loopback ; CLI sans `Origin` : libre) |
| Requête refusée par un verrou `on_request` | statut du rejet (`401`/`403`/`429`… — hors `400`–`599` corrigé à `400`), message en corps JSON-RPC, en-têtes supplémentaires **assainis** (nom hors jeton ou valeur à CRLF écartés), connexion fermée · hook en panique : `500` générique, le serveur continue · `/health` et les routes inconnues ne passent jamais par le verrou |
| Toute erreur JSON-RPC | l'`id` de la requête est **repris** dans la réponse (JSON-RPC 2.0 §5) |

## Benchmarks

> Règle du projet : **jamais d'adjectif sans chiffre.** Toute mesure est
> reproductible via `cargo run --release --example bench_backends` et
> `cargo run --release --example slo` (release, loopback).

**Comparatif 9 frameworks — campagne du 2026-10-25** (même démo de 4 outils, client neutre
unique, Windows 11 loopback, 3 passes) :

| | LiteMCP 0.7.1 | Meilleur concurrent |
|---|---|---|
| `tools/call` keep-alive | **0,075–0,091 ms** | 0,162–0,186 ms — tower-mcp 0.25 |
| p99 | **0,25–0,29 ms** | 0,90–1,28 ms — tower-mcp |
| Mémoire après ≈6 900 requêtes | **5,1 Mo** | 8,5 Mo — rust-mcp-sdk 2.0 |

Méthode, définitions, planchers de transport (TCP nu 0,060–0,067 ms, socket local
0,032–0,037 ms), portée exacte des chiffres et journal brut : [`docs/BENCH.md`](https://github.com/DeamonDev888/litemcp/blob/main/docs/BENCH.md) ·
graphique : [SVG des latences](https://github.com/DeamonDev888/litemcp/blob/main/docs/bench-arena-2026-10-25.svg).

### Mesures 0.5.1 — Windows 11 (rustc 1.99.1, release LTO fat/strip, loopback)

Trois passes consécutives, même machine, même session :

| Métrique | Valeur | Détail |
|---|---|---|
| `tools/call` keep-alive (moyenne) | **0,077–0,086 ms/req** | 2 000 appels, warmup 200 déduit |
| p50 / p99 keep-alive | 0,062–0,065 / **0,297–0,442 ms** | 2 000 appels |
| Connexions neuves | **1 576–1 759 conn/s** | 300 connexions |
| Binaire release strippé — **minimal** (1 outil) | **263 Kio** (269 312 o) | LTO fat + `codegen-units = 1`, mesuré depuis un projet consommateur (0.9.0 publiée) |
| Binaire release strippé — **démo `hello`** (3 registres + annotations) | **596 Kio** (610 816 o) | idem |
| Démarrage in-process (bind + 1ʳᵉ réponse) | **5,4 ms** | `examples/slo` |
| Mémoire après 2 300 requêtes | n/a hors Linux | `VmRSS` via `/proc/self/status` |

> Le p99 de 0,3–0,44 ms reflète les micro-pics du scheduler Windows en
> environnement bureautique ; les campagnes Linux du 2026-10-24 (0,031–0,033 ms
> en moyenne, p99 0,048–0,065 ms) restent la référence pour un serveur
> dédié — les conditions figées de chaque campagne sont dans [`docs/BENCH.md`](https://github.com/DeamonDev888/litemcp/blob/main/docs/BENCH.md).

### Décisions prises sur mesures

**Route maison vs backend alternatif `tiny_http`** (bench A/B du 2026-10-22,
`examples/bench_backends`) :

| Métrique | Route maison | tiny_http | Verdict |
|---|---|---|---|
| `tools/call` keep-alive | **0,10–0,11 ms/req** | 0,19–0,20 ms/req | maison ~1,85× plus rapide |
| Connexions neuves | 1 750–2 000 conn/s | 2 080–2 180 conn/s | tiny +10–25 % |
| Coût binaire | — | +360 Ko (+80 %) | maison plus légère |
| Dépendances ajoutées | 0 | +4 (ascii, chunked_transfer, httpdate, log) | maison plus légère |

**Décision tranchée par les chiffres** : la route maison reste le seul backend — plus rapide en régime keep-alive (le cas d'un agent qui parle à ses serveurs), plus légère en deps et en binaire. Le backend alternatif a été **retiré du dépôt** : le crate garde exactement deux dépendances (`serde`, `serde_json`).

**Optimisations « léger & rapide »** (A/B dos à dos, même machine et même session) :

| Métrique | 0.3.0 | Après optimisation | Verdict |
|---|---|---|---|
| `tools/call` keep-alive (moyenne) | 0,097–0,111 ms | **0,065–0,070 ms** | **~35 % plus rapide** |
| p50 / p99 keep-alive | 0,085–0,112 / 0,157–0,166 ms | **0,061 / 0,095–0,123 ms** | plus serré |
| Mémoire de lecture par connexion | 8 Ko | **4 Ko** | 1 Mio au plafond de 256 connexions |

Les leviers : réponse HTTP assemblée dans un seul buffer (**un seul write** par réponse — avant : deux syscalls, deux paquets avec nodelay), parsing d'en-têtes **zéro allocation** (`Origin`/`Host`/`Accept`/`Connection` réduits à des verdicts, sans chaînes ni `Vec`), buffers de lecture réutilisés sur toute la connexion keep-alive, **zéro clone des arguments** dans le dispatch (`params` déplacé hors du JSON parsé, handler et validation travaillent par référence), `route`/`allow_origins` partagés par `Arc`. Dépendances inchangées : toujours exactement deux (`serde`, `serde_json`).

### SLO — cibles et campagnes figées

| Métrique | Cible | Linux (Ubuntu 26.04, 2026-10-24) |
|---|---|---|
| Binaire release stripped | < 1,5 Mo | **687 Ko** ✅ |
| `tools/call` keep-alive (moyenne) | < 1 ms | **0,031–0,033 ms** ✅ |
| p99 keep-alive | < 2 ms | **0,048–0,065 ms** ✅ |
| Connexions neuves | — | **4 400–5 041 conn/s** |
| Mémoire après 2 300 requêtes | < 10 Mo | **2,7 Mio `VmRSS`, +4 Kio pendant la charge** ✅ |
| Démarrage in-process | < 10 ms | **~0,4 ms** ✅ |

`examples/slo` mesure lui-même ses bornes : démarrage in-process, 2 300 requêtes keep-alive (warmup déduit) et `VmRSS` avant/après via `/proc/self/status` (n/a hors Linux). La suite **313 tests** (dont fuzz proptest ; 304 par défaut avec les macros, 295 sans aucune feature) est vérifiée sur les deux OS.

## Comparatif — ViteMCP (TypeScript)

[`@vitemcp/server`](https://github.com/vitemcp/server) est le framework TS qui cible la **même spec** stateless 2026-07-28 — le comparatif naturel. Même serveur démo (4 outils identiques) des deux côtés, **un seul client de bench neutre** (Rust, `litemcp-it/examples/bench_remote`), Windows 11, loopback, passes alternées (campagne du 2026-10-22).

**Résumé express :**

| | LiteMCP | ViteMCP (TS) |
|---|---|---|
| ⚡ Temps de réponse moyen | **0,09 ms** | 1,43 ms |
| 😰 Le pire cas (p99) | **0,18 ms** | 5,4 ms |
| 🔌 Connexions réutilisées | **2000/2000** | 0/2000 |
| 📊 Nouvelles connexions par seconde | **~1000** | ~400 |
| 🧠 Mémoire après 2300 requêtes | **5,5 Mo** | ~417 Mo |

Détail complet (plages, passes alternées) :

| Métrique | **LiteMCP** | @vitemcp/server | Verdict |
|---|---|---|---|
| keep-alive (moyenne) | **0,09–0,10 ms** | 1,43–1,73 ms | **~15–18× plus rapide** |
| p99 keep-alive | **0,18–0,22 ms** | 5,4–5,9 ms | ~25× plus resserré |
| Connexions réutilisées | **2000/2000** | 0/2000 | flux SSE maintenu ouvert côté ViteMCP → reconnect à chaque appel |
| Nouvelles connexions acceptées **(connexions/seconde)** | **848–1257 connexions/s** | 354–498 connexions/s | **~2–3× plus de nouveaux clients par seconde** |
| Mémoire après 2 300 requêtes | **5,5 Mo** (0,9 Mo privé, reste = DLL partagées) | ~417 Mo (3 processus Node : 47 + 63 + 306) | **~75× moins de mémoire** |

L'écart structurel : LiteMCP délimite ses réponses par `Content-Length` et clôt le flux après l'événement — la connexion TCP reste proprement réutilisable ; ViteMCP répond en **SSE encadré** qu'il maintient ouvert, le client doit donc rouvrir une connexion par appel séquentiel. (Conditions : tsx/Node 24 côté TS ; les deux serveurs complétaient les mêmes requêtes JSON-RPC.)

## Hébergement VPS & proxies (le piège du tamponnage SSE)

Derrière beaucoup d'hébergements, un proxy (nginx, Cloudflare…) met les réponses en tampon et n'envoie les flux SSE que lorsqu'il a accumulé ~8 Ko — les événements « s'empilent ». Deux lignes de défense :

- **LiteMCP est structuré pour ça** : son SSE est *request-scoped* — un événement, `Connection: close`, `Content-Length` explicite. Un proxy tamponne les flux *en cours* ; une réponse terminée est toujours poussée. Le symptôme ne peut pas se produire ;
- **les réponses SSE portent les en-têtes anti-tampon** `X-Accel-Buffering: no` (honoré par nginx **par réponse**, même avec `proxy_buffering on` global) et `Cache-Control: no-cache, no-transform`.

Si vous contrôlez le proxy, la configuration propre :

```nginx
location /mcp {
    proxy_pass http://127.0.0.1:23080;
    proxy_http_version 1.1;
    proxy_buffering off;        # optionnel : X-Accel-Buffering: no suffit
    proxy_read_timeout 3600s;
}
```

**Repli officiel si l'hébergeur est hostile au SSE** : le client négocie `Accept: application/json` et reçoit une réponse JSON sèche — aucun flux, rien à tamponner. Les clients MCP gèrent cette négociation automatiquement.

**Choix architectural : pas de SSE longue durée.** Le mode stateless ne tient aucun flux ouvert (pas de progress ni de notifications serveur→client) : un POST, un événement, clôture. C'est ce choix qui immunise le transport contre le tamponnage des proxies et le rend trivial à déployer derrière nginx. Un flux permanent exigerait des sessions — hors périmètre, réévalué seulement si un besoin concret l'exige. **Conséquence assumée : pas d'annulation MCP** — la déconnexion d'un client pendant l'exécution d'un handler n'est détectée qu'à l'écriture de la réponse (une seule écriture, qui échoue alors silencieusement) et aucun jeton d'annulation n'est exposé aux handlers ; l'évolution (jeton coopératif + détection pendant l'exécution) est chiffrée dans le CHANGELOG « Prévu ».

## Tests

```bash
cargo test --workspace --all-features        # 313 tests : unitaires + intégration (vraies sockets) + doctests
cargo test --workspace                       # 304 tests : le noyau + macros incluses (jeu par défaut)
cargo test --workspace --no-default-features # 295 tests : le noyau strict, sans les macros
cargo clippy --workspace --all-targets --all-features -- -D warnings   # CI stricte
cargo fmt --check                       # formatage
```

## Journal des versions

L'historique complet et détaillé vit dans [CHANGELOG.md](CHANGELOG.md) — section **[Unreleased]** en tête : chaque modification y est consignée au fil de l'eau, et une version n'est coupée que pour un lot cohérent (voir la [politique de versioning](CHANGELOG.md#politique-de-versioning)). Le pipeline de publication (crates.io, tag `vX.Y.Z`, release GitHub) est documenté dans [`docs/RELEASE.md`](https://github.com/DeamonDev888/litemcp/blob/main/docs/RELEASE.md).

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

Pour apprendre la lib pas à pas : le [tutoriel officiel](https://deamondev888.github.io/litemcp-tuto/).

## FAQ

**Par où commencer ?**
Par le [tutoriel pas à pas](https://deamondev888.github.io/litemcp-tuto/) — il construit un serveur MCP complet avec LiteMCP, du premier `#[tool]` aux schémas, ressources et prompts. L'[Intégration](#intégration) ci-dessus suffit pour exposer vos premiers outils depuis un programme existant en dix lignes ; les [Concepts](#concepts) détaillent chaque surface ; la [doc du crate](https://docs.rs/litemcp) (docs.rs) référence aussi le tutoriel.

**Pourquoi un seul transport ?**
Parce qu'un serveur lancé comme sous-processus d'un client meurt avec lui et n'accepte qu'un seul pair — c'est le modèle du client, pas de la lib. LiteMCP fait le choix inverse : **votre programme devient un démon Streamable HTTP stateless**, bindé sur un socket TCP, que plusieurs agents peuvent joindre, qui survit aux clients et se déploie derrière nginx. Le MCP local en sous-processus est hors périmètre, par conception.

**Pourquoi un module de schémas intégré ?**
Une lib MCP sans schéma n'est qu'un routeur JSON : les clients ont besoin de l'`inputSchema` pour présenter vos outils, et vos handlers ont besoin d'arguments garantis. Le module `zod` couvre les deux — dérivation JSON Schema + validation runtime — sans dépendance de plus : il pèse moins que les bibliothèques de schémas génériques qu'on embarquerait sinon.

**Pourquoi pas de tokio ?**
Une lib MCP mono-outil n'a pas besoin d'un runtime async complet pour lire des requêtes et répondre du JSON. Le thread par connexion suffit jusqu'à des centaines de connexions (plafond par défaut : 256). Si votre application est déjà dans un runtime async, l'avez-vous simplement dans le même processus — LiteMCP reste un serveur réseau, appelable en HTTP.

**Pourquoi réécrire HTTP ?**
Le périmètre est volontairement minuscule : une route POST, `Content-Length`, keep-alive, un writer SSE mono-événement. Pas de chunked entrant, pas de TLS, pas de compression. Ce choix est **tranché par les tests** : le bench maison contre le backend alternatif testé (tiny_http) montre la route maison ~1,85× plus rapide en keep-alive et 0 dépendance de plus (voir [Benchmarks](#benchmarks)) — et elle embarque son durcissement (timeouts, limites d'en-têtes, `Expect: 100-continue`).

**Pourquoi pas de WebSocket ?**
Choix architectural, pas un oubli : WS n'économiserait que le re-parsing des en-têtes HTTP (~1–2 % du trajet — JSON, syscalls et noyau réseau dominent), ne passe pas bien derrière les proxies d'hébergement, et traînerait sessions et reconnexions — les trois choses que le mode stateless évite. La spec 2026-07-28 est HTTP-only ; le keep-alive one-write de LiteMCP est déjà au plancher de TCP.

**Pourquoi pas de SSE longue durée ?**
Le mode stateless n'a aucun flux ouvert : chaque POST est auto-suffisant (un événement `data:`, clôture). C'est ce choix qui immunise le transport contre le tamponnage des proxies de VPS et le rend trivial à déployer derrière nginx — voir [Hébergement VPS & proxies](#hébergement-vps--proxies-le-piège-du-tamponnage-sse). Un flux permanent (progress, notifications serveur→client) exigerait des sessions : hors périmètre, réévalué si un besoin concret l'exige.

**Comment sont choisies les versions ?**
Pacement volontaire : toute modification atterrit d'abord dans la section `[Unreleased]` du [CHANGELOG](CHANGELOG.md) ; une version n'est coupée que pour un lot cohérent (fonction, correctif, doc majeure). Le `0.x` court jusqu'à la conformance officielle MCP verte ; la mention « 1.0 » attend ce jalon.

## MCP Control — client, gateway et gestionnaire

Le [**MCP Control**](https://github.com/DeamonDev888/mcp-control) est le pendant côté exploitation de LiteMCP — un seul outil, trois rôles :

- **Client** : superviseur de serveurs MCP en Rust natif — détection multi-machines par scan et SSH, état, outils, latence ;
- **Gestionnaire** : une UI qui démarre, arrête, relance et installe (boutique) vos serveurs, avec démarrage automatique ;
- **Gateway** : grâce au module [`Gateway`](#fonctions-phares) de cette lib, il agrège tous les serveurs détectés derrière **un seul endpoint MCP** — côté client (Claude, Cursor, vos agents), **un seul serveur à déclarer**, et toute la surface exposée dans l'UI.

Il reconnaît les serveurs LiteMCP **par fingerprint** — `initialize` répondu en 2026-07-28 sans négocier la version demandée + sonde `GET /health` « ok » — et affiche alors un badge dédié.

**Intégration native (opt-in)** — `.announce()` sur le builder : au bind réussi, le serveur écrit une annonce JSON dans le dossier temporaire partagé (`%TEMP%/litemcp-registry/<nom>.json`, sinon `$TMPDIR` ou `/tmp`) — `{ name, version, addr, port, instance, pid, exe, args, cwd, ts }` : l'**adresse effective connectable** du listener (un bind multi-interface est publié par son bouclage — `0.0.0.0` → `127.0.0.1`, `::` → `::1` ; un bind spécifique — IPv6, IP précise — est publié tel quel), une **identité d'instance vérifiable** (`instance`, également exposée par `GET /health` en en-tête `X-Instance-Id`), le **port effectif**, le PID et la ligne de commande. MCP Control peut alors démarrer, arrêter et relancer vos serveurs nativement, sans scan de ports ni saisie manuelle. L'annonce est **retirée à la fin de vie du serveur** — arrêt gracieux ou simple `drop` après `bind` sans `serve()` — et seulement si le couple `pid`/`port` **et l'identité d'instance** (`instance`) du fichier sont ceux de l'instance appelante — un redémarrage dans le même process sur le même port publie une autre identité, dont l'annonce survit au retrait de l'ancienne ; écriture et retrait sont **sérialisés par un verrou inter-processus par nom** (dossier créé exclusivement) : une annonce plus récente ne peut plus être détruite par le retrait d'une instance plus ancienne. Le sweep anti-zombie juge l'annonce sur **preuve d'identité** : l'adresse publiée répond avec une identité différente → usurpation prouvée → retrait ; un port sans réponse ou une identité invérifiable → **conservée** (un refus TCP n'est pas une preuve de mort — un serveur vivant peut refuser, et un serveur lié à IPv6 ou à une IP précise ne répond pas sur `127.0.0.1`). Durcissements intégrés : nom de fichier sûr (`:`/`/`/`\`/`..` refusés — sous Windows `C:x` remplacerait la base du chemin et `nom:flux` écrirait un flux NTFS alternatif), écriture **atomique** (fichier temporaire créé exclusivement puis `rename` — jamais de JSON partiel lu, jamais d'écrasement à travers un symlink planté dans un dossier partagé), permissions `0600` sous Unix. Toute défaillance est silencieuse : l'annonce est une commodité d'intégration, jamais une condition de fonctionnement du serveur — et zéro coût tant que `.announce()` n'est pas appelé.

C'est un projet **sibling** : il consomme LiteMCP, il n'en fait pas partie. La lib n'ajoute **aucune** dépendance
ni code pour cette intégration — la reconnaissance repose uniquement sur des comportements déjà publics
(`initialize`, `/health`).

## Licence

`MIT OR Apache-2.0` — [LICENSE-MIT](LICENSE-MIT) · [LICENSE-APACHE](LICENSE-APACHE)
