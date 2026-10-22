# litemcp — Roadmap

> **Pivot du 2026-10-21 (décision opérateur)** : **HTTP-only. Streamable HTTP
> est l'unique transport.**
> litemcp = Streamable HTTP **stateless** (spec MCP 2026-07-28) sur un socket
> bindé — zéro runtime async, zéro session, zéro handshake.
> Nom `litemcp` vérifié libre sur crates.io le 2026-10-21 (404 API).

---

## 1. Positionnement

| | rmcp (officiel) | fastmcp_rust | **litemcp** |
|---|---|---|---|
| Runtime | tokio + hyper + tower | asupersync (maison, nightly) | **aucun** (threads bloquants `std`) |
| Toolchain | stable | nightly épinglé | **stable, MSRV 1.85** |
| Deps core | tokio, hyper, schemars… | nombreuses | **serde + serde_json** |
| Transports | multi (HTTP, WS, sous-processus) | multi (HTTP, SSE, WS, sous-processus) | **Streamable HTTP uniquement (POST/SSE)** |
| Session | oui (ou stateless) | oui | **aucune — chaque POST est auto-suffisant** |
| `unsafe` | autorisé | interdit | **interdit** (`forbid`) |
| Mémoire | non communiquée | non communiquée | **SLO mesurés à chaque release** |

Cible : serveurs MCP embarqués (CLI, app, edge, IoT), tests unitaires rapides, processus courts — là où embarquer tokio coûte plus cher que le serveur lui-même.

## 2. Décisions techniques arrêtées

1. **Spec cible : MCP 2026-07-28 stateless uniquement** — un `POST /mcp` par message, réponse `application/json` (ou `text/event-stream` request-scoped mono-événement si le client ne demande que du SSE), notification → `202` sans corps, `GET` → `405`. Pas de `initialize` obligatoire (toléré pour les inspecteurs), pas de `Mcp-Session-Id`.
2. **Un seul transport** (décision opérateur) : le serveur est un démon Streamable HTTP joignable par plusieurs clients, pas un sous-processus d'un seul. Tout autre mode d'exécution est hors périmètre du produit.
3. **HTTP/1.1 minimal maison** sur `std::net::TcpListener` : route unique, `Content-Length`, keep-alive, garde DNS-rebinding sur `Origin` (refus par défaut, liste blanche explicite), `MAX_BODY` = 1 Mio, erreurs HTTP propres (403/404/405/411/413).
4. **Zéro runtime async** : threads `std`, un thread par connexion. Adaptateur tokio éventuel plus tard, en feature, jamais par défaut.
5. **`#![forbid(unsafe_code)]`** ; deps core `serde` + `serde_json` uniquement ; pas de schemars.
6. **API actuelle (0.1)** : `ServerBuilder` + closures d'outils. **0.2** : macros `#[tool]`/`#[resource]`/`#[prompt]` + schémas dérivés (workspace split `litemcp-macros`).
7. **Licence** dual `MIT OR Apache-2.0` ; **MSRV 1.85** (edition 2024).
8. **Versioning** 0.x jusqu'à conformance verte ; publication précoce **0.1.0** assumée comme socle pour réserver le nom crates.io ; 1.0 après conformance.

## 3. Phases

### Phase 0 — Fondations ✅ (2026-10-21)

- [x] Structure repo, `Cargo.toml` prêt crates.io, licences, `.gitignore`, CI (fmt/clippy/test/doc)
- [x] `git init` main + commits initial

**Jalon** : `cargo test` vert localement et en CI.

### Phase 1 — Noyau JSON-RPC + dispatch MCP ✅ (2026-10-21)

- [x] `jsonrpc` : `RequestId`, `ErrorObject` (codes -32700/-32600/-32601/-32602/-32603), `Incoming`, `Outgoing` (`id: null` sur parse error comme exigé)
- [x] `protocol` : `PROTOCOL_VERSION` 2026-07-28, `Tool`/`inputSchema`, `ToolsListResult`, `CallToolResult` (`isError` + `structuredContent`), `ServerInfo`
- [x] `server` : `ServerBuilder` + `tool()` + dispatch `initialize`/`ping`/`tools/list`/`tools/call`, notifications sans réponse
- [x] Tests unitaires (round-trip, parse error, succès sans champ `error`)

**Jalon** : dispatch complet, aucune I/O dans le noyau.

### Phase 2 — Transport HTTP stateless ✅ (2026-10-21)

- [x] `http::BoundServer` : `local_addr()`, `serve()` (accept loop, thread par connexion)
- [x] Parseur HTTP/1.1 minimal : ligne de requête, en-têtes, `Content-Length`, keep-alive, `Connection: close`
- [x] Réponses : `200` JSON, `200` SSE mono-événement (`Accept: text/event-stream` seul), `202` notifications, `403` Origin refusée, `404` route, `405` GET, `411`, `413`
- [x] Garde DNS-rebinding (`Origin` refusée par défaut, `allow_origins()`)
- [x] 14 tests d'intégration sur **vraies sockets** (bind `127.0.0.1:0`)
- [x] Exemple `hello`

**Jalon** : `cargo test` + `cargo run --example hello` fonctionnels.

### Phase 3 — Durcissement HTTP

- [ ] Fuzz du parseur (proptest/corpus), timeouts de lecture (slowloris), limite du nombre d'en-têtes
- [ ] Tests réels : MCP Inspector (mode URL), Claude, Cursor
- [ ] Keep-alive multi-requêtes testé; gestion `Expect: 100-continue`
- [ ] Décision : garder HTTP maison ou basculer sur `tiny_http` selon les résultats du fuzz

**Jalon** : aucune panique ni blocage possible depuis un client hostile (fuzz vert).

### Phase 4 — Ergonomie (0.2)

- [ ] Workspace split : `crates/litemcp` + `crates/litemcp-macros`
- [ ] `#[tool]`, `#[resource]`, `#[prompt]` — schéma dérivé des types (sans schemars)
- [ ] Ressources/prompts dans le noyau (aujourd'hui : `tools` seulement)
- [ ] Exemples enrichis

**Jalon** : hello-tool en < 15 lignes ; binaire encore plus petit (mesuré).

### Phase 5 — Publication crates.io 🚧

- [x] `cargo package --list` propre (src + Cargo.toml + README + licences)
- [ ] `cargo publish` **0.1.0** — *token API reçu le 2026-10-21, stocké dans `~/.cargo/credentials.toml` (hors repo) ; garde-fou `.cargo/` ajouté au `.gitignore`*
- [ ] docs.rs propre, badge crates.io, CHANGELOG

**Jalon** : `crates.io/crates/litemcp` en ligne.

### Phase 6 — Conformance + benchmarks → 1.0

- [ ] Suite de conformance officielle MCP 2026-07-28, mode serveur (cible : 0 FAILURE)
- [ ] Benchmarks publics (RSS, binaire, p99) vs rmcp+axum, dans le README
- [ ] Client HTTP minimal (optionnel) ; feature `legacy` 2025-11-25 étudiée

**Jalon** : conformance verte + tableau de bench → tag `1.0.0`.

## 4. SLO (hypothèses, première mesure en Phase 3)

| Métrique | Cible | Mesure |
|---|---|---|
| RSS hello-tool (HTTP, au repos) | < 2 Mo | VmRSS |
| Binaire release stripped | < 1,5 Mo | `ls -lh` |
| `tools/call` p99 local | < 1 ms | harness, 10k appels |
| Démarrage à froid | < 10 ms | time-to-first-response |

## 5. Risques

| Risque | Impact | Mitigation |
|---|---|---|
| HTTP/1.1 maison (parsing, edge cases) | Élevé | Périmètre minimal (POST only), fuzz en Phase 3, repli `tiny_http` décidé par les tests |
| Conformance incomplète en 0.1 | Moyen | 0.1 assumée « socle » ; 1.0 seulement après suite officielle |
| Superlatif « le plus léger du monde » non mesuré | Réputation | Benchmarks obligatoires à chaque release ; la phrase reste un objectif, les chiffres tranchent |

## 6. Journal

- **2026-10-21 v1** — Phase 0 : scaffold (Cargo prêt crates.io, licences, CI, README, roadmap), `git init` + commit `16ad4e4`. Nom `litemcp` vérifié libre.
- **2026-10-21 v2** — README style framework (ViteMCP/FastMCP), commit `3bde75b`.
- **2026-10-21 v3** — **PIVOT HTTP-only** (décision opérateur) : Phases 1 et 2 implémentées d'un coup (noyau JSON-RPC + HTTP stateless sur socket bindé, 14 tests d'intégration) ; `transport.rs` supprimé ; `0.1.0` prêt à publier ; token crates.io reçu et stocké hors repo.
