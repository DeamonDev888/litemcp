# Publication — pipeline crates.io et releases GitHub

Ce document décrit le pipeline de publication d'une version de LiteMCP (crates `litemcp` et `litemcp-macros`) : prérequis, séquence locale exacte, automatisation GitHub Actions et répartition entre les workflows.

## Prérequis

- **Droits crates.io** : être *owner* (ou *maintainer* avec droit de publication) des deux crates — [`litemcp`](https://crates.io/crates/litemcp) et [`litemcp-macros`](https://crates.io/crates/litemcp-macros). Deux modes d'authentification, dans cet ordre :
  - **secret** `CARGO_REGISTRY_TOKEN` (token d'API [crates.io/settings/tokens](https://crates.io/settings/tokens)) — mode historique, prioritaire s'il est configuré ;
  - **trusted publishing (OIDC)** — si le secret est absent, le workflow échange le JWT OIDC de GitHub contre un **jeton crates.io éphémère** (`rust-lang/crates-io-auth-action`, révoqué en fin de job). À configurer une fois par crate sur crates.io (dépôt, fichier de workflow, environnement), puis `id-token: write` sur le seul job `publish` suffit ; aucun secret de longue durée à stocker ;
  - en local, `cargo login` reste la voie normale ;
- **Droits Git/GitHub** : pousser sur `main`, créer et pousser des tags `v*` (le push d'un tag déclenche `release.yml`, seul chemin de publication sur tag — `publish.yml` reste un chemin manuel) ;
- **Toolchain** : stable récente et MSRV 1.85 respecté (`rust-version = "1.85"`, edition 2024) ;
- **CHANGELOG à jour** : la section `## [X.Y.Z] - AAAA-MM-JJ` de la version à publier doit exister dans `CHANGELOG.md` — le corps de la release GitHub en est extrait automatiquement, et la cohérence **refuse toute publication sans elle** (gate `check`, voir ci-dessous) ;

## Pourquoi `litemcp-macros` est publiée en premier

`litemcp` dépend de sa crate macros avec une **exigence de version** (`litemcp-macros = { path = "litemcp-macros", version = "X.Y.Z" }`). crates.io refuse de publier un crate dont une dépendance versionnée n'existe pas encore sur l'index : **la macros part donc toujours la première**, et le `version = "…"` de la dépendance dans le `Cargo.toml` de `litemcp` est **aligné** sur la version qui vient d'être publiée. Après chaque publication, l'index met quelques secondes à propager — c'est pourquoi la publication de `litemcp` suit (les workflows interrogent d'ailleurs l'API crates.io et attendent l'indexation avant d'enchaîner).

## Séquence exacte (locale)

Depuis la racine du workspace, sur un arbre propre, commité et à jour de `main` :

```bash
# 1. Vérifications — exactement les étapes du job `verify` du workflow Release
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo doc --no-deps --all-features

# 2. Bump de version — litemcp-macros D'ABORD, puis litemcp
#    litemcp-macros/Cargo.toml : version = "X.Y.Z"
#    Cargo.toml (litemcp)      : version = "X.Y.Z"
#                                et litemcp-macros = { …, version = "X.Y.Z" }
#    + couper la section [Unreleased] du CHANGELOG en `## [X.Y.Z] - AAAA-MM-JJ`
#    + README : les extraits d'installation restent en forme de plage (`0.10`,
#      jamais `0.10.N`) — une version exacte vieillit dès le patch suivant et
#      contredit le badge de version
#    + commit (« X.Y.Z - … »)

# 3. Vérification des paquets : compilation, métadonnées, liste des fichiers
cargo package -p litemcp-macros --allow-dirty
cargo package -p litemcp --allow-dirty

# 4. Publication — dans cet ordre, jamais l'inverse
cargo publish -p litemcp-macros
cargo publish -p litemcp

# 5. Tag sur le commit EXACT de la version publiée, puis push du tag
git tag vX.Y.Z
git push origin vX.Y.Z   # déclenche UNIQUEMENT release.yml (canonique)
```

Deux chemins sont autorisés et aboutissent au même état : publier localement puis taguer (le workflow Release constate la version déjà en ligne et se contente de créer la release GitHub et de notifier), ou taguer directement sans publier (le workflow Release fait la publication depuis la CI après son gate — c'est le chemin canonique qui garantit le **gate** : aucun `cargo publish` ne part tant que la cohérence des versions, fmt/clippy/tests/doc et le contenu des paquets ne sont pas verts). En dernier recours — raté crates.io transitoire, republication d'une version déjà en ligne, notification à rejouer sans re-créer de tag — le **break-glass** : dispatch manuel de `publish.yml` (input `version`), qui impose le même gate `verify` que le canonique avant de publier.

## `.cargo_vcs_info.json` et traçabilité

`cargo package` embarque dans le paquet `.crate` un fichier `.cargo_vcs_info.json` : le hash du commit Git au moment de l'empaquetage (et l'état `dirty` s'il y a des fichiers modifiés non commités). D'où la règle du projet : **le tag `vX.Y.Z` est posé sur le commit exact dont la version est issue** — un paquet publié reste traçable dans les deux sens (le commit mène au paquet, le paquet mène au commit, le contenu publié se reconstitue avec `cargo package` sur le commit tagué). Les `--allow-dirty` de la séquence ne dispensent donc pas de committer avant de taguer : c'est l'état commité qui est embarqué et vérifié.

## Politique de versioning du CHANGELOG

- **Flux permanent** : toute modification atterrit d'abord dans `## [Unreleased]`, au fil de l'eau ;
- **Lots cohérents** : une version n'est coupée que pour un lot cohérent — une fonctionnalité complète, un correctif notable, une étape de documentation majeure. Pas de bump rituel à chaque commit ;
- **Tag sur le commit exact** : chaque version publiée est taguée `vX.Y.Z` sur le commit dont elle est issue (`.cargo_vcs_info.json`, voir ci-dessus) ;
- Le corps de la **release GitHub** est extrait automatiquement de la section `## [X.Y.Z]` du `CHANGELOG.md` : rédigez (et commitez) la section **avant** de pousser le tag ;
- `0.x` court jusqu'à la conformance officielle MCP verte ; la mention `1.0` attend ce jalon.

## Automatisation — quel workflow fait quoi

Tous les contrôles de publication vivent dans **`scripts/release.ps1`** (source unique, exécuté par les deux workflows sous `pwsh`) : `semver` (regex SemVer complète — toutes les combinaisons numériques, `0.10.10` inclus), `check` (cohérence dérivée de `cargo metadata` : racine = tag/input, `litemcp-macros` = exigence exacte de la dépendance racine, section `## [X.Y.Z]` du CHANGELOG ; le tag est lu dans `GITHUB_REF`, donc aucun argument optionnel vide ne peut faire disparaître le contrôle), `verify` (gate qualité : **simulations du pipeline** via `scripts/release-tests.ps1`, puis fmt workspace, clippy `-D warnings`, tests, doc, contenu des deux paquets — licences MIT/Apache, README) et `publish` (crates.io avec distinction stricte `200`/`404`/autre : une erreur réseau n'est **jamais** interprétée comme une preuve d'absence, un « already uploaded » est traité comme déjà-publié).

| Workflow | Déclencheurs | Rôle |
|---|---|---|
| [`ci.yml`](../.github/workflows/ci.yml) — **CI** | push `main`, toute pull request | fmt, clippy strict, tests, **simulations du pipeline de publication**, doc, MSRV 1.85, advisories RustSec |
| [`release.yml`](../.github/workflows/release.yml) — **Release (canonique)** | push de tag `v*`, dispatch manuel | **`verify`** (cohérence des versions PUIS simulations + fmt + clippy + tests + doc + paquets) → **`publish`** (revalidation `check` **puis** crates.io, idempotent, macros d'abord, jeton secret ou OIDC) → **release GitHub** (SEUL job avec `contents: write` ; corps extrait du CHANGELOG) → **notification du tuto** (après la release GitHub) |
| [`publish.yml`](../.github/workflows/publish.yml) — **Publication (break-glass)** | dispatch manuel UNIQUEMENT (input `version` obligatoire) — **aucun déclencheur tag** | même gate `verify` que le canonique, même publication idempotente, notification du tuto — pour republier/notifier sans tag (ex. raté crates.io transitoire) |

**Séparation stricte des deux chemins** : le push d'un tag ne déclenche **que** `release.yml` — aucun workflow ne peut plus publier sur tag avant son gate `verify` (l'ancien déclencheur `tags: v*` de `publish.yml` est supprimé ; le groupe de concurrence `publication` reste partagé pour sérialiser un break-glass manuel contre un run canonique en cours, et l'idempotence de `scripts/release.ps1 publish` écarte toute publication en double). **Attention** : GitHub ne conserve qu'**un seul** run en attente par groupe de concurrence — pousser plusieurs tags d'un coup annule les runs en file (constaté : `v0.10.3` annulé pendant que `v0.10.1` et `v0.10.2` tournaient). Poussez les tags **un par un**, et vérifiez la conclusion de chaque run avant de passer au suivant. Les actions GitHub sont épinglées par SHA de commit dans ces deux workflows. La cohérence (`cargo metadata`) est vérifiée **avant** tout `cargo publish` : un tag `vX.Y.Z` différent du manifeste racine, une `litemcp-macros` désalignée de sa dépendance ou une section CHANGELOG manquante stoppent le pipeline en secondes.

Deux garanties structurelles complètent ce gate : le job `publish` **rejoue lui-même `check`** juste avant le premier `cargo publish` (la garantie ne dépend donc ni de l'ordre des jobs ni d'un output recopié — un job `publish` rejoué seul repasse le contrôle), et la **notification ne part qu'après la release GitHub** (`needs: [publish, github-release]`) et ne se déclenche que sur un **résultat de publication réussi** (`needs.publish.result`), jamais sur un indicateur fabriqué — une republication idempotente re-notifie volontairement (cas d'usage du break-glass). Les workflows ne transmettent **jamais d'argument de version vide** (`pwsh -File script.ps1 -Param ""` n'est pas portable : Windows PowerShell 5.1 supprime l'argument vide et la liaison échoue) — le tag est dérivé de `GITHUB_REF` par le script, l'éventuel input manuel n'est ajouté que s'il est renseigné. Le jeton crates.io reste **optionnel** : `id-token: write` est confiné au job `publish` (le reste du pipeline est en `contents: read`) et l'étape OIDC est ignorée dès qu'un secret existe.

Les actions de **tous** les workflows (`ci.yml`, `release.yml`, `publish.yml`, `conformance.yml`) sont épinglées par SHA de commit — un tag peut être redéplacé, un SHA non. Ces invariants (absence de déclencheur tag, `contents: write` réservé à `github-release`, `id-token: write` réservé à `publish`, épinglage SHA, ordre publication → release → notification, revalidation avant publication) sont figés par `tests/release_pipeline.rs`, et le comportement (tag incohérent, tests rouges, version déjà publiée, réseau indisponible, contenu des paquets) par `scripts/release-tests.ps1` — tous deux **sans secret ni publication réelle**, exécutés par `release.ps1 verify` et, pour les simulations, sur chaque pull request par `ci.yml`.

## Dépannage

- **« crate `litemcp-macros@X.Y.Z` is already uploaded »** : la macros est déjà en ligne — vérifier que la dépendance `version = "X.Y.Z"` du `Cargo.toml` de `litemcp` correspond, puis publier `litemcp` seul ; les workflows traitent ce cas comme « déjà publié » et continuent ;
- **« crate … is not yet indexed »** : la propagation de l'index crates.io n'est pas terminée — attendre quelques minutes et relancer `cargo publish -p litemcp` (les workflows font cette attente automatiquement) ;
- **« statut invérifiable / crates.io injoignable »** : une erreur réseau ou un code HTTP inattendu (ni 200 ni 404) **arrête le pipeline sans publication** — l'absence d'une version n'est jamais déduite d'un échec réseau ; attendre le rétablissement et relancer le run ;
- **« OIDC / trusted publishing » en échec** : l'étape `Jeton crates.io par OIDC` ne s'exécute que si `CARGO_REGISTRY_TOKEN` est absent. Si elle échoue, vérifier la configuration *trusted publisher* de la crate sur crates.io (dépôt `DeamonDev888/litemcp`, fichier `.github/workflows/release.yml` **et** `.github/workflows/publish.yml`, environnement éventuel), ou configurer le secret `CARGO_REGISTRY_TOKEN` pour revenir au mode historique ;
- **La version existe déjà sur crates.io** : une version publiée est **immuable** — couper une nouvelle version `X.Y.(Z+1)` (nouvelle entrée CHANGELOG, nouveau tag) ;
- **Release GitHub sans corps** : impossible sur le chemin canonique — la cohérence refuse de publier sans la section `## [X.Y.Z]` ; si cela arrive malgré tout (CHANGELOG modifié entre les deux jobs), éditer la release à la main ;
- **Le job `verify` échoue sur le tag** : ne pas republier sur le même tag — corriger sur `main`, couper la version suivante ; crates.io n'accepte pas de republication d'une version.
