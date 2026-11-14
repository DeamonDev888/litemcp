# Schémas à la Zod — référence complète

Le module `litemcp::zod` dérive un JSON Schema (draft 2020-12) et applique **exactement** ce
document à la validation : pas de double déclaration schéma/struct, pas de dérive entre ce qui
est publié dans `tools/list` et ce qui est vérifié avant l'appel. Le README n'en garde que
l'essentiel ; cette page est la référence.

[← Retour au README](../README.md)

## Outils — schémas à la Zod

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

**Valeurs par défaut** — `.default(v)` attache une valeur par défaut à n'importe quel schéma. Elle est **publiée** (mot-clé `default` du JSON Schema) **et appliquée** : le champ devient optionnel, et la valeur est **injectée** dans les arguments avant l'exécution du handler (comme Zod). Le contrat « `null` = absence » vaut **aussi** pour un champ à défaut : le schéma publié porte `type: [T, "null"]` **avec** `default: v`, `null` est accepté et reçoit le défaut comme une absence (`{}` et `{"champ": null}` donnent la même valeur injectée). Côté `schemars`, `#[serde(default)]` sur un `Option<T>` garde la sémantique serde (`null` → `None`, `default: null` publié) : le **verdict** est identique, la valeur injectée est celle de serde.

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

## Référence des combinators zod

Chaque combinator du module `zod` publie une forme JSON Schema (draft 2020-12) et applique **exactement** cette sémantique à la validation — le schéma publié est exécuteur, sans dérogation :

| Combinator | Publié dans l'`inputSchema` | Validation appliquée |
|---|---|---|
| `object().field(n, s)` | propriété `n` + `n` listé dans `required` | présent et valide, sinon issue (`arguments.n is required`) ; doublon de nom → panique de développement |
| `object().opt(n, s)` | propriété `n`, hors `required`, `null` admis (`"type": [T, "null"]` ; un schéma objet sans `type` de premier niveau est enveloppé en `anyOf` avec la branche `null`) | absent **ou** `null` → valide (« pas de valeur ») ; présent non nul → validé par `s` ; un champ couvert par un `default` suit le même contrat (`[T, "null"]` **avec** `default`, `null` reçoit le défaut) |
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
| `<tout schéma>.default(v)` | mot-clé `default: v` ; le champ **sort de `required`** | absent → `v` **injecté** dans les arguments avant le handler (récursivement : un objet imbriqué porte ses propres défauts) ; le défaut lui-même est validé — un défaut incohérent fait échouer la validation ; `null` vaut absence (le champ publie `type: [T, "null"]`) et **reçoit le défaut** : `{}` et `{"champ": null}` sont équivalents |

Bornes communes : profondeur d'imbrication plafonnée (`MAX_DEPTH = 128` — issue « maximum nesting depth ») et collecteur d'issues borné (`MAX_ISSUES = 64` — au-delà, les problèmes sont comptés et résumés par « … and N more issue(s) omitted » : un tableau géant d'éléments invalides ne peut pas faire grossir la mémoire). Les paniques signalées ci-dessus sont des **erreurs de programmation**, levées à la construction du schéma (donc au démarrage) — jamais des entrées utilisateur.

Hors DSL (feature `schemars`) : [`from_schemars::<T>()`](SCHEMAS.md#adaptateur-schemars-feature-schemars) et `from_json_schema(document)` branchent le JSON Schema de votre choix avec la même promesse — **publié tel quel**, appliqué par le validateur `jsonschema` compilé avec les `format` **activés** (les formats inconnus restent ignorés ; leurs profils sont ceux de la référence, parfois moins stricts que le DSL `zod` — voir le tableau ci-dessus). Les entiers intégraux (`3.0`) y sont eux aussi **normalisés en entiers exacts** avant le handler (marche guidée par le document publié, `$ref` locaux résolus) : un champ `u32`/`i64`/`u64` dérivé reçoit un entier désérialisable même si le client envoie `3.0` (`1e19` arrive en `u64` exact), et le schéma publié porte pour **chaque** type entier dérivé (`u8`…`u64`, `usize`, `i8`…`i64`, `isize`) les bornes exactes du type Rust — une valeur hors plage est refusée en `-32602` par le schéma, jamais acceptée puis cassée à la désérialisation. Les `default` publiés — dérivés par `schemars` (`#[serde(default)]`) ou écrits à la main dans un document brut — y sont **injectés** dans les arguments absents : récursivement, `$ref` locaux chaînés résolus (cycles détectés, parcours borné). L'injection ne visite que la branche `anyOf`/`oneOf` que l'instance satisfait déjà (`oneOf` : uniquement si elle est la seule à la satisfaire ; `allOf` : toutes, elles s'appliquent ensemble) — aucun défaut d'une branche non sélectionnée n'arrive au handler. `from_json_schema` compile en outre son validateur **à la construction** : un document qui ne compile pas (type inconnu, `$ref` local absent, `$ref` distant) **panique** au démarrage avec l'erreur et le document dans le message, au lieu d'être publié puis de faire échouer chaque appel en `-32602`. Une erreur de schéma **interne** résiduelle (validateur exécuteur indisponible) sort en `-32603` message `internal error` — jamais `-32602`, jamais l'URL interne ni un nom de feature.
