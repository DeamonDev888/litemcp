# Changelog

Toutes les évolutions notables de ce projet sont documentées dans ce fichier.
Format : [Keep a Changelog](https://keepachangelog.com/) — versioning : SemVer.

## Politique de versioning

- **Flux permanent** : toute modification (fonction, correctif, doc) atterrit d'abord dans la section **[Unreleased]** ci-dessous, au fil des commits.
- **Pacement** : une version n'est coupée que pour un **lot cohérent** — une fonctionnalité complète, un correctif notable, ou une étape de documentation majeure. Pas de bump rituel à chaque commit.
- **0.x** jusqu'à la conformance officielle MCP verte (suite 2026-07-28, mode serveur, 0 FAILURE) ; la mention **1.0** attend ce jalon.
- Chaque version publiée est taguée `vX.Y.Z` sur le commit exact dont elle est issue (`.cargo_vcs_info.json` vérifié avant `cargo publish`).

## [0.3.1] - 2026-10-22

### Ajout — point fort repris de ViteMCP

- **Sonde `GET /health`** → `200` « ok » sans JSON-RPC (`HEALTH_ROUTE`, inspiration ViteMCP) : probes k8s/systemd/équilibreur ; compatible keep-alive (la sonde pipelinée ne casse pas la connexion) ; autre méthode → `405` avec `Allow: GET`. `GET /mcp` reste `405`.

### Comparatif ViteMCP (TypeScript) — 2026-10-22

- Premier face-à-face avec [`@vitemcp/server`](https://github.com/vitemcp/server) — le framework TS ciblant la même spec stateless 2026-07-28. Même serveur démo (4 outils) des deux côtés, client de bench neutre unique (`litemcp-it/examples/bench_remote`), Windows 11 loopback, passes alternées :
  - keep-alive : **0,09–0,10 ms** vs 1,43–1,73 ms (**~15–18× plus rapide**) ;
  - p99 : **0,18–0,22 ms** vs 5,4–5,9 ms ;
  - connexions neuves : **848–1257 conn/s** vs 354–498 conn/s ;
  - réutilisation : **2000/2000** vs 0/2000 — ViteMCP répond en SSE encadré maintenu ouvert, le client doit rouvrir une connexion par appel séquentiel ; litemcp délimite par `Content-Length` et clôt le flux ;
  - mémoire après 2 300 requêtes : **5,5 Mo** (0,9 Mo privé) vs ~417 Mo (chaîne de 3 processus Node : 47 + 63 + 306 Mo) — **~75× moins**.
- Constat de fiabilité : `enableJsonResponse` est déclaré dans les types de `@vitemcp/server` mais sans implémentation dans le code — l'option est un no-op.
- Banc reproduisible, hors dépôt : un projet TypeScript (`@vitemcp/server` + tsx) d'un côté, le banc externe d'intégration de l'autre (serveur démo 4 outils + client de bench neutre `bench_remote`).

### Performance — lot « le plus léger, le plus rapide »

- **Latence keep-alive ~35 % meilleure** (A/B dos à dos, même machine et même session : code 0.3.0 0,097–0,111 ms → code optimisé 0,065–0,070 ms/req ; p50 0,061 ms, p99 0,095–0,123 ms) :
  - réponse HTTP assemblée dans un seul buffer → **un seul write** par réponse (avant : deux syscalls et, avec nodelay, deux paquets) ;
  - parsing d'en-têtes **zéro allocation** : noms comparés `eq_ignore_ascii_case` (plus de minuscules allouées), `Origin`/`Host` réduits à des verdicts booléens, `Accept`/`Connection` sans chaînes ni `Vec` temporaires ;
  - ligne de requête et en-têtes lus dans des buffers **réutilisés** sur toute la connexion (keep-alive inclus) ;
  - **zéro clone des arguments** dans le dispatch : `params` et `id` sont déplacés hors du JSON parsé (`Map::remove`), et handler comme validation reçoivent les arguments par référence — `tool_parse` seul clone encore, pour la désérialisation typée ;
  - `route` et `allow_origins` partagés par `Arc` (plus de clone par connexion sur le chemin d'accept) ;
  - `Outgoing.jsonrpc` en `&'static str` (une allocation de moins par réponse).
- **Binaire release −18,5 %** (483 Ko → 385 Ko) : `lto = "fat"` + `codegen-units = 1` dans le profil release du dépôt (les consommateurs gardent la main sur leur propre profil).
- **Mémoire de lecture par connexion divisée par deux** : buffer `BufReader` 4 Ko (1 Mio au plafond de 256 connexions) — un corps plus grand que la capacité est lu directement depuis la socket.
- Bench `examples/bench_backends` : warmup déduit et percentiles **p50/p99** en plus de la moyenne — passes comparables d'une campagne à l'autre.
- **Confirmation indépendante côté consommateur** : banc externe (`litemcp-it`, serveur démo de 4 outils important le crate depuis crates.io vs `path` local) — keep-alive 0,102–0,108 ms (0.3.0 publié) → **0,087–0,091 ms** (lot en cours), plages sans chevauchement, **~13 % plus rapide** sur ce serveur plus riche ; les 12 tests d'intégration du banc passent à l'identique dans les deux modes.
- Docs : section « Pourquoi litemcp » réécrite (bibliothèque vs service expliqué simplement, cas d'usage introduits).

### Tests

- 70 tests au total (19 unitaires JSON-RPC/zod/protocol, 42 HTTP sur vraies sockets, 5 smoke, 4 doctests).

## [0.3.2] - 2026-10-22

### Ajout — anti-tampon SSE (réponse au problème des VPS derrière un proxy)

- **En-têtes anti-tampon sur toute réponse SSE** (`SSE_ANTI_BUFFERING`) : `X-Accel-Buffering: no` — la directive **par réponse** que nginx honore même quand `proxy_buffering` est actif globalement — et `Cache-Control: no-cache, no-transform`. Le symptôme « les événements s'empilent jusqu'au seuil du buffer » ne peut de toute façon pas se produire (SSE request-scoped : un événement, `Connection: close`, `Content-Length` explicite — un proxy pousse toujours une réponse terminée) ; les en-têtes ferment le dossier pour les proxies les plus zélés. JSON non affecté (2 tests : présents en SSE, absents en JSON).
- **Choix architectural documenté** : **pas de SSE longue durée** (progress, notifications serveur→client) — le mode stateless ne tient aucun flux ouvert ; documenté dans le code (contrat : tout écrivain SSE flush après chaque événement et porte `SSE_ANTI_BUFFERING`) et dans le README.
- README : nouvelle section « Hébergement VPS & proxies » (explication du tamponnage, recette nginx, repli JSON officiel, choix architectural), FAQ « Pourquoi pas de WebSocket ? » (choix architectural : ~1–2 % de gain au mieux, proxies hostiles, sessions), table Configuration avec la sonde `/health`.

### Tests

- 72 tests au total (19 unitaires JSON-RPC/zod/protocol, 44 HTTP sur vraies sockets, 5 smoke, 4 doctests).

## [Unreleased]

### Zod — durcissement & optimisation (audit du module, 2026-10-24)

- **Bornes cassées impossibles à publier** : `min > max` (integer/string/number/array), bornes non finies (`NaN`/±∞ sur `number()` — `serde_json` les aurait publiées en `null`, borne jamais appliquée), `enumeration` vide (insatisfaisable) ou doublonnée (meta-schema JSON Schema exige des éléments uniques) → **panique de développement au démarrage**, comme le doublon de champ ;
- **Chemin de validation zéro allocation** (`zod::Path`) : le chemin était alloué (`format!`) par champ et par élément de tableau **même quand tout était valide** (12 allocations pour un appel valide à 2 champs + 10 éléments) — désormais un buffer unique étendu/tronqué, une chaîne n'est construite qu'en émettant une issue. *Breaking* : `Schema::validate` prend `&mut Path` au lieu de `&str` (les implémentations custom de `Schema` doivent s'aligner) ;
- **`tools/list` et `server/discover` ne resérialisent plus rien** : le catalogue est immuable après `bind()`/`core()` — le JSON est calculé une fois (`OnceLock`) puis servi tel quel.

### Ajout — zod et écosystème

- **Formats de chaîne exécutés** : `.email()`, `.uuid()`, `.url()`, `.datetime()` — publiés `format` (`email`/`uuid`/`uri`/`date-time`, mots-clés JSON Schema 2020-12) **et appliqués** (`date-time` vérifie le calendrier : jours par mois, années bissextiles, fuseau obligatoire RFC 3339 ; aucun regex, aucune dépendance) ;
- **`zod::nullable(schéma)`** : accepte `null` en plus du schéma interne, publié `anyOf: [schéma, {"type":"null"}]` — compose avec tout, sans changer la cardinalité (`.field` reste requis, `.opt` optionnel) ;
- **`zod::describe("…", schéma)`** : description publiée dans le JSON Schema ;
- **Feature opt-in `schemars`** : l'adaptateur `from_schemars::<T>()` rend n'importe quel type `#[derive(JsonSchema)]` utilisable partout où l'API attend `impl zod::Schema` — le schéma publié est celui de schemars, la validation applique **le même document** via le validateur `jsonschema` (erreurs mappées en issues bornées, chemins lisibles). Deux deps de plus (`schemars`, `jsonschema`), uniquement si activée ; le cœur reste à `serde` + `serde_json`.

### Documentation
- Lien réciproque vers le [MCP Command Center](https://github.com/DeamonDev888/mcp-command-center) — client qui détecte les serveurs litemcp par fingerprint (badge dédié) — nouvelle section « Supervisé par MCP Command Center ».

### Prévu (suite)

- Tests clients réels (MCP Inspector, Claude, Cursor) et corpus de fuzz dédié au parseur.
- `zod::record(inner)` (dictionnaires `{"clé": valeur}`).

## [0.4.1] - 2026-10-24

### Ajout — suite de la feuille de route

- **Macros `#[resource]` et `#[prompt]`** (feature `macros`) : même mécanique de dérivation que `#[tool]`. `#[resource]` prend une fonction sans paramètre (attributs `uri` requise, `name`, `description` requise) et génère l'enregistrement `.resource(…)` ; `#[prompt]` dérive le schéma des types des paramètres et valide les `arguments` d'un `prompts/get` **avant** l'exécution du handler.
- **`.prompt_typed(name, description, schéma, handler)`** : la variante validée de `.prompt()` — hors schéma → `-32602` avec `error.data.issues`, le handler ne tourne jamais (même contrat que `tool_typed`).
- **Fuzz du transport HTTP (proptest, dev-dépendance uniquement)** : trois propriétés sur serveur réel — (1) des octets arbitraires ne produisent que des réponses HTTP **complètes et correctement cadrées** (status line, en-têtes terminés, `Content-Length` exact) ou rien ; (2) des requêtes « réalistes » (verbes/chemins plausibles, en-têtes piégeux : duplications, obs-fold, `Transfer-Encoding`, `Content-Length` menteurs, séparateurs TAB) respectent le même invariant et le serveur **survit** à chaque cas ; (3) toute requête JSON-RPC valide reçoit exactement une enveloppe bien formée (`jsonrpc: "2.0"` + `result`|`error`, code numérique). 144 cas générés par exécution de la suite.

### Mesures — campagne SLO Linux (conditions figées)

**Suite complète sur Linux** : 134/134 tests (Ubuntu 26.04 LTS, kernel 7.0.0-38, 8 cœurs, rustc 1.99.1) — y compris le fuzz proptest et les sockets. Nouvel exemple **`examples/slo`** (démarrage in-process, 2 300 requêtes keep-alive warmup déduit, `VmRSS` avant/après via `/proc/self/status`) : la campagne est reproductible par n'importe qui (`cargo run --release --example slo`).

| Métrique | Windows 11 | Linux (Ubuntu 26.04, 8 cœurs) |
|---|---|---|
| keep-alive moyenne / p99 | 0,065–0,070 / 0,095–0,123 ms | **0,031–0,033 / 0,048–0,065 ms** |
| Connexions neuves | 848–1 257 conn/s | **4 400–5 041 conn/s** |
| Mémoire après 2 300 requêtes | 5,5 Mo (WorkingSet) | **2,7 Mio `VmRSS`, +4 Kio pendant la charge** |
| Démarrage in-process (bind + 1ʳᵉ réponse) | — | **~0,4 ms** |
| Binaire release stripped (hello) | 385 Ko | 687 Ko |

### CI — protection de la branche `main`

- Workflow : groupe `concurrency` (les runs supplantés d'une même branche/PR sont annulés) ; les jobs `test`/`msrv`/`advisories` se déclenchent automatiquement sur **tout push et toute pull request**.
- Protection de branche `main` : les trois checks CI sont **requis**, une **approbation** est exigée avant merge (reviews périmées rejetées sur nouveau push), histoire linéaire requise — les admins conservent l'accès direct.

## [0.4.0] - 2026-10-24

### Ajout — le lot « gros ROI » de la feuille de route

- **Arrêt propre de `serve()`** (ferme le point B16 des audits) : `bound.handle()` rend une `ServerHandle` cloneable ; `handle.shutdown()` (idempotent) réveille la boucle d'`accept` par une socket factice → `serve()` rend `Ok(())`, et chaque connexion vivante se ferme **entre deux requêtes** (une lecture bloquée en cours ferme au `read_timeout`). La requête en cours termine toujours proprement — les threads ne sont pas tués.
- **Timeout d'exécution des handlers** (`.tool_timeout(duration)`, défaut illimité) : un handler d'outil qui dépasse rend un résultat `isError` (le résultat timeout reste un `result` complet : `resultType`, `_meta`), un handler de ressource/prompt une erreur `-32000` (plage implementation-defined). **Limite assumée et documentée** : le thread du handler n'est pas tué (Rust n'a pas d'annulation de thread) — il termine en arrière-plan, résultat ignoré ; en mode borné, une panique de handler est rendue en `-32603` et les arguments d'un `tools/call` sont clonés une fois par appel.
- **`outputSchema`** (`.output_schema("outil", schéma)`) : publié dans `tools/list` et **exécuteur** comme l'input — la valeur `structuredContent` réellement envoyée est validée avant de partir ; non conforme → résultat `isError` qui cite le `outputSchema`. Ordre d'appel indépendant (même mécanique que `.annotate()`).
- **Resources** : `.resource(uri, name, description, handler)` — `resources/list` et `resources/read` implémentées avec les indices SEP-2549 (`ttlMs`/`cacheScope`) et l'identification SEP-2575 (`_meta` serverInfo) ; uri inconnue → `-32602`, handler en échec → `-32000`.
- **Prompts** : `.prompt(name, description, handler)` — `prompts/list` et `prompts/get` (messages `user`/`assistant` via `PromptMessage::user`/`::assistant`), `arguments` objet exigé (même contrat que `tools/call`).
- Les **capacités** `resources`/`prompts` sont annoncées dans `server/discover` et `initialize` seulement quand leur registre n'est pas vide (une capacité annoncée est une capacité servie).
- **Macro `#[tool]`** — nouvelle crate `litemcp-macros` (workspace), feature opt-in `macros` : le schéma d'entrée est **dérivé des types des paramètres** d'une fonction ordinaire (`i64`→integer, `f64`→number, `String`→string, `Option<T>`→champ optionnel, `Vec<T>`→array), la macro génère `register_<fonction>` à enchaîner via `.with_tool(…)`. Le cœur garde exactement **2 dépendances** : `syn`/`quote` ne sont que des deps de compilation activées par la feature. Aucune exigence nouvelle pour le consommateur (pas besoin de `serde` direct : l'extraction est générée, pas désérialisée).
- `ToolResult` ré-exporté à la racine (`litemcp::ToolResult`) — le type de retour documenté de la macro.
- CI alignée : `--workspace --all-features` sur clippy/test, job MSRV sur le workspace.

### Sécurité & conformité — audit du 2026-10-24

Audit complet (statique + sondes dynamiques + vérification du changelog officiel MCP `2026-07-28` en ligne). **Aucune vulnérabilité critique ou haute** ; les correctifs de l'audit du 2026-10-23 sont vérifiés présents et couverts. Rapport : `docs/AUDIT-LITEMCP-2026-10-24.md`. Les 6 nouveaux constats (P3/Info) sont corrigés :

- **[P3] `serverInfo` dans chaque résultat** (SEP-2575 : les serveurs *SHOULD* s'identifier dans le `_meta` de chaque résultat) : `tools/list` et `tools/call` — succès **et** `isError` — portent désormais `_meta["io.modelcontextprotocol/serverInfo"]`, comme `server/discover` et `initialize` ;
- **[P3] `ping` porte `resultType` + `_meta`** : la promesse « `resultType` dans chaque résultat » devient vraie sans exception (méthode legacy, résultat enrichi — additif pour les clients `2025-11-25`) ;
- **[P3] En-têtes à valeur unique dupliqués refusés** (`Content-Type`, `Mcp-Method`, `Mcp-Name` → `400`) : le verdict dépendait de **l'ordre** (prouvé : `Content-Type: application/json` puis `text/plain` → `415`, ordre inverse → `200` ; `Mcp-Method: tools/list` + `Mcp-Method: ping` accepté silencieusement) — dernier-gagne divergent d'un proxy qui combine les valeurs ou retient la première. Même traitement que `Origin`/`Host` ;
- **[P3] Route validée au bind** : `.route()` doit commencer par `/`, ne pas contenir de `?` et ne pas masquer `/health` — sinon `InvalidInput` au `bind` au lieu de démarrer un serveur **silencieusement injoignable** (prouvé : route `/health` → tout `POST` prenait le `405` de la sonde) ;
- **[P3] `structuredContent` : justification périmée corrigée** — l'enveloppe objet (`{ "result": … }` pour les non-objets) est désormais documentée comme un **choix d'API** ; la spec `2026-07-28` (SEP-2106) autorise en effet toute valeur JSON, l'exigence d'objet date d'avant. Comportement inchangé, promesse honnête ;
- **[Info] CI** : `cargo clippy --all-targets -- -D warnings` (les tests et exemples sont désormais lintés en CI, comme localement) + job `msrv` sur la toolchain 1.85 déclarée dans `Cargo.toml`. Au passage, le commentaire du job `advisories` est rendu exact : `Cargo.lock` n'étant pas versionné (convention lib), `cargo-deny` résout les dépendances à la volée — la vérification porte sur les versions compatibles les plus récentes, pas sur un lockfile committé ;

### Tests

- Avec le lot « gros ROI » ci-dessus : **128 tests au total** (27 unitaires, 8 features, 80 HTTP sur vraies sockets, 3 macros, 5 smoke, 5 doctests) — 15 nouveaux pour verrouiller les cinq fonctionnalités (timeout, outputSchema, resources, prompts, arrêt propre, macro).


## [0.3.3] - 2026-10-23

### Conformité MCP 2026-07-28 — la spec est maintenant implémentée **entièrement**

La lib annonçait viser la spec `2026-07-28` mais implémentait encore la surface *legacy* `2025-11-25`. La spec `2026-07-28` a changé de modèle (stateless, sans handshake, version par requête) — l'écart est désormais comblé :

- **`server/discover`** (SEP-2575, **MUST**) : implémentée — annonce `supportedVersions`, `capabilities` et `serverInfo` sous `_meta["io.modelcontextprotocol/serverInfo"]` ;
- **`resultType` obligatoire** (SEP-2322) : présent dans **chaque** résultat (`"complete"`) ;
- **`ttlMs` / `cacheScope`** (SEP-2549) : présents sur `tools/list` (`60000` / `"private"`) ;
- **Gestion de version** (SEP-2575) : la version est lue dans `_meta["io.modelcontextprotocol/protocolVersion"]` ; une version **explicite** non supportée est refusée en `-32022` (`UnsupportedProtocolVersionError`) avec `data.supported` / `data.requested`. L'**absence** de version est tolérée (compatibilité `2025-11-25`) ;
- **En-têtes de routage** (SEP-2243) : `Mcp-Method` et `Mcp-Name` sont acceptés et **vérifiés contre le corps** de la requête ; incohérents → `-32020` (`HeaderMismatch`) ;
- `initialize` et `ping` (retirées de la spec) restent **tolérées** comme extensions legacy.

### Sécurité — correctifs de l'audit du 2026-10-23

Audit statique (code intégral) + revue indépendante croisée + **vérifications dynamiques sur serveur réel** (build release, config par défaut). Un **P1** et quatre **P2** corrigés :

- **[P1] Désynchronisation de la file sur `/health`** : la branche `health_probe` répondait **sans consommer le corps**. Le corps résiduel devenait la requête suivante — un `GET /health` avec un `Content-Length` et un corps contenant une `POST /mcp` produisait **2 réponses** et **exécutait le corps caché** (reproduit : `id: 42`). Correctif : **un corps est refusé sur la sonde** (`400`, fermeture) — plus simple et plus sûr qu'un drain, qui aurait obligé à attendre des octets pouvant ne jamais arriver (un `Content-Length: 1048576` sans corps retenait un slot jusqu'à la deadline de 60 s, et `Expect: 100-continue` provoquait un blocage croisé) ;
- **[P1] `Transfer-Encoding` refusé APRÈS le routage** : `TE: chunked` sur `/health` était **accepté** (200) et les octets chunked résiduels étaient relus comme requête. Le contrôle est désormais appliqué **avant** le routage, donc sur toutes les routes ;
- **[P2] Le `503` « trop de connexions » était perdu** : le `close()` partait avec la requête du client **non lue** dans le tampon de réception → RST → le tampon du client (qui contient le 503) était purgé. Mesuré : **4/20 réponses lues**. Correctif : drain borné (64 Kio / 50 ms) + `shutdown(Write)`, et le refus est écrit **hors de la boucle d'`accept`** (thread court, borné à 64 refus simultanés) — mesuré : **15/15** lues, latence médiane par refus **1,4 ms** (contre 240 ms de blocage de la boucle auparavant). Le test existant passait pour la mauvaise raison (il n'envoyait rien avant de lire) ;
- **[P2] Amplification mémoire ×9 800** : le corps était pré-alloué et zéroé en entier avant la moindre lecture (`vec![0u8; len]`). 256 clients annonçant 1 Mio sans envoyer de corps consommaient **264 Mio** de commit pour 28 Kio reçus. Lecture désormais **par tranches** (buffer 64 Ko réutilisé) → **+24 Mio** mesurés (×864) ;
- **[P2] TTL de connexion** : le deadline par requête était réinitialisé à chaque requête keep-alive — un client qui drip-feed des requêtes valides (sous le `read_timeout`) retenait son slot **indéfiniment**, et 256 clients lents suffisaient à saturer le plafond durablement. Nouvelle option `.connection_ttl(duration)` (défaut **5 min**) ;
- **[P2] L'`id` JSON-RPC était perdu sur les erreurs** : toute réponse `-32600` portait `id: null`, comme la réponse de panique. L'`id` est désormais extrait *best-effort* **avant** la validation et repris dans la réponse (JSON-RPC 2.0 §5) ;
- **[P2] `arguments` non-objet accepté** : un outil sans schéma recevait un scalaire/tableau/`null` en violation du contrat de l'API (« la closure reçoit `arguments` (objet JSON) »). Refusé en `-32602` dans **tous** les cas ;
- **[P3] Schéma Zod exécuteur** : `integer()` acceptait `3.0`/`1e2` alors que `Value::as_i64()` y renvoie `None` — le motif documenté `args["a"].as_i64().unwrap()` **paniquait** sur une valeur que le schéma déclarait valide. `integer()` n'accepte plus que les entiers JSON **syntaxiques**, et publie ses bornes exactes (`minimum`/`maximum`) : ce que le schéma annonce est **exactement** ce que la validation applique — aucune dérogation possible ;
- **[P3] Doublon de nom de champ** : `Obj::field`/`Obj::opt` ne refusaient pas les doublons → `required: ["a","a"]` (JSON Schema invalide) et un outil insatisfaisable. Panique de développement au démarrage ;
- **[P3] Parsing strict** : séparateurs de la ligne de requête = **espaces ASCII (SP) uniquement** (le HTAB et les caractères de contrôle sont refusés) ; fin de ligne **CRLF stricte** (le LF seul — vecteur *httpoxy* — est refusé) ; `Accept` et `Expect` comparés **par jeton exact** (`text/event-stream-x` ne vaut pas `text/event-stream` ; `not-100-continued` ne déclenche pas de `100 Continue`) ; `Connection: close` est **absorbant** en HTTP/1.0 ;
- **[P3] `Content-Type` vérifié** : un `POST` en `text/plain` était traité comme du JSON → `415 Unsupported Media Type` (absent : accepté) ;
- **[P3] `.annotate()` sans effet selon l'ordre** : les annotations sont désormais stockées à part et fusionnées à l'enregistrement de l'outil — `.annotate()` **avant** ou **après** `.tool()` donne le même résultat (avant : perte silencieuse) ;
- **[P3] `tool_parse` ne clone plus le `Value`** : la désérialisation passait par `value.clone()` (copie de l'arbre JSON des arguments, jusqu'à 1 Mio par appel) — désormais sérialisation unique puis désérialisation ;
- **[P3] `u64::MAX` accepté comme `id`** : JSON-RPC autorise tout `Number` ; les ids au-delà de `i64::MAX` sont désormais acceptés (`RequestId::Unsigned`) ;
- **[P3] `core()` documenté comme un instantané** ; `serve()` reste non interruptible (les tests fuient ~51 threads — connu, non corrigé) ;
- **CI** : job `cargo-deny` (advisories RustSec sur `Cargo.lock`) + `deny.toml` minimal ;
- **README** : section « Sécurité » (modèle de menace opérationnel — TLS/auth/rate-limit par IP au proxy, plafond global ≠ par IP, TTL de connexion, timeout des handlers bloquants à la charge du handler, format des origines autorisées : origine normalisée navigateur, sans port par défaut) + tableau « Protocole » aligné sur la spec `2026-07-28`.

### Tests

- **109 tests au total** (27 unitaires JSON-RPC/zod/protocol, 73 HTTP sur vraies sockets, 5 smoke, 4 doctests) — 29 nouveaux, un par correctif ci-dessus.

## [0.3.0] - 2026-10-22

### Added

- **Durcissement HTTP (Phase 3)** :
  - timeout de lecture par opération anti-slowloris (`READ_TIMEOUT` 30 s, configurable `.read_timeout(durée)`) — un client trop lent est déconnecté sans réponse ;
  - timeout d'écriture (`WRITE_TIMEOUT` 10 s) ;
  - limites `MAX_HEADERS` (64) et `MAX_LINE` (16 Kio) → `431 Request Header Fields Too Large`, avec lecture de lignes bornée en mémoire (`read_line_capped`) — un client hostile ne peut plus faire grossir le buffer ;
  - support `Expect: 100-continue` (intermédiaire `100` avant lecture du corps) ;
  - keep-alive multi-requêtes testé (pipelining 2 requêtes sur une connexion).
- **Backend HTTP alternatif testé puis retiré** (bench A/B du 2026-10-22, `examples/bench_backends`) : tiny_http (vérifié 100 % Rust) s'est avéré ~1,85× plus lent en keep-alive (0,19 vs 0,10 ms/req), +360 Ko de binaire et +4 deps — il a été **supprimé du dépôt** : la route maison est le seul backend, le crate garde exactement **2 dépendances** (`serde`, `serde_json`).
- **Première campagne de benchmarks** : binaire hello 462 Ko (cible < 1,5 Mo ✅), keep-alive 0,10 ms (cible < 1 ms ✅), RSS 4,7 Mo (WorkingSet Windows, mesure `VmRSS` Linux à venir), démarrage ~130 ms via shell Windows (mesure in-process à venir).
- 67 tests au total (19 unitaires JSON-RPC/zod/protocol, 39 HTTP sur vraies sockets, 5 smoke, 4 doctests).

### Audit complet du 2026-10-22 — durcissement & conformité

**Sécurité / robustesse :**

- **Plafond d'issues de validation** — nouveau collecteur borné `zod::Issues` (`MAX_ISSUES` = 64) : au-delà du plafond, les problèmes sont comptés sans être stockés. Avant : un corps de 1 Mio d'éléments invalides produisait ~15 Mo d'issues (~15× le corps, ~4 Go potentiels sur 256 connexions). L'amplification mémoire est éliminée.
- **Lignes obs-fold rejetées `400`** (RFC 9112 §5.2) : une ligne d'en-tête commençant par une espace/tabulation serait susceptible d'être réassemblée différemment par un proxy — refus net plutôt qu'acceptation ambiguë.
- **`Content-Length` malformé → `400`** (RFC 9112 §6.3, non numérique ou hors usize) ; le `411` est désormais réservé à l'absence du champ.
- **En-tête `Host`** : requis et unique en HTTP/1.1 → `400` sinon (RFC 9112 §3.2) ; HTTP/1.0 dispensé.
- **Version HTTP validée** : ligne de requête malformée (version absente, jeton superflu) → `400` ; version ≠ HTTP/1.0/1.1 → `505`.
- **Réponse `503` non bloquante** : les timeouts socket sont posés avant la branche « trop de connexions » — l'écriture ne peut plus bloquer l'accept loop sur un client qui ne lit pas.
- **Compteur de connexions par serveur** : le plafond vit dans `BoundServer` (plus de static global partagé entre toutes les instances du processus).

**Conformité MCP / JSON-RPC :**

- **`structuredContent` toujours objet** (spec MCP) : une valeur de handler scalaire/tableau/`null` est enveloppée dans `{ "result": … }`, et le contenu texte reflète la valeur structurée réellement envoyée — `structuredContent: 42` ou `: null` violait la spec.
- **`integer()` ne sature plus** : les valeurs f64 aux bornes ±2^63 sont rejetées au lieu de saturer en `i64::MIN`/`i64::MAX` au cast.
- Refus des `id` JSON-RPC fractionnaires (1.5) documenté comme choix assumé (la spec les déconseille, MCP ne les emploie jamais).

**Changements incompatibles (0.x) :**

- `zod::Schema::validate` accumule dans `zod::Issues` (borné) au lieu d'un `Vec<String>` — les implémentations du trait doivent s'adapter (une ligne).
- Réenregistrer un outil du même nom remplace l'ancien — documenté (comportement inchangé).

**Docs :**

- Doc-comment de `respond_error` aligné sur le code (fermeture systématique, jamais de drain) + compromis RST documenté.
- Note `panic = "abort"` : neutralise `catch_unwind`, une panique de handler arrête le processus.
- README : compte de tests corrigé, lignes de protocole `505`/`Host`/obs-fold/CL malformé, `.max_connections` dans la configuration, enveloppe `structuredContent` documentée.

## [0.2.0] - 2026-10-22

### Added

- **Module `zod` — schémas à la Zod** (`src/zod.rs`, 11 tests unitaires) :
  - `object()` avec `.field(nom, schéma)` (requis), `.opt(nom, schéma)` (optionnel), `.strict()` (refuse les champs inconnus) ;
  - primitives : `string().min/.max` (longueur), `integer().min/.max` (i64), `number().min/.max` (f64), `boolean()`, `enumeration(&[..])` (ensemble fermé), `array(s).min/.max` (éléments validés récursivement) ;
  - chaque schéma produit son fragment **JSON Schema draft 2020-12** (`type`, `properties`, `required`, `additionalProperties`, `minLength`/`maxLength`, `minimum`/`maximum`, `minItems`/`maxItems`, `enum`) **et** valide à l'exécution en accumulant tous les problèmes (pas d'arrêt au premier) ;
  - trait `Schema` public : extension possible par l'utilisateur ;
  - zéro dépendance de plus — toujours `serde` + `serde_json` uniquement.
- **`ServerBuilder::tool_typed(nom, description, schéma, closure)`** :
  - l'`inputSchema` publié dans `tools/list` est **dérivé** du schéma (fini le générique `{"type":"object"}`) ;
  - chaque `tools/call` est **validé avant exécution** : un appel hors schéma reçoit `-32602` avec `error.data.issues` — la liste lisible des problèmes (champ manquant, type attendu, borne franchie, champ inconnu) — et le handler n'est **jamais** appelé ;
  - la closure reçoit `&Value` garanti conforme au schéma.
- **`ServerBuilder::tool_parse(nom, description, schéma, closure)`** — comme `tool_typed`, avec les arguments désérialisés dans un struct `Deserialize` : le handler reçoit des données **typées** (`|args: SearchArgs|`).
- **Annotations MCP 2026-07-28** — `protocol::ToolAnnotations` avec builder fluide `.title()`, `.read_only()`, `.destructive()`, `.idempotent()`, `.open_world()` ; attachées via `.annotate(nom_outil, ...)` et sérialisées dans `tools/list` (`readOnlyHint`, `destructiveHint`, `idempotentHint`, `openWorldHint`) ; champs non posés omis du JSON (défauts de la spec côté client).
- **`jsonrpc::ErrorObject::with_data(code, message, data)`** — erreurs JSON-RPC avec `data` structuré (utilisé pour les issues de validation, disponible pour les handlers).
- **Exemple `hello` enrichi** — montre les trois registres d'outils (closure brute / schéma validé / struct typé) + annotations sur deux outils.
- **Tests** : 32 au total (11 unitaires `zod`, 3 tests E2E typés sur vraies sockets — dérivation de schéma + annotations dans `tools/list`, exécution valide, rejet `-32602` avec issues — en plus des 18 existants).

### Documentation

- README : section « Outils — schémas à la Zod » (exemple `tool_parse` complet, tableau des trois registres, catalogue des schémas), section « Annotations », FAQ « Zod en Rust », badges crates.io/CI/docsrs actifs, badge statut 0.2.0.
- ROADMAP : Phase 4 close (« Ergonomie Zod »), macros décalées en 0.3, décision technique #6 mise à jour, journal v5.
- Cargo.toml : `keywords` `model-context-protocol` → `mcp-server` (limite crates.io : 20 caractères par mot-clé).

## [0.1.0] - 2026-10-21

### Added

- **Noyau JSON-RPC 2.0** (`src/jsonrpc.rs`) :
  - `RequestId` (nombre ou chaîne, untagged), `ErrorObject` avec les cinq codes standards (-32700, -32600, -32601, -32602, -32603) ;
  - `Incoming` / `Outgoing` : parse error avec `id: null` comme exigé par la spec, `success` n'embarque jamais de champ `error` (et inversement) ;
  - notifications (sans `id`) détectées et sans réponse.
- **Dispatch MCP stateless** (`src/server.rs`) :
  - méthodes `initialize` (renvoie `protocolVersion` + `serverInfo` — toléré pour les inspecteurs, jamais requis), `ping` → `{}`, `tools/list`, `tools/call` ;
  - `ServerBuilder::new(name, version).tool(nom, description, closure).bind(addr)?.serve()` + `prelude` ;
  - `ServerCore` réutilisable sans transport (tests, intégrations custom) ;
  - outil en erreur → `isError: true` + message en contenu texte ; outil inconnu / params invalides → `-32602` ; méthode inconnue → `-32601`.
- **Transport Streamable HTTP stateless** (`src/http.rs`) :
  - socket bindé `std::net::TcpListener`, un thread `std` par connexion, zéro runtime async ;
  - `POST /mcp` par message : réponse `application/json`, ou SSE request-scoped mono-événement (`Accept: text/event-stream` seul) ;
  - notification → `202 Accepted` sans corps ; `GET` → `405` (pas de listener SSE permanent en stateless) ;
  - HTTP/1.1 minimal maison : ligne de requête, en-têtes, `Content-Length`, keep-alive, `Connection: close` ;
  - erreurs propres : `404` route inconnue, `411` sans `Content-Length`, `413` au-delà de `MAX_BODY` (1 Mio) ;
  - garde DNS-rebinding : toute requête avec `Origin` non listée est refusée `403` (`allow_origins()` pour la liste blanche) ; les clients CLI sans `Origin` passent.
- **Types MCP 2026-07-28** (`src/protocol.rs`) : `PROTOCOL_VERSION`, `Tool` / `inputSchema`, `ToolsListResult`, `CallToolResult` (`content` + `isError` + `structuredContent`), `ServerInfo`.
- **Tests** : 18 (4 unitaires JSON-RPC + 13 d'intégration HTTP sur vraies sockets bindées `127.0.0.1:0` + 1 smoke), clippy `-D warnings` zéro, `cargo audit` zéro vulnérabilité.

### Security

- `#![forbid(unsafe_code)]` — zéro `unsafe` dans le crate.
- Deux dépendances seulement : `serde` + `serde_json`.
- Garde DNS-rebinding sur `Origin` actif par défaut.

### Publication

- crates.io : [litemcp 0.1.0](https://crates.io/crates/litemcp) — package 17 fichiers, `.cargo_vcs_info.json` vérifié (sha1 = commit tagué).
- GitHub : dépôt [DeamonDev888/litemcp](https://github.com/DeamonDev888/litemcp), tag `v0.1.0`, release, CI verte (fmt / clippy / test / doc).
- Token API crates.io stocké dans `~/.cargo/credentials.toml` (hors repo), garde `.cargo/` au `.gitignore`.

[Unreleased]: https://github.com/DeamonDev888/litemcp/compare/v0.4.1...HEAD
[0.4.1]: https://github.com/DeamonDev888/litemcp/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/DeamonDev888/litemcp/compare/v0.3.3...v0.4.0
[0.3.3]: https://github.com/DeamonDev888/litemcp/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/DeamonDev888/litemcp/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/DeamonDev888/litemcp/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/DeamonDev888/litemcp/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/DeamonDev888/litemcp/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/DeamonDev888/litemcp/releases/tag/v0.1.0
