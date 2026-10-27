# Publication — pipeline crates.io et releases GitHub

Ce document décrit le pipeline de publication d'une version de LiteMCP (crates `litemcp` et `litemcp-macros`) : prérequis, séquence locale exacte, automatisation GitHub Actions et répartition entre les workflows.

## Prérequis

- **Droits crates.io** : être *owner* (ou *maintainer* avec droit de publication) des deux crates — [`litemcp`](https://crates.io/crates/litemcp) et [`litemcp-macros`](https://crates.io/crates/litemcp-macros). La publication utilise un token d'API ([crates.io/settings/tokens](https://crates.io/settings/tokens)) : stocké comme secret GitHub `CARGO_REGISTRY_TOKEN` pour les workflows, ou enregistré localement (`cargo login`) pour une publication manuelle ;
- **Droits Git/GitHub** : pousser sur `main`, créer et pousser des tags `v*` (le push d'un tag déclenche les workflows de publication) ;
- **Toolchain** : stable récente et MSRV 1.85 respecté (`rust-version = "1.85"`, edition 2024) ;
- **CHANGELOG à jour** : la section `## [X.Y.Z] - AAAA-MM-JJ` de la version à publier doit exister dans `CHANGELOG.md` — le corps de la release GitHub en est extrait automatiquement (une section absente produit un corps de repli générique).

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
#    + commit (« X.Y.Z - … »)

# 3. Vérification des paquets : compilation, métadonnées, liste des fichiers
cargo package -p litemcp-macros --allow-dirty
cargo package -p litemcp --allow-dirty

# 4. Publication — dans cet ordre, jamais l'inverse
cargo publish -p litemcp-macros
cargo publish -p litemcp

# 5. Tag sur le commit EXACT de la version publiée, puis push du tag
git tag vX.Y.Z
git push origin vX.Y.Z   # déclenche les workflows Release et Publication
```

Deux chemins sont autorisés et aboutissent au même état : publier localement puis taguer (le workflow Release constate la version déjà en ligne et se contente de créer la release GitHub), ou taguer directement sans publier (le workflow Release fait la publication depuis la CI après son `verify` — c'est le chemin qui garantit le **gate** : aucun `cargo publish` ne part tant que fmt/clippy/tests/doc ne sont pas verts).

## `.cargo_vcs_info.json` et traçabilité

`cargo package` embarque dans le paquet `.crate` un fichier `.cargo_vcs_info.json` : le hash du commit Git au moment de l'empaquetage (et l'état `dirty` s'il y a des fichiers modifiés non commités). D'où la règle du projet : **le tag `vX.Y.Z` est posé sur le commit exact dont la version est issue** — un paquet publié reste traçable dans les deux sens (le commit mène au paquet, le paquet mène au commit, le contenu publié se reconstitue avec `cargo package` sur le commit tagué). Les `--allow-dirty` de la séquence ne dispensent donc pas de committer avant de taguer : c'est l'état commité qui est embarqué et vérifié.

## Politique de versioning du CHANGELOG

- **Flux permanent** : toute modification atterrit d'abord dans `## [Unreleased]`, au fil de l'eau ;
- **Lots cohérents** : une version n'est coupée que pour un lot cohérent — une fonctionnalité complète, un correctif notable, une étape de documentation majeure. Pas de bump rituel à chaque commit ;
- **Tag sur le commit exact** : chaque version publiée est taguée `vX.Y.Z` sur le commit dont elle est issue (`.cargo_vcs_info.json`, voir ci-dessus) ;
- Le corps de la **release GitHub** est extrait automatiquement de la section `## [X.Y.Z]` du `CHANGELOG.md` : rédigez (et commitez) la section **avant** de pousser le tag ;
- `0.x` court jusqu'à la conformance officielle MCP verte ; la mention `1.0` attend ce jalon.

## Automatisation — quel workflow fait quoi

| Workflow | Déclencheurs | Rôle |
|---|---|---|
| [`ci.yml`](../.github/workflows/ci.yml) — **CI** | push `main`, toute pull request | fmt, clippy strict, tests, doc, MSRV 1.85, advisories RustSec |
| [`release.yml`](../.github/workflows/release.yml) — **Release** | push de tag `v*`, dispatch manuel | **`verify`** (fmt + clippy + tests + doc, toutes features) → **`publish`** (crates.io, idempotent, macros d'abord) → **release GitHub** (corps extrait du CHANGELOG) |
| [`publish.yml`](../.github/workflows/publish.yml) — **Publication** | push de tag `v*`, dispatch manuel (input `version`) | publication crates.io **idempotente** (vérifie crates.io avant chaque `cargo publish`, attend l'indexation) puis notification du dépôt du tutoriel |

**Chevauchement assumé entre `release.yml` et `publish.yml`** : les deux partagent le déclencheur `tags: v*` et le groupe de concurrence `publication` (`cancel-in-progress: false`) — leurs runs sont **sérialisés** sur ce groupe, et chaque étape de publication vérifie l'API crates.io avant d'agir : le premier arrivé publie, le second constate la version déjà en ligne et saute. Une version n'est donc jamais publiée deux fois. La différence de fond : seul `release.yml` applique le **gate `verify`** avant publication et crée la **release GitHub** ; seul `publish.yml` notifie le **tutoriel**. Pour supprimer la redondance un jour, retirer le déclencheur `tags: v*` de `publish.yml` (en conservant `workflow_dispatch`, qui sert de chemin de secours manuel et de notification du tuto).

## Dépannage

- **« crate `litemcp-macros@X.Y.Z` is already uploaded »** : la macros est déjà en ligne — vérifier que la dépendance `version = "X.Y.Z"` du `Cargo.toml` de `litemcp` correspond, puis publier `litemcp` seul ;
- **« crate … is not yet indexed »** : la propagation de l'index crates.io n'est pas terminée — attendre quelques minutes et relancer `cargo publish -p litemcp` (les workflows font cette attente automatiquement) ;
- **La version existe déjà sur crates.io** : une version publiée est **immuable** — couper une nouvelle version `X.Y.(Z+1)` (nouvelle entrée CHANGELOG, nouveau tag) ;
- **Release GitHub sans corps** : la section `## [X.Y.Z]` manquait dans `CHANGELOG.md` au moment du tag — éditer la release à la main, ou re-taguer proprement après commit de la section ;
- **Le job `verify` échoue sur le tag** : ne pas republier sur le même tag — corriger sur `main`, couper la version suivante ; crates.io n'accepte pas de republication d'une version.
