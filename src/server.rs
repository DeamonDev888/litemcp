//! Noyau de dispatch JSON-RPC + builder — indépendant du transport.

use crate::jsonrpc::{ErrorObject, Incoming, Outgoing};
use crate::protocol::{self, CallToolResult, ServerInfo, Tool, ToolAnnotations, ToolsListResult};
use crate::zod::Schema;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// Résultat d'un outil : `Ok(valeur)` ou `Err(message lisible par le client)`.
pub type ToolResult = Result<Value, String>;

type ToolFn = dyn Fn(&Value) -> ToolResult + Send + Sync;

#[derive(Clone)]
struct ToolDef {
    description: Option<String>,
    schema: Option<Arc<dyn Schema>>,
    annotations: Option<ToolAnnotations>,
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
                annotations: def.annotations.clone(),
                input_schema: Some(
                    def.schema
                        .as_ref()
                        .map(|schema| schema.json())
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
        if let Some(schema) = &tool.schema {
            let mut issues = Vec::new();
            schema.validate("arguments", &arguments, &mut issues);
            if !issues.is_empty() {
                return Outgoing::failure(
                    id.clone(),
                    ErrorObject::with_data(
                        ErrorObject::INVALID_PARAMS,
                        format!("Arguments invalides : {}", issues.join(" ; ")),
                        json!({ "issues": issues }),
                    ),
                );
            }
        }
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
    read_timeout: Duration,
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
            read_timeout: crate::http::READ_TIMEOUT,
            tools: BTreeMap::new(),
        }
    }

    /// Timeout de lecture par opération (anti-slowloris). Défaut : 30 s.
    ///
    /// Un client qui met plus de temps à produire sa requête est déconnecté
    /// sans réponse. Les tests passent une valeur courte.
    pub fn read_timeout(mut self, timeout: Duration) -> Self {
        self.read_timeout = timeout;
        self
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
                annotations: None,
                f: Arc::new(f),
            },
        );
        self
    }

    /// Enregistre un outil avec un **schéma à la Zod** (module [`crate::zod`]).
    ///
    /// Le schéma dérive l'`inputSchema` de `tools/list` **et** valide chaque
    /// appel : un `tools/call` hors schéma reçoit `-32602` avec la liste des
    /// problèmes dans `error.data.issues` et n'exécute jamais le handler.
    /// La closure reçoit les arguments validés.
    pub fn tool_typed<S, F>(mut self, name: &str, description: &str, schema: S, f: F) -> Self
    where
        S: Schema + 'static,
        F: Fn(&Value) -> ToolResult + Send + Sync + 'static,
    {
        self.tools.insert(
            name.to_string(),
            ToolDef {
                description: Some(description.to_string()),
                schema: Some(Arc::new(schema)),
                annotations: None,
                f: Arc::new(f),
            },
        );
        self
    }

    /// Comme [`ServerBuilder::tool_typed`], avec un handler **typé** :
    /// les arguments validés par le schéma sont désérialisés dans `T`
    /// (généralement un struct `#[derive(Deserialize)]`).
    ///
    /// ```no_run
    /// use litemcp::prelude::*;
    /// use serde::Deserialize;
    ///
    /// #[derive(Deserialize)]
    /// struct AddArgs { a: i64, b: i64 }
    ///
    /// # fn main() -> std::io::Result<()> {
    /// ServerBuilder::new("calc", "1.0.0")
    ///     .tool_parse(
    ///         "add",
    ///         "Additionne deux entiers",
    ///         zod::object()
    ///             .field("a", zod::integer())
    ///             .field("b", zod::integer()),
    ///         |args: AddArgs| Ok(serde_json::json!({ "somme": args.a + args.b })),
    ///     )
    ///     .bind("127.0.0.1:8080")?
    ///     .serve()?;
    /// # Ok::<(), std::io::Error>(())
    /// # }
    /// ```
    pub fn tool_parse<S, T, F>(mut self, name: &str, description: &str, schema: S, f: F) -> Self
    where
        S: Schema + 'static,
        T: DeserializeOwned,
        F: Fn(T) -> ToolResult + Send + Sync + 'static,
    {
        let handler =
            Arc::new(
                move |value: &Value| match serde_json::from_value::<T>(value.clone()) {
                    Ok(parsed) => f(parsed),
                    Err(error) => Err(format!("arguments invalides : {error}")),
                },
            );
        self.tools.insert(
            name.to_string(),
            ToolDef {
                description: Some(description.to_string()),
                schema: Some(Arc::new(schema)),
                annotations: None,
                f: handler,
            },
        );
        self
    }

    /// Attache des annotations MCP (2026-07-28) à un outil déjà enregistré :
    /// `title`, `readOnlyHint`, `destructiveHint`, `idempotentHint`,
    /// `openWorldHint`. Sans effet si l'outil n'existe pas.
    pub fn annotate(mut self, tool: &str, annotations: ToolAnnotations) -> Self {
        if let Some(def) = self.tools.get_mut(tool) {
            def.annotations = Some(annotations);
        }
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
            read_timeout: self.read_timeout,
        })
    }
}
