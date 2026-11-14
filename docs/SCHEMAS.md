# Macros et adaptateur `schemars`

Deux façons d'obtenir un schéma sans l'écrire à la main : les macros `#[tool]` / `#[resource]` /
`#[prompt]` (incluses par défaut, zéro dépendance externe) qui dérivent le schéma des types Rust,
et l'adaptateur `schemars` (feature optionnelle) qui branche n'importe quel `#[derive(JsonSchema)]`
ou un document JSON écrit à la main. Détail complet ci-dessous ; le README n'en garde que
l'essentiel.

[← Retour au README](../README.md)

## Macros `#[tool]` · `#[resource]` · `#[prompt]` (incluses par défaut)

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

`#[tool]` et `#[prompt]` valident les arguments via le schéma dérivé (`#[prompt]` passe par `.prompt_typed(…)`, la variante validée de `.prompt()`) ; `#[resource]` prend une fonction **sans paramètre** (`uri` requise). Types supportés : entiers (`i8`…`i64`, `isize`, `u8`…`u64`, `usize` — **bornes exactes publiées et appliquées** pour chacun : les non signés passent par [`zod::uint()`](ZOD.md#référence-des-combinators-zod), plage complète `[0, u64::MAX]` pour `u64` et `[0, usize::MAX]` pour `usize` selon la cible ; un négatif est refusé, jamais transformé en grand entier), `f32`/`f64`, `bool`, `String`, `Option<T>` (champ optionnel : absent **ou** `null` → `None`), `Vec<T>`. Les valeurs par défaut des paramètres entiers sont publiées **exactement** (un littéral `u64::MAX` ou au-delà de `i64::MAX`, avec ou sans suffixe, est accepté et publié en u64 ; au-delà des plages i64/u64, erreur de compilation). Le crate reste à **2 dépendances externes directes** — et `litemcp-macros` elle-même n'a **aucune dépendance externe** (pur `proc_macro` standard, sans `syn` ni `quote`). Nuance de build : un projet consommateur aux features par défaut compile tout de même `syn`, `quote`, `proc-macro2` et `unicode-ident`, tirés par `serde_derive` (`serde` avec `derive`) — ils ne viennent pas de `litemcp-macros`.

**Valeurs par défaut** — `default(nom = valeur, …)` déclaré sur la macro (Rust interdit les attribute macros sur les paramètres). Le champ devient optionnel, la valeur est **publiée** dans l'`inputSchema` **et injectée** dans les arguments si elle est absente ; `null` vaut absence et **reçoit le défaut** (`{}` et `{"limit": null}` sont équivalents) — la macro émet `.opt(nom, schéma.default(valeur))`, donc hérite du contrat décrit dans [`ZOD.md`](ZOD.md#référence-des-combinators-zod).

```rust
#[tool(description = "Recherche", default(limit = 10, mode = "fast"))]
fn recherche(query: String, limit: u64, mode: String) -> litemcp::ToolResult {
    Ok(serde_json::json!({ "query": query, "limit": limit, "mode": mode }))
}
// sans "limit" dans les arguments → limit = 10 ; "limit": 42 → 42
```

## Adaptateur `schemars` (feature `schemars`)

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

Utilisable partout où l'API attend `impl zod::Schema` (`tool_typed`, `tool_parse`, `prompt_typed`, `output_schema`). Sémantiques JSON Schema de bout en bout (le validateur officiel accepte par ex. `3.0` pour `integer` — et litemcp **normalise** ces intégraux en entiers exacts avant le handler, comme pour `zod::integer()` : `as_i64()`/désérialisation serde réussissent toujours, parce que l'adaptateur complète les bornes que schemars omet pour **tous** les formats entiers qu'il publie — `uint8`…`uint64`, `uint` (`usize`), `int8`…`int64`, `int` (`isize`, selon la cible) : le schéma publié porte les bornes exactes du type Rust, une valeur hors plage est donc refusée en `-32602` par le schéma au lieu de passer puis de casser à la désérialisation, et une borne **déjà publiée** n'est jamais écrasée (un `#[schemars(range(min = 1, max = 100))]` sur un `u8` reste `[1, 100]`)). `u64` et `usize` suivent le contrat de [`zod::uint()`](ZOD.md#référence-des-combinators-zod) : le schéma publié porte `minimum: 0` **et** `maximum: u64::MAX` / `usize::MAX` (schemars omet la borne haute) ; une valeur hors plage ou non entière est refusée en `-32602` et `1e19` arrive en `u64` exact au handler. Limite de précision inhérente à JSON : un littéral entier hors de la plage `i64` distant de moins d'une unité de la borne (`-9223372036854775809`) est lu en `f64` par `serde_json` **avant** la validation et arrondit à `i64::MIN`, qui passe la borne — la précision est perdue avant que le schéma ne s'applique ; les bornes restent exactes pour toute valeur représentable en `f64` sans arrondi. Les `format` (`email`, `uuid`, `date-time`, `uri`…) sont **appliqués** selon les validateurs de la référence `jsonschema` — parfois moins stricts que le DSL `zod` (voir [`ZOD.md`](ZOD.md#référence-des-combinators-zod)). Deux deps de plus, uniquement si la feature est activée.

**Schéma JSON brut** — pour les schémas que le DSL n'exprime pas (`oneOf`, `not`, `patternProperties`, `if`/`then`/`else`, `$ref` internes…), `from_json_schema` prend un document écrit à la main : il est **publié tel quel** et **appliqué** par le même validateur. Le validateur est compilé **à la construction** : un document qui ne compile pas (type inconnu, `$ref` local absent, `$ref` distant) **panique** au démarrage avec l'erreur et le document dans le message, au lieu d'être publié puis de faire échouer chaque appel. Un booléen JSON Schema reste un document valide, mais l'`inputSchema` **racine** d'un outil MCP doit admettre un objet : l'enregistrement (`tool_typed`/`tool_parse`) refuse une racine `true`/`false` **et** une racine dont les mots-clés prouvent qu'elle n'admet aucun objet (`{"type":"string"}`, `{"enum":["a","b"]}`, une énumération dérivée par schemars) — les schémas booléens et non-objet restent acceptés partout où JSON Schema les autorise (`properties.*`, `items`…). Une erreur de schéma **interne** (validateur exécuteur indisponible) sort en `-32603` message `internal error` — jamais `-32602`, jamais l'URL interne ni un nom de feature.

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

## Défauts connus (audit du 2026-11-13)

Relevés par les sondes de l'audit (`.zwork/rapports-2026-11-13/sonde-schemas`), **tous corrigés** par le lot C1 (2026-11-13), puis par le lot D1 pour les deux résidus (bornes de tous les entiers, `Option<T>` + `default`) — le tableau garde la trace du défaut et du comportement retenu :

| # | Défaut (avant correctif) | Effet (avant correctif) | Comportement actuel |
|---|---|---|---|
| M1 | les `default` étaient injectés dans **toutes** les branches `allOf`/`anyOf`/`oneOf`, pas seulement celle qui s'applique | une instance **valide** devenait invalide et recevait `-32602` : `{"a":5}` valide pour un `oneOf` de deux objets en `additionalProperties:false` était refusé après ajout du `b` de l'autre branche | seule la branche que l'instance satisfait déjà reçoit ses défauts (`oneOf` : uniquement si elle est la seule à la satisfaire ; `allOf` : toutes) — `src/schemars.rs:586` |
| M2 | `u64` n'était normalisé que jusqu'à `i64::MAX` et le schéma publié n'avait pas de `maximum` | `1e19` arrivait en `f64` au handler, `2^64` était accepté | bornes publiées pour **tous** les formats entiers de schemars (`uint8`…`uint64`, `uint`, `int8`…`int64`, `int`), sans écraser une borne existante ; `1e19` livré en `u64` exact, hors plage ou non entier refusé en `-32602` |
| M3 | `Option<T>` : la macro et `.opt()` refusaient `null`, `from_schemars` l'acceptait | deux verdicts pour la même sémantique selon la porte d'entrée | `null` et l'absence valent « pas de valeur » par les trois chemins (`.opt()` publie `"type": [T, "null"]`) — **y compris pour un champ à `default`** : publié `[T, "null"]` **avec** le `default`, `null` reçoit le défaut comme une absence ; côté `schemars`, `#[serde(default)] Option<T>` garde la sémantique serde (`null` → `None`) : même verdict, valeur injectée propre à chaque adaptateur |
| M4 | `from_json_schema` publiait un document invalide tel quel | chaque appel rendait `-32602 « invalid internal schema »` sans explication exploitable | validateur compilé **à la construction** : document invalide (type inconnu, `$ref` local absent ou distant) → panique au démarrage — `src/schemars.rs:130` |
| M5 | un `inputSchema` racine non-objet (`{"type":"string"}`) était accepté à l'enregistrement | l'outil était annoncé, puis **tous** ses appels échouaient | toute racine dont les mots-clés prouvent qu'elle n'admet aucun objet est refusée (panique à l'enregistrement) ; une erreur de schéma **interne** sort en `-32603` message `internal error` — `src/server.rs:491`, `src/server.rs:608` |

Le DSL `zod` est concerné par M3 (`.opt()`) ; M1, M2, M4 et M5 ne touchent que la voie `schemars`/document brut. Le différentiel `tests/zod_diff_jsonschema.rs` continue de couvrir la sémantique `zod` face au validateur de référence.