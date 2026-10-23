# litemcp — Roadmap

> **Pivot du 2026-10-21 (décision opérateur)** : **HTTP-only. Streamable HTTP
> est l'unique transport.**
> Stateless · socket bindé · **Streamable HTTP uniquement** (POST/SSE) · zéro
> runtime async — spec MCP 2026-07-28, zéro session, zéro handshake.
> **Publié le 2026-10-21** : [crates.io/crates/litemcp](https://crates.io/crates/litemcp)
> · [github.com/DeamonDev888/litemcp](https://github.com/DeamonDev888/litemcp) (tag `v0.1.0`).

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
3. **HTTP/1.1 minimal maison** sur `std::net::TcpListener` : route unique, `Content-Length`, keep-alive, garde DNS-rebinding sur `Origin` (refus par défaut, liste blanche explicite), `MAX_BODY` = 1 Mio, erreurs HTTP propres (403/404/405/411/413/431). **Décision du 2026-10-22 (bench A/B)** : la route maison est le **seul** backend — ~1,85× plus rapide en keep-alive (0,10 vs 0,19 ms/req), +360 Ko et +4 deps évités ; le repli `tiny_http` (testé 100 % Rust) a été **retiré du dépôt** après la mesure : minimalisme strict, 2 deps.
4. **Zéro runtime async** : threads `std`, un thread par connexion. Adaptateur tokio éventuel plus tard, en feature, jamais par défaut.
5. **`#![forbid(unsafe_code)]`** ; deps core `serde` + `serde_json` uniquement ; pas de schemars.
6. **API actuelle (0.3)** : `ServerBuilder` + 3 registres d'outils — `tool` (closure brute), `tool_typed` (schéma Zod dérivé + validé), `tool_parse` (struct `Deserialize`) — et `.annotate()` / `ToolAnnotations` ; validation bornée (`zod::Issues`), `structuredContent` toujours objet. **0.4** : macros `#[tool]`/`#[resource]`/`#[prompt]` + schéma dérivé du type Rust (workspace split `litemcp-macros`).
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

### Phase 3 — Durcissement HTTP ✅ (2026-10-22, fuzz corpus → 0.4)

- [x] Timeout de lecture par opération (anti-slowloris) : `READ_TIMEOUT` 30 s, configurable `.read_timeout()` ; client trop lent → déconnecté sans réponse (testé)
- [x] Timeout d'écriture (`WRITE_TIMEOUT` 10 s)
- [x] Limite du nombre d'en-têtes (`MAX_HEADERS` 64 → `431`, testé) et de longueur de ligne (`MAX_LINE` 16 Kio → `431`, testé) — lecture bornée mémoire (`read_line_capped`)
- [x] `Expect: 100-continue` : intermédiaire `100` envoyé avant la lecture du corps (testé)
- [x] Keep-alive multi-requêtes sur une connexion (pipelining 2 requêtes, testé)
- [x] Décision repli tranchée par le bench A/B (`examples/bench_backends`) : maison ~1,85× plus rapide keep-alive, tiny +360 Ko / +4 deps → **tiny retiré du dépôt** (minimalisme strict : 2 deps), la route maison est le seul backend ; l'implémentation tiny avait été vérifiée 100 % Rust (`default-features = false`) et testée (6 tests) avant le retrait
- [x] Première campagne de benchmarks : binaire 462 Ko (< 1,5 Mo ✅), keep-alive 0,10 ms (< 1 ms ✅), RSS 4,7 Mo (WorkingSet Windows — `VmRSS` Linux à venir), démarrage ~130 ms via shell (in-process à venir)
- [x] **Audit complet (revue indépendante, 2026-10-22)** : en-tête `Host` requis et unique en HTTP/1.1 (`400`), `Content-Length` malformé → `400` (le `411` réservé à l'absence), version HTTP ≠ 1.0/1.1 → `505`, ligne de requête stricte (exactement 3 jetons), obs-fold rejeté, `structuredContent` toujours objet (spec MCP), compteur de connexions par instance, timeouts posés avant la réponse `503`, saturation i64 des bornes f64 corrigée
- [ ] Fuzz du parseur (proptest/corpus), MCP Inspector / Claude / Cursor réels → **0.4**

**Jalon atteint** : aucun blocage ni panique sur les vecteurs hostiles testés (slowloris, lignes/en-têtes géants, corps surdimensionné) ; la décision HTTP maison vs repli est chiffrée, plus un débat.

### Phase 4 — Ergonomie « Zod » ✅ (2026-10-22, macros → 0.4)

- [x] Module `zod` : schémas à la Zod (`object().field/.opt/.strict`, `string`, `integer`, `number`, `boolean`, `enumeration`, `array` + bornes) — dérivation JSON Schema (draft 2020-12) **et** validation runtime, zéro dépendance de plus
- [x] `ServerBuilder::tool_typed` — schéma dérivé dans `tools/list`, arguments validés avant exécution (`-32602` + `error.data.issues`, l'équivalent `ZodError`)
- [x] `ServerBuilder::tool_parse` — arguments désérialisés dans un struct `Deserialize` après validation
- [x] Annotations MCP 2026-07-28 (`title`, `readOnlyHint`, `destructiveHint`, `idempotentHint`, `openWorldHint`) via `.annotate()` / `ToolAnnotations`
- [x] `ErrorObject::with_data` — erreurs JSON-RPC avec `data` structuré
- [x] 11 tests unitaires zod + 3 tests E2E typés sur vraies sockets (32 au total) ; exemple `hello` triple registre (brut / schéma / typé)
- [ ] Macros `#[tool]`/`#[resource]`/`#[prompt]` dérivant le schéma du type Rust → **0.4** (workspace split `litemcp-macros`)
- [ ] Ressources/prompts dans le noyau (aujourd'hui : `tools` seulement)

**Jalon atteint** : l'API outil égale fastmcp TS (schéma → `inputSchema` dérivé → validation → handler typé), en plus léger : toujours 2 deps, zéro async.

### Phase 5 — Publication crates.io ✅ (2026-10-21)

- [x] `cargo package --list` propre (src + Cargo.toml + README + licences)
- [x] `cargo publish` **0.1.0** — *token API stocké dans `~/.cargo/credentials.toml` (hors repo), garde-fou `.cargo/` au `.gitignore` ; keyword trop long corrigé (limite 20 caractères → `mcp-server`)*
- [x] docs.rs, badges crates.io + CI + docsrs dans le README, CHANGELOG
- [x] Repo GitHub créé + push, tag `v0.1.0` sur le SHA publié, release GitHub, CI verte

**Jalon atteint** : [crates.io/crates/litemcp](https://crates.io/crates/litemcp) en ligne (`max_version: 0.1.0`).

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
- **2026-10-21 v4** — **Publication 0.1.0** : fmt / clippy -D warnings / 18 tests / audit (0 vulnérabilité) verts ; package 17 fichiers, `.cargo_vcs_info.json` intègre ; `cargo publish` OK (`max_version: 0.1.0` confirmé par l'API) ; repo GitHub créé, tag `v0.1.0`, release, CI verte.
- **2026-10-22 v5** — **Phase 4 « Zod »** : module `zod` (dérivation JSON Schema + validation runtime, 0 dep de plus), `tool_typed`/`tool_parse` (handlers typés façon fastmcp TS), annotations MCP, `ErrorObject::with_data` ; 32 tests verts ; version 0.2.0. Macros `#[tool]` décalées en 0.3.
- **2026-10-22 v6** — **Phase 3 « Durcissement »** : timeouts lecture/écriture (anti-slowloris, `.read_timeout()`), limites en-têtes/lignes (`431`, lecture bornée `read_line_capped`), `Expect: 100-continue`, keep-alive multi-requêtes (pipelining) — 5 nouveaux tests E2E. Bench A/B **maison vs tiny_http** (`examples/bench_backends`) : maison ~1,85× plus rapide keep-alive (0,10 vs 0,19 ms/req), +360 Ko et +4 deps côté tiny → **route maison confirmée seul backend, tiny retiré du dépôt** (décision opérateur : minimalisme strict). Première campagne SLO : binaire 462 Ko ✅, keep-alive 0,10 ms ✅, RSS 4,7 Mo (WorkingSet), démarrage ~130 ms (shell). 37 tests verts.
- **2026-10-22 v7** — **Audit complet + publication 0.3.0** : revue indépendante, tout corrigé — plafond d'issues `zod::Issues` (`MAX_ISSUES` 64, amplification mémoire ~15× éliminée), `structuredContent` toujours objet (enveloppe `{"result": …}`), `Host` requis/unique HTTP/1.1, `Content-Length` malformé → `400`, version → `505`, obs-fold → `400`, ligne de requête stricte, compteur de connexions par instance, timeouts avant `503`, saturation i64 corrigée. Changement incompatissant assumé : `Schema::validate` prend `&mut zod::Issues`. Macros/fuzz décalés en 0.4. **67 tests verts**, clippy `-D warnings`, fmt, doc. Version 0.3.0 (bump mineur : breaking 0.x).
- **2026-10-22 v8** — **Lot « léger & rapide »** : réponse HTTP en **un seul write** (avant : 2 syscalls/2 paquets), parsing d'en-têtes **zéro allocation**, zéro clone des arguments au dispatch (`params`/`id` déplacés, arguments par référence), buffers de lecture réutilisés keep-alive, `Arc` route/origins, buffer lecture 4 Ko (moitié moins par connexion). A/B dos à dos : keep-alive **0,097–0,111 → 0,065–0,070 ms (~−35 %)**, binaire **483 → 385 Ko (−18,5 %**, LTO fat + `codegen-units = 1`**)**. Bench enrichi (warmup, p50/p99). 67 tests verts, déps toujours 2.
- **2026-10-22 v9** — **Comparatif ViteMCP + adoption `/health`** : identification du vrai pendant TS (`@vitemcp/server`, même spec stateless 2026-07-28 — « ViteMCP » n'existe pas tel quel sur npm, c'est le nom du projet GitHub), serveur démo 4 outils construit des deux côtés, client de bench **neutre unique** (`bench_remote` : Content-Length, chunked, EOF). Résultat : keep-alive **~15–18× plus rapide** (0,09–0,10 vs 1,43–1,73 ms), connexions neuves ~2–3×, réutilisation 2000/2000 vs 0/2000 (ViteMCP maintient le SSE ouvert → reconnect par appel). Point fort repris : **sonde `GET /health`** (200 « ok », keep-alive-compatible, 405 sinon). Candidat 0.4 noté : `outputSchema`. 70 tests verts (3 sonde) + 13 au banc externe en mode local.
- **2026-10-22 v10** — **Publication 0.3.1** : lot « léger & rapide » (keep-alive ~−35 % in-repo, −13 % côté consommateur, binaire −18,5 %, buffer lecture 4 Ko) + sonde `GET /health` + comparatif ViteMCP chiffré. Vérifications : 70 tests, clippy `-D warnings`, fmt, package 19 fichiers, `.cargo_vcs_info.json` = commit tagué. Le README crates.io (section « Pourquoi » réécrite) est rafraîchi à cette occasion.
- **2026-10-22 v11** — **Anti-tampon SSE** (problème des VPS derrière un proxy tamponneur) : en-têtes `X-Accel-Buffering: no` + `Cache-Control: no-cache, no-transform` sur toute réponse SSE (`SSE_ANTI_BUFFERING`, JSON non affecté) ; **choix architectural documenté** : pas de SSE longue durée — request-scoped, un événement puis clôture (le transport ne peut pas empiler) ; contrat d'écriture SSE écrit dans le code (flush par événement + en-têtes). README : section « Hébergement VPS & proxies » (recette nginx, repli JSON). 72 tests verts. `outputSchema` reste le chantier 0.4 (mesuré : +0,2–1,4 % du trajet réseau).
- **2026-10-22 v12** — **Publication 0.3.2** : anti-tampon SSE + docs (Hébergement VPS, FAQ WebSocket, sonde `/health` dans la configuration). Relecture complète du README de bout en bout avant publication ; cible d'audit `examples/audit_target.rs` exclue du dépôt et du package (`.gitignore`). 72 tests, clippy `-D warnings`, fmt.
