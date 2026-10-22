# Changelog

Toutes les évolutions notables de ce projet sont documentées dans ce fichier.
Format : [Keep a Changelog](https://keepachangelog.com/) — versioning : SemVer.

## [0.2.0] - 2026-10-22

### Added

- **Module `zod`** — schémas à la Zod : `object().field(..)/.opt(..)/.strict()`, `string().min/.max`, `integer().min/.max`, `number().min/.max`, `boolean()`, `enumeration(&[..])`, `array(s).min/.max`. Chaque schéma produit son fragment JSON Schema (draft 2020-12) **et** valide les arguments à l'exécution — zéro dépendance de plus.
- **`ServerBuilder::tool_typed`** — `inputSchema` dérivé du schéma dans `tools/list` ; chaque `tools/call` est validé avant exécution, un appel hors schéma reçoit `-32602` avec la liste des problèmes dans `error.data.issues` (équivalent `ZodError`) et n'exécute jamais le handler.
- **`ServerBuilder::tool_parse`** — comme `tool_typed` avec les arguments désérialisés dans un struct `Deserialize` : le handler reçoit des données typées.
- **Annotations MCP 2026-07-28** — `ToolAnnotations` (`title`, `readOnlyHint`, `destructiveHint`, `idempotentHint`, `openWorldHint`) publiées dans `tools/list` via `.annotate()`.
- **`ErrorObject::with_data`** — erreurs JSON-RPC avec `data` structuré.
- Exemple `hello` enrichi : les trois registres d'outils (brut / schéma / typé) + annotations.
- 32 tests (11 unitaires `zod`, 3 tests E2E typés sur vraies sockets en plus des 18 existants).

## [0.1.0] - 2026-10-21

### Added

- Noyau JSON-RPC 2.0 : `RequestId`, `ErrorObject` (codes -32700/-32600/-32601/-32602/-32603), parse error avec `id: null`.
- Dispatch MCP stateless : `initialize`, `ping`, `tools/list`, `tools/call` ; notifications sans réponse.
- Transport **Streamable HTTP uniquement** : socket bindé (`std::net::TcpListener`, un thread par connexion), `POST /mcp` par message, réponse `application/json` ou SSE request-scoped mono-événement (`Accept: text/event-stream` seul), notification → `202`, `GET` → `405`.
- Garde DNS-rebinding sur `Origin` (refus par défaut, `allow_origins()`), `411` sans `Content-Length`, `413` au-delà de 1 Mio.
- API : `ServerBuilder::new(name, version).tool(nom, description, closure).bind(addr)?.serve()` + `prelude`.
- 18 tests (unitaires + intégration sur vraies sockets, bind `127.0.0.1:0`).

### Security

- `#![forbid(unsafe_code)]` — zéro `unsafe` dans le crate.
- Deux dépendances seulement : `serde` + `serde_json`.
