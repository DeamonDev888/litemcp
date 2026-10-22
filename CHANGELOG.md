# Changelog

Toutes les évolutions notables de ce projet sont documentées dans ce fichier.
Format : [Keep a Changelog](https://keepachangelog.com/) — versioning : SemVer.

## Politique de versioning

- **Flux permanent** : toute modification (fonction, correctif, doc) atterrit d'abord dans la section **[Unreleased]** ci-dessous, au fil des commits.
- **Pacement** : une version n'est coupée que pour un **lot cohérent** — une fonctionnalité complète, un correctif notable, ou une étape de documentation majeure. Pas de bump rituel à chaque commit.
- **0.x** jusqu'à la conformance officielle MCP verte (suite 2026-07-28, mode serveur, 0 FAILURE) ; la mention **1.0** attend ce jalon.
- Chaque version publiée est taguée `vX.Y.Z` sur le commit exact dont elle est issue (`.cargo_vcs_info.json` vérifié avant `cargo publish`).

## [Unreleased]

### Ajouts en cours (non publiés)

- **Durcissement HTTP (Phase 3)** :
  - timeout de lecture par opération anti-slowloris (`READ_TIMEOUT` 30 s, configurable `.read_timeout(durée)`) — un client trop lent est déconnecté sans réponse ;
  - timeout d'écriture (`WRITE_TIMEOUT` 10 s) ;
  - limites `MAX_HEADERS` (64) et `MAX_LINE` (16 Kio) → `431 Request Header Fields Too Large`, avec lecture de lignes bornée en mémoire (`read_line_capped`) — un client hostile ne peut plus faire grossir le buffer ;
  - support `Expect: 100-continue` (intermédiaire `100` avant lecture du corps) ;
  - keep-alive multi-requêtes testé (pipelining 2 requêtes sur une connexion).
- **Backend HTTP alternatif testé puis retiré** (bench A/B du 2026-10-22, `examples/bench_backends`) : tiny_http (vérifié 100 % Rust) s'est avéré ~1,85× plus lent en keep-alive (0,19 vs 0,10 ms/req), +360 Ko de binaire et +4 deps — il a été **supprimé du dépôt** : la route maison est le seul backend, le crate garde exactement **2 dépendances** (`serde`, `serde_json`).
- **Première campagne de benchmarks** : binaire hello 462 Ko (cible < 1,5 Mo ✅), keep-alive 0,10 ms (cible < 1 ms ✅), RSS 4,7 Mo (WorkingSet Windows, mesure `VmRSS` Linux à venir), démarrage ~130 ms via shell Windows (mesure in-process à venir).
- 37 tests au total (11 `zod`, 21 HTTP dont 5 durcissement, 4 JSON-RPC, 1 smoke).

### Prévu (cible 0.3)

- Macros `#[tool]` / `#[resource]` / `#[prompt]` dérivant le schéma du type Rust (workspace split `litemcp-macros`) — finira de supprimer la double déclaration schéma/struct.
- Fuzz du parseur HTTP (proptest/corpus) et tests clients réels (MCP Inspector, Claude, Cursor).
- Ressources et prompts dans le noyau (aujourd'hui : `tools` seulement).
- Mesures SLO Linux (`VmRSS`, démarrage in-process) et publication des conditions figées.

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

[Unreleased]: https://github.com/DeamonDev888/litemcp/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/DeamonDev888/litemcp/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/DeamonDev888/litemcp/releases/tag/v0.1.0
