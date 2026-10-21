//! Transports MCP.
//!
//! - `stdio` (Phase 2) : JSON-RPC délimité par newlines sur `Read` + `Write`.
//! - `http` (Phase 3) : Streamable HTTP **stateless** — `POST /mcp` unique,
//!   réponse `application/json` ou `text/event-stream` (SSE request-scoped).
//!
//! Le cœur (dispatch JSON-RPC) ne dépend d'aucun transport : ces modules ne
//! font que brancher des octets sur le noyau.
