# Changelog

Toutes les évolutions notables de ce projet sont documentées dans ce fichier.
Format : [Keep a Changelog](https://keepachangelog.com/) — versioning : SemVer.

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
