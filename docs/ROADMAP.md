# litemcp — Roadmap

> **Objectif** : le framework MCP le plus rapide et le plus léger en mémoire pour Rust.
> Zéro runtime async obligatoire, spec MCP **2026-07-28** (stateless) en cible primaire.
> Nom `litemcp` vérifié libre sur crates.io le 2026-10-21 (404 API).

---

## 1. Positionnement

Le marché Rust est occupé par deux extrêmes : **rmcp** (officiel, excellent mais tokio + hyper + tower + schemars) et **fastmcp_rust** (nightly épinglé, runtime maison asupersync, conformité non vérifiée). Le créneau libre : **stable, minuscule, mesuré**.

| | rmcp (officiel) | fastmcp_rust | **litemcp** |
|---|---|---|---|
| Runtime | tokio + hyper + tower | asupersync (maison) | **aucun au core** (threads bloquants `std`) |
| Toolchain | stable | nightly épinglé | **stable, MSRV 1.85** |
| Deps core | tokio, hyper, schemars… | nombreuses, maison | **serde + serde_json uniquement** |
| `unsafe` | autorisé | interdit | **interdit** (`forbid`) |
| Spec | 2026-07-28 stable | 2026-07-28 « en cours, non vérifié » | 2026-07-28 stateless-first |
| Transports | stdio, Streamable HTTP (hyper) | stdio, HTTP, SSE, WS | stdio + **HTTP/1.1 minimal maison** |
| Mémoire | non communiquée | non communiquée | **SLO mesurés à chaque release** |

Cible d'usage : serveurs MCP embarqués (CLI, TUI, edge, IoT), tests unitaires rapides, processus courts — là où embarquer tokio coûte plus cher que le serveur lui-même.

## 2. Décisions techniques arrêtées (la « config » discutée)

1. **Spec cible** : MCP 2026-07-28 **stateless** — chaque message = un POST sur une route unique, réponse JSON ou flux SSE *request-scoped*. Pas de `initialize`, pas de `Mcp-Session-Id`, métadonnées dans `_meta`. La compat 2025-11-25 (handshake + session) arrive en feature `legacy` en Phase 5.
2. **Transports** :
   - **stdio** : framing newline-delimited JSON-RPC (le framing standard de la spec, réutilisable sur tout flux d'octets) ;
   - **Streamable HTTP** : mini HTTP/1.1 maison sur `std::net::TcpListener` — POST + `Content-Length` + keep-alive + writer SSE (`data: …\n\n`). TLS hors scope : derrière reverse proxy (nginx/caddy), comme la spec le recommande.
3. **Zéro runtime async dans le core** : threads bloquants, un thread par connexion HTTP. Un adaptateur tokio optionnel pourra exister en feature, jamais par défaut.
4. **`#![forbid(unsafe_code)]`** sur tous les crates.
5. **Dépendances core** : `serde`, `serde_json`. Rien d'autre. Pas de `schemars` : génération de JSON Schema maison minimale (Phase 4).
6. **API** : `ServerBuilder` fluent + macros `#[tool]` / `#[resource]` / `#[prompt]` (crate `litemcp-macros`, split workspace en Phase 4).
7. **Licence** dual `MIT OR Apache-2.0` (standard écosystème Rust).
8. **MSRV** : 1.85 (edition 2024), politique documentée ; bump de MSRV = minor mineure.
9. **Mesures** : RSS (RSS peak), taille binaire stripped et latence p50/p99 publiés dans le README **à chaque release**, avec la même machine et le même harness que les comparatifs (rmcp mesuré à plat, jamais estimé).
10. **Versioning** : 0.x jusqu'à conformance MCP officielle verte ; semver strict ensuite.

## 3. Phases

### Phase 0 — Fondations ✅ *(cette session)*

- [x] Arborescence repo + `Cargo.toml` prêt crates.io (métadonnées complètes)
- [x] Licences MIT + Apache-2.0, `.gitignore`, README avec positionnement
- [x] CI GitHub Actions : `fmt --check`, `clippy -D warnings`, `test`, `doc`
- [x] Squelette `src/` compilant + premier test (`jsonrpc::RequestId`, `tests/smoke.rs`)
- [x] `git init` (branche `main`) + commit initial
- [ ] Création du repo GitHub `DeamonDev888/litemcp` + premier push — **décision requise : public ou privé** (public recommandé : CI gratuite illimitée + crédibilité crates.io)
- [ ] CI verte sur GitHub

**Jalon** : `cargo test` + `cargo clippy` verts en local et en CI.

### Phase 1 — Noyau protocole (JSON-RPC + types MCP)

- Types JSON-RPC 2.0 : `Request`, `Response`, `Notification`, `ErrorObject`, `RequestId` (string|int), batch
- Types MCP 2026-07-28 : `tools/list`, `tools/call`, `resources/list|read|templates`, `prompts/list|get`, `ping`, `_meta.io.modelcontextprotocol/*`, capabilities
- Mapping erreurs domaine → codes JSON-RPC ; `CallToolResult` (content blocks, `isError`, `structuredContent`)
- Tests : round-trip serde sur un corpus de messages d'exemple de la spec (golden files)

**Jalon** : 100 % des messages d'exemple de la spec 2026-07-28 encodés/décodés sans perte.

### Phase 2 — Transport stdio

- `Server::serve(reader: impl Read, writer: impl Write)` — générique, testable sans processus
- Boucle : ligne → dispatch → réponse ; EOF propre = arrêt ; erreurs non fatales loguées en JSON-RPC error
- Client stdio minimal (spawn d'un sous-processus) pour les tests e2e
- Interop : litemcp server ↔ litemcp client, puis ↔ MCP Inspector (mode stdio)
- **Première mesure** : RSS hello-tool, taille binaire stripped, latence p50/p99

**Jalon** : serveur utilisable dans Claude Desktop (`mcpServers` → commande stdio) ; tableau de mesures publié dans le README.

### Phase 3 — Streamable HTTP stateless (le socket bindé)

- Mini HTTP/1.1 : `TcpListener::bind`, accept → thread par connexion, parse requête (méthode, path, headers, `Content-Length`), keep-alive
- Route unique `POST /mcp` : JSON-RPC → exécution → `application/json` (ou `text/event-stream` request-scoped si le tool streame)
- Codes propres : 405 sur `GET` (pas de listener channel en stateless), 400 JSON invalide, 415 mauvais `Content-Type`, 413 corps trop gros
- Garde DNS-rebinding : vérification `Origin`/`Host` (comme le fait le SDK PHP/Symfony)
- Tests : `curl`, MCP Inspector en mode URL, fuzz basique du parseur HTTP (corpus + proptest en dev-dep)
- **Mesure comparée** : RSS et latence vs rmcp + axum sur le même scénario

**Jalon** : `cargo run --example http` → `http://127.0.0.1:8080/mcp` inspectable dans MCP Inspector ; benchmark publié.

### Phase 4 — Ergonomie (macros + schémas)

- Split workspace : `crates/litemcp` + `crates/litemcp-macros` (proc-macro)
- `#[tool]`, `#[resource]`, `#[prompt]` : génération du handler + JSON Schema minimal depuis les types (sans schemars — objectif : binaire plus petit, à mesurer)
- `ServerBuilder` fluent, état partagé (`Arc`) ou par session, erreurs typées
- Exemples : `hello`, `multi_tools`, `shared_state`, `http_nginx`

**Jalon** : hello-tool complet en < 30 lignes ; gains de taille binaire documentés vs schemars.

### Phase 5 — Client + compat legacy

- Client stdio + client HTTP stateless (mêmes types, pas de duplication)
- Feature `legacy` : ère 2025-11-25 côté serveur **et** client (`initialize`, `Mcp-Session-Id`, GET SSE listener optionnel)
- Matrice d'interop : litemcp ↔ rmcp ↔ fastmcp_rust (serveur et client), documentée

**Jalon** : litemcp client parle à rmcp server et inversement, dans les deux ères.

### Phase 6 — Publication crates.io + conformance

- `docs.rs` propre (toutes features), `CHANGELOG.md`, guide de contribution
- Suite de conformance officielle MCP (`scripts/run_conformance.sh`, spec 2026-07-28, mode serveur — objectif : 0 FAILURE)
- `cargo publish --dry-run` puis publication réelle `0.1.0`, tag `v0.1.0`
- Benchmarks publics dans le README (RSS, binaire, latence) + section « comparé à rmcp »

**Jalon** : `crates.io/crates/litemcp` en ligne, conformance verte, tableau de bench public.

## 4. SLO à établir (Phase 2) et figer (Phase 6)

| Métrique | Cible initiale | Mesure |
|---|---|---|
| RSS serveur hello-tool (stdio) | < 2 Mo | `/proc/<pid>/status` VmRSS au repos |
| Binaire release stripped | < 1,5 Mo | `ls -lh target/release` |
| Latence locale `tools/call` p99 | < 1 ms (hors IO) | harness criterion, 10k appels |
| Démarrage à froid (stdio) | < 10 ms | time-to-first-response |

Les cibles sont des **hypothèses de départ** : elles seront ajustées une seule fois après la première mesure réelle, puis figées comme SLO de release.

## 5. Risques connus

| Risque | Impact | Mitigation |
|---|---|---|
| HTTP/1.1 maison (parsing, edge cases) | Élevé | Périmètre volontairement minimal (POST only, pas de chunked request, pas de TLS) ; fuzz + Inspector ; repli : crate `tiny_http` si bloquant |
| Génération de schéma sans schemars | Moyen | Golden tests contre les schémas de la spec ; escape hatch : feature `schemars` optionnelle |
| rmcp avance vite (spec mouvante) | Moyen | Veille à chaque release MCP ; le core suit la spec, pas rmcp |
| Sur-promesse « le plus rapide » | Réputation | Benchmarks mesurés et publiés, jamais d'adjectif sans chiffre |

## 6. Prochaines actions immédiates

1. **Décider** : repo GitHub public ou privé.
2. Créer `github.com/DeamonDev888/litemcp` + push initial (le token GitHub habituel de la machine suffit — même pattern `url.insteadOf` que les autres repos).
3. Vérifier la CI verte (ubuntu, Actions gratuites si public).
4. Attaquer la Phase 1 : types JSON-RPC + corpus golden.

## 7. Journal de bord

- **2026-10-21** — Phase 0 : scaffold créé (Cargo prêt crates.io, licences, CI, README, roadmap), `git init` + commit initial. Nom `litemcp` vérifié libre sur crates.io.
