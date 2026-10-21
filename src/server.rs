//! Noyau de dispatch JSON-RPC + builder — indépendant du transport.

use crate::jsonrpc::{ErrorObject, Incoming, Outgoing};
use crate::protocol::{self, CallToolResult, ServerInfo, Tool, ToolsListResult};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Résultat d'un outil : `Ok(valeur)` ou `Err(message lisible par le client)`.
pub type ToolResult = Result<Value, String>;

type ToolFn = dyn Fn(&Value) -> ToolResult + Send + Sync;

#[derive(Clone)]
struct ToolDef {
    description: Option<String>,
    schema: Option<Value>,
    f: Arc<ToolFn>,
}

/// Cœur de dispatch MCP stateless, réutilisable sans le transport HTTP.
#[derive(Clone)]
pub struct ServerCore {
    name: String,
    version: String,
    tools: Arc<BTreeMap<String, ToolDef>>,
}

impl ServerCore {
    /// Traite un corps de requête JSON-RPC.
    ///
    /// Renvoie `None` pour une notification (aucune réponse attendue —
    /// le transport répondra `202 Accepted` sans corps).
    pub fn dispatch(&self, body: &[u8]) -> Option<Vec<u8>> {
        let incoming: Incoming = match serde_json::from_slice(body) {
            Ok(value) => value,
            Err(_) => return Some(Outgoing::parse_error().to_vec()),
        };
        if incoming.is_notification() {
            return None;
        }
        let id = incoming
            .id
            .as_ref()
            .map(|id| serde_json::to_value(id).unwrap_or(Value::Null))
            .unwrap_or(Value::Null);

        let outgoing = match incoming.method.as_str() {
            protocol::method::PING => Outgoing::success(id, json!({})),
            protocol::method::INITIALIZE => Outgoing::success(
                id,
                json!({
                    "protocolVersion": protocol::PROTOCOL_VERSION,
                    "capabilities": { "tools": {} },
                    "serverInfo": ServerInfo {
                        name: self.name.clone(),
                        version: self.version.clone(),
                    },
                }),
            ),
            protocol::method::TOOLS_LIST => Outgoing::success(
                id,
                serde_json::to_value(self.list_tools()).unwrap_or_else(|_| json!({ "tools": [] })),
            ),
            protocol::method::TOOLS_CALL => self.call_tool(&id, incoming.params),
            other => Outgoing::failure(
                id,
                ErrorObject::new(
                    ErrorObject::METHOD_NOT_FOUND,
                    format!("Méthode inconnue : {other}"),
                ),
            ),
        };
        Some(outgoing.to_vec())
    }

    fn list_tools(&self) -> ToolsListResult {
        let tools = self
            .tools
            .iter()
            .map(|(name, def)| Tool {
                name: name.clone(),
                description: def.description.clone(),
                input_schema: Some(
                    def.schema
                        .clone()
                        .unwrap_or_else(|| json!({ "type": "object" })),
                ),
            })
            .collect();
        ToolsListResult { tools }
    }

    fn call_tool(&self, id: &Value, params: Option<Value>) -> Outgoing {
        let params = params.unwrap_or_else(|| json!({}));
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Outgoing::failure(
                id.clone(),
                ErrorObject::new(ErrorObject::INVALID_PARAMS, "params.name manquant"),
            );
        };
        let Some(tool) = self.tools.get(name) else {
            return Outgoing::failure(
                id.clone(),
                ErrorObject::new(
                    ErrorObject::INVALID_PARAMS,
                    format!("Outil inconnu : {name}"),
                ),
            );
        };
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let result = CallToolResult::from((tool.f)(&arguments));
        Outgoing::success(
            id.clone(),
            serde_json::to_value(result).unwrap_or(Value::Null),
        )
    }
}

/// Builder d'un serveur MCP stateless.
#[derive(Clone)]
pub struct ServerBuilder {
    name: String,
    version: String,
    route: String,
    allow_origins: Vec<String>,
    tools: BTreeMap<String, ToolDef>,
}

impl ServerBuilder {
    /// Nouveau builder — `name`/`version` sont les `serverInfo` MCP.
    pub fn new(name: &str, version: &str) -> Self {
        Self {
            name: name.to_string(),
            version: version.to_string(),
            route: "/mcp".to_string(),
            allow_origins: Vec::new(),
            tools: BTreeMap::new(),
        }
    }

    /// Change la route MCP (par défaut `/mcp`).
    pub fn route(mut self, route: &str) -> Self {
        self.route = route.to_string();
        self
    }

    /// Origines `Origin` autorisées (clients navigateur).
    ///
    /// Par défaut, toute requête portant un en-tête `Origin` est refusée
    /// (garde DNS-rebinding) — les clients CLI n'en envoient pas.
    pub fn allow_origins(mut self, origins: &[&str]) -> Self {
        self.allow_origins = origins.iter().map(|o| (*o).to_string()).collect();
        self
    }

    /// Enregistre un outil.
    ///
    /// La closure reçoit `arguments` (objet JSON) et renvoie la valeur
    /// structurée, ou un message d'erreur lisible par le client.
    pub fn tool<F>(mut self, name: &str, description: &str, f: F) -> Self
    where
        F: Fn(&Value) -> ToolResult + Send + Sync + 'static,
    {
        self.tools.insert(
            name.to_string(),
            ToolDef {
                description: Some(description.to_string()),
                schema: None,
                f: Arc::new(f),
            },
        );
        self
    }

    /// Construit le cœur de dispatch (tests, intégrations custom).
    pub fn core(&self) -> ServerCore {
        ServerCore {
            name: self.name.clone(),
            version: self.version.clone(),
            tools: Arc::new(self.tools.clone()),
        }
    }

    /// Bind le socket TCP — puis [`crate::http::BoundServer::serve`] bloque.
    pub fn bind(self, addr: &str) -> std::io::Result<crate::http::BoundServer> {
        let listener = std::net::TcpListener::bind(addr)?;
        let core = Arc::new(ServerCore {
            name: self.name,
            version: self.version,
            tools: Arc::new(self.tools),
        });
        Ok(crate::http::BoundServer {
            listener,
            core,
            route: self.route,
            allow_origins: self.allow_origins,
        })
    }
}
