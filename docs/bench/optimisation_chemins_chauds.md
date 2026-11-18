# Optimisation des chemins chauds — campagne L4 (2026-11-16)

Campagne de mesure **avant / après** du lot L4 du TODO d'audit, sur l'arbre
0.12.0 (auth comprise). Règle appliquée : mesure d'abord sur l'arbre courant,
optimisation, mesure ensuite, **même machine**, build `--release` ; une
optimisation dont le gain ne dépasse pas le bruit est documentée puis rejetée
(ou conservée avec son chiffre et son motif).

## 1. Conditions de mesure

| Élément | Valeur |
|---|---|
| Machine | Intel Core i7-7700 @ 3,60 GHz, 15,9 Gio de RAM, Windows 11 |
| Toolchain | `cargo`/`rustc` 1.99.1 (2026-08-27 / 2026-10-13), node 24.20.0 |
| Profil | `--release`, `target/` chaud, machine au repos, mesures séquentielles |
| Harnais principaux | `examples/bench_serialisation.rs` (catalogue + schémas), `docs/bench/point0-harnais-commun.rs` (point 0, copié à l'identique dans le worktree 0.11.0) |
| Comptage d'allocations | allocateur global de comptage (appels + octets), harnais compris — identique avant/après, donc les deltas sont attribuables |
| Référence 0.11.0 | `git worktree add $env:TEMP\litemcp-b350c16 b350c16` (hors dépôt, supprimé après) |

Reproductibilité — les commandes exactes :

```powershell
# 1. campagne principale (catalogues, schémas, coût de tool_timeout)
cargo run --release --example bench_serialisation --all-features

# 2. point 0 : même fichier dans les deux arbres
Copy-Item docs\bench\point0-harnais-commun.rs examples\bench_point0_commun.rs
Copy-Item docs\bench\point0-harnais-commun.rs "$env:TEMP\litemcp-b350c16\examples\bench_point0_commun.rs"
cargo run --release --example bench_point0_commun         # arbre courant
cd "$env:TEMP\litemcp-b350c16"; cargo run --release --example bench_point0_commun
```

Les journaux bruts sont dans ce dossier : `L4-avant.log`, `L4-apres.log`,
`L4-apres-point1.log`, `L4-avant-point3.log`, `L4-avant-point3-run2.log`,
`L4-apres-point3.log`, `point0-courant-avant.log`, `point0-courant-apres.log`,
`point0-0.11.0.log`, `point0-0.11.0-apres.log`, `point0-ab-interleave.log`.

Bruit de mesure constaté : ±10 à ±40 % d'un run à l'autre sur les latences médianes
(machine 4 cœurs, Windows, antivirus). Le point 0 a donc été remesuré en **tours alternés**
(A/B/A/B/A/B : worktree 0.11.0 puis arbre courant, trois tours) ; les autres comparaisons
reposent sur la médiane de 2 à 3 runs. Les débits d'allocation, eux, sont déterministes.

## 2. Point 0 — non-régression du chemin sans vérificateur

Harnais commun (20 outils à schéma `zod`, profil legacy, profil strict non
utilisé), `tools/list` et `tools/call`, modes `dispatch` (cœur seul) et
`http-json` (aller-retour HTTP réel keep-alive). Médianes en µs, allocations
par appel.

| Scénario | 0.11.0 (`b350c16`) | 0.12.0 **avant** L4 | 0.12.0 **après** L4 | Verdict |
|---|---|---|---|---|
| `tools/list` — dispatch | 11,1 µs · 13 alloc · 8 868 o | 12,5 µs · 13 · 8 868 | **1,1 µs · 8 · 7 100** | −90 % |
| `tools/list` — http-json | 82,8 µs · 15 alloc · 8 999 o | 92,1 µs · 15 · 8 999 | **67,8 µs · 10 · 7 231** | −18 % |
| `tools/call` — dispatch | 3,4 µs · 30 alloc · 3 750 o | 157,5 µs · 43 · 8 691 | **35,1 µs · 38 · 8 531** | voir ci-dessous |
| `tools/call` — http-json | 69,1 µs · 32 alloc · 3 881 o | 216,6 µs · 45 · 8 822 | **101,4 µs · 40 · 8 662** | voir ci-dessous |

**Régression trouvée, cause identifiée, corrigée en grande partie.** Entre
0.11.0 et l'arbre 0.12.0, `tools/call` était passé de 3,35 µs à **157,5 µs**
(×47) : le `tool_timeout` de 30 s par défaut (L3) faisait exécuter **chaque**
handler sur un fil système neuf — `CreateThread` coûte ~150 µs sous Windows,
soit 40× le dispatch complet. L'arbre sans fil (`in_handler()` faux, aucun
délai) n'existait plus par défaut.

Correction : **cache borné de fils de handlers** (32 fils résidents,
`src/server.rs`). Un fil rendu au repos par un appel précédent est réutilisé ;
au-delà du plafond, un fil éphémère est créé — exactement le comportement
d'avant, donc un handler bloqué ne peut pas assécher le cache et un pic de
charge n'ajoute que des fils éphémères. Aucun job n'attend un fil occupé
(l'attente entrerait dans le délai et couperait un handler innocent).

Gain mesuré : **157,5 µs → 35,1 µs** (dispatch, ×4,5) et **216,6 µs → 101,4 µs**
(http-json, ×2,1).

**Coût résiduel attribué, non supprimable sans revenir sur `tool_timeout`.**
Le reste de l'écart avec 0.11.0 est le passage de relais au fil de handler
(réveil + canal), pas le schéma ni le transport :

| Même outil, même corps | `dispatch` | `http-json` |
|---|---|---|
| 0.12.0 borné (défaut, 30 s) | 35,1 µs | 101,4 µs |
| 0.12.0 `.no_tool_timeout()` | **3,1 µs** | **71,9 µs** |
| 0.11.0 (référence) | 3,4 µs | 69,1 µs |

Autrement dit : sans borne, l'arbre courant est au niveau de 0.11.0 (±5 %) ;
avec la borne par défaut, `tools/call` paie ~31 µs de dispatch et ~30 µs de
bout en bout pour la garantie de délai. C'est un choix de conception de L3,
pas une régression accidentelle : il n'est pas annulé ici.

**Aucun octet ne change sur le fil.** Les empreintes FNV-1a 64 des charges
servies sont **identiques** entre 0.11.0 et l'arbre courant :

```text
EMPREINTE|tools/list|6296|0ab87c4318736e68     (0.11.0 et 0.12.0 après L4)
EMPREINTE|tools/call|252|fd86d5b9ffd84edc      (0.11.0 et 0.12.0 après L4)
```

## 3. Point 1 — catalogues (50 et 200 outils, ouvert et filtré)

Mode `dispatch`, vérificateur sans coût de signature (l'en-tête `x-cle` donne
les droits), droits alternés par outil (`groupe0` / `groupe1`). « ouvert » =
sans vérificateur ; « filtre-large » = les deux groupes ; « filtre-etroit » =
un seul groupe.

| Scénario | Avant | Après | Gain | Allocs/appel | Octets/appel |
|---|---|---|---|---|---|
| `tools/list` 50 — ouvert | 26,0 µs | **1,5 µs** | ×17 | 14 → 8 | 17 060 → 16 440 |
| `tools/list` 50 — filtre-large | 453 µs | **3,9 µs** | ×116 | 3 754 → 21 | 454 241 → 32 291 |
| `tools/list` 50 — filtre-etroit | 203 µs | **2,8 µs** | ×72 | 1 899 → 18 | 229 188 → 24 508 |
| `tools/list` 200 — ouvert | 97,6 µs | **2,5 µs** | ×39 | 16 → 8 | 66 212 → 62 890 |
| `tools/list` 200 — filtre-large | 2 022 µs | **9,0 µs** | ×225 | 14 858 → 21 | 1 804 441 → 125 191 |
| `tools/list` 200 — filtre-etroit | 705 µs | **5,7 µs** | ×124 | 7 453 → 18 | 904 288 → 94 183 |

Ce qui était payé : (a) sur le chemin **ouvert**, la resérialisation
`Value` → octets du catalogue à chaque requête ; (b) sur le chemin **filtré**,
une reconstruction complète de la liste — `schema.json()` par outil (clone
profond du document `zod`), clones de noms/descriptions/annotations, puis
sérialisation.

Changement : `Catalogue` (`src/server.rs`) pré-sérialise la charge `result` une
fois (découpe `prefixe` / éléments / `suffixe`, fidélité vérifiée en `debug`),
et sert :

- **chemin ouvert** : les octets mis en cache, seules `jsonrpc` et `id` sont
  écrites (`reponse_pre_serialisee`, plafond `max_response_bytes` appliqué sur
  la taille totale sans matérialiser un corps hors plafond) ;
- **chemin filtré** : réassemblage des seuls éléments visibles à partir des
  octets déjà sérialisés et des droits de l'appelant. La charge filtrée n'est
  **jamais** stockée — donc jamais servie à un autre appelant.

Preuves : `reponse_pre_serialisee_est_identique_a_success_bounded` (octets et
`Over.total` comparés pour 5 formes d'`id` × 2 charges × 3 plafonds),
`catalogue_*` (vide, nominal, un élément), et
`tests/catalogues_preselectionnes_2026_11_16.rs` (deux clés alternées, octets
figés au caractère près, plafond à l'octet sur les deux chemins).

## 4. Point 2 — allocations d'en-têtes

Audit des sites cités par l'audit, dans l'ordre :

| Site | Ce qui était alloué | Verdict |
|---|---|---|
| `src/http.rs:1743` (`Mcp-Param-*`) | `name[10..].to_ascii_lowercase()` | **conservé** : la valeur est **stockée** dans `McpHeaders::params` (suffixe normalisé, contrat public). `eq_ignore_ascii_case` ne remplace pas une valeur stockée |
| `src/server.rs:829` (`McpHeaders::from_headers`) | `str::to_ascii_lowercase()` | **conservé** : même contrat (suffixe stocké), et le chemin n'existe qu'avec un vérificateur |
| `src/server.rs:462` (`extract_mcp_param_defs`) | `to_ascii_lowercase()` | **hors chemin chaud** : appelé à l'enregistrement d'un outil, jamais par requête |
| `src/http.rs:3012` (`starts_a_request_line`) | `String::from_utf8_lossy(..)` | **supprimé** : remplacé par `is_valid_token_bytes(&[u8])` — aucun octet alloué, aucun octet substitué, verdict identique (test unitaire) |

Audit de `handle_connection` : la boucle d'en-têtes ne conserve que des
**verdicts** booléens (`Origin`, `Host`, `Accept`, `Content-Type`,
`Connection`, doublons) ; les seules `String` allouées sont des valeurs
**retenues puis relues** après la boucle (`Host` pour le verdict `421` et
l'autorité RFC 9728, `Origin` pour l'écho CORS, `Mcp-Protocol-Version` /
`Mcp-Method` / `Mcp-Name` pour le noyau). Les supprimer exigerait de déplacer
ces verdicts dans la boucle et de garder des emprunts sur un tampon réutilisé :
risque élevé sur le chemin de sécurité `Host`, gain attendu ~0,1 µs.

**Mesure** : allocations par requête keep-alive **inchangées** — 10 alloc/appel
(`tools/list`) et 40 (`tools/call`) en http-json, avant comme après ; le site
corrigé n'est atteint que sur un **résidu** en fin de corps (pipelining), hors
boucle chaude. Décision : **rejeté** pour les sites qui stockent (aucun gain
possible sans changer la forme stockée), **gardé** pour `starts_a_request_line`
(suppression d'allocation et de substitution d'octets, à coût nul).

## 5. Point 3 — `normalize_integers` / `apply_defaults`

Avant : `has_defaults()` et `has_integers()` de `FromJsonSchema` /
`FromSchemars` **parcouraient tout le document publié à chaque
`tools/call`** (deux parcours récursifs, `$defs` et branches compris), et la
première injection de défauts compilait les validateurs `jsonschema` des
branches `anyOf`/`oneOf` **pendant la requête**.

Après (`Prepared` dans `src/schemars.rs`) : document + branches + deux
booléens sont calculés une fois (à la construction pour `from_json_schema`,
à la matérialisation du schéma pour `from_schemars`), et les branches sont
compilées **dans cette même passe** — uniquement quand le document porte des
défauts, seul cas où la sélection de branche sert. Une requête ne compile plus
rien et ne parcourt plus le document pour détecter.

| Schéma `schemars`, 120 propriétés, `tools/call` | Avant | Après |
|---|---|---|
| sans défaut (détection seule) | 82,9 / 90,6 µs · 230 alloc | 72,7 µs · 226 alloc |
| avec 120 défauts (détection + injection) | 142,7 / 128,0 µs · 493 alloc | 134,1 µs · 490 alloc |

**Le gain est du même ordre que le bruit** (±10 % d'un run à l'autre). Il est
donc **conservé sans revendication de latence** : ce qui est supprimé est un
parcours O(taille du document) par requête (deux fois) et une compilation de
schéma sur le chemin chaud, ce que la mesure de bout en bout ne distingue pas
du bruit sur ce document, mais qui croît avec la taille du schéma. Aucun octet
ne change sur le fil (les tests de parité `schema_defaults_parity` et
`zod_diff_jsonschema` restent verts, plus le test de compilation à la
construction ajouté à `tests/schemars.rs`).

## 6. Décisions

| Point | Changement | Décision | Motif |
|---|---|---|---|
| 0 | Cache borné de fils de handlers (32) | **gardé** | 157,5 → 35,1 µs (dispatch), 216,6 → 101,4 µs (http) ; octets identiques |
| 1 | `Catalogue` pré-sérialisé + réassemblage filtré | **gardé** | ×17 à ×225 selon scénario ; allocations ÷ 3 à ÷ 700 ; octets figés |
| 2 | `is_valid_token_bytes` | **gardé** | supprime une allocation **et** une substitution d'octets ; verdict identique |
| 2 | `eq_ignore_ascii_case` sur les autres sites | **rejeté** | les valeurs sont stockées (contrat), pas seulement comparées ; aucune allocation évitable |
| 3 | `Prepared` : détection mémorisée + branches compilées à la construction | **gardé, sans gain de latence revendiqué** | gain dans le bruit ; supprime un parcours O(document) par requête, prouvé par test |
| — | `tool_timeout` par défaut (L3) | **non touché** | le coût résiduel est la garantie de délai ; `.no_tool_timeout()` rend les chiffres 0.11.0 |

## 7. Re-mesure indépendante du 2026-11-16 (vérification finale L6)

Re-mesure par un agent distinct de la campagne, **machine au repos** (cible de compilation
dédiée, aucun autre `cargo` en parallèle), cinq runs consécutifs de la commande de la §1,
médianes par scénario — journaux bruts : `.zwork/rapports-2026-11-13/logs-L6/bench-5runs.log`.

| Scénario | Publié (§4/§5) | Re-mesuré (médiane de 5) | Écart |
|---|---:|---:|---|
| `tools/list 50 — ouvert` | 1,5 µs | **1,4 µs** | dans le bruit |
| `tools/list 50 — filtre-large` | 3,9 µs | **3,7 µs** | dans le bruit |
| `tools/list 50 — filtre-etroit` | 2,8 µs | **2,6 µs** | dans le bruit |
| `tools/list 200 — ouvert` | 2,5 µs | **2,4 µs** | dans le bruit |
| `tools/list 200 — filtre-large` | 9,0 µs | **8,7 µs** (min 8,6, max 9,0) | dans le bruit |
| `tools/list 200 — filtre-etroit` | 5,7 µs | **5,4 µs** | dans le bruit |
| `tools/call schéma 120` | 72,7 µs | **64,8 µs** | plus rapide que publié |
| `tools/call schéma+défauts` | 134,1 µs | **123,5 µs** | plus rapide que publié |
| `tools/call petit — http-json-sans-borne` | 71,9 µs | **69,8 µs** | dans le bruit |

Les **allocations et les octets par appel sont identiques** aux valeurs publiées sur tous les
scénarios (8/21/18, 226/490, 62 890/125 191 o…), donc les gains structurels revendiqués
(÷3 à ÷700 sur les allocations) sont reproduits à l'identique.

Un premier contrôle indépendant (§8 du fragment statique L6) avait mesuré
`tools/list 200 — filtre-large` à 23,1 µs : il tournait en même temps que la matrice de tests
(`target/` partagé, compilations concurrentes). Le même scénario au repos tombe à 8,6–9,0 µs
sur cinq runs. **Conclusion : bruit de contention, aucune régression du chemin filtré** —
les facteurs de gain publiés tiennent.
