//! Noyau de dispatch JSON-RPC + builder — indépendant du transport.

use crate::jsonrpc::{ErrorObject, Outgoing};
use crate::protocol::{self, CallToolResult, ServerInfo, Tool, ToolAnnotations, ToolsListResult};
use crate::zod::{Issues, Schema};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// Résultat d'un outil : `Ok(valeur)` ou `Err(message lisible par le client)`.
///
/// À l'ok, la valeur devient `structuredContent` : les objets JSON passent
/// tels quels ; toute autre valeur (scalaire, tableau, `null`) est enveloppée
/// dans `{ "result": … }` pour rester conforme à la spec MCP.
pub type ToolResult = Result<Value, String>;

type ToolFn = dyn Fn(&Value) -> Result<Value, ToolFailure> + Send + Sync;

/// Échec remonté par un wrapper de handler :
/// - `Tool(String)` → échec d'exécution (`isError: true`, HTTP 200) ;
/// - `Protocol(i64, String)` → erreur de protocole (ex. arguments qui ne
///   désérialisent pas dans le struct typé → `-32602`).
#[derive(Debug)]
enum ToolFailure {
    Tool(String),
    Protocol(i64, String),
}

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
    ///
    /// Conforme JSON-RPC : `id: null` explicite reçoit une réponse avec
    /// `id: null` ; version `jsonrpc` invalide ou lot → `-32600` ; JSON
    /// illisible → `-32700`.
    pub fn dispatch(&self, body: &[u8]) -> Option<Vec<u8>> {
        let incoming = match crate::jsonrpc::parse_incoming(body) {
            Ok(incoming) => incoming,
            Err(crate::jsonrpc::ParseFail::Syntax) => {
                return Some(Outgoing::parse_error().to_vec());
            }
            Err(crate::jsonrpc::ParseFail::Invalid(message)) => {
                return Some(
                    Outgoing::invalid_request(None, format!("requête invalide : {message}"))
                        .to_vec(),
                );
            }
        };
        if incoming.is_notification() {
            return None;
        }
        // id réel, ou null explicite : les deux exigent une réponse ; l'id
        // null explicite est rendu tel quel (Value::Null).
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
            let mut issues = Issues::new();
            schema.validate("arguments", &arguments, &mut issues);
            if !issues.is_empty() {
                let rendered = issues.render();
                return Outgoing::failure(
                    id.clone(),
                    ErrorObject::with_data(
                        ErrorObject::INVALID_PARAMS,
                        format!("Arguments invalides : {}", rendered.join(" ; ")),
                        json!({ "issues": rendered }),
                    ),
                );
            }
        }
        let result = match (tool.f)(&arguments) {
            Ok(value) => CallToolResult::from(Ok::<Value, String>(value)),
            Err(ToolFailure::Tool(message)) => CallToolResult::from(Err::<Value, String>(message)),
            Err(ToolFailure::Protocol(code, message)) => {
                return Outgoing::failure(id.clone(), ErrorObject::new(code, message));
            }
        };
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
    max_connections: usize,
    tools: BTreeMap<String, ToolDef>,
}

impl ServerBuilder {
    /// Nouveau builder — `name`/`version` sont les `serverInfo` MCP.
    #[must_use]
    pub fn new(name: &str, version: &str) -> Self {
        Self {
            name: name.to_string(),
            version: version.to_string(),
            route: "/mcp".to_string(),
            allow_origins: Vec::new(),
            read_timeout: crate::http::READ_TIMEOUT,
            max_connections: crate::http::MAX_CONNECTIONS,
            tools: BTreeMap::new(),
        }
    }

    /// Timeout de lecture par opération (anti-slowloris). Défaut : 30 s.
    ///
    /// Un client qui met plus de temps à produire sa requête est déconnecté
    /// sans réponse. Les tests passent une valeur courte.
    #[must_use]
    pub fn read_timeout(mut self, timeout: Duration) -> Self {
        self.read_timeout = timeout;
        self
    }

    /// Plafond de connexions simultanées (au-delà → `503` immédiat).
    /// Défaut : [`crate::http::MAX_CONNECTIONS`] (256).
    #[must_use]
    pub fn max_connections(mut self, max: usize) -> Self {
        self.max_connections = max;
        self
    }

    /// Change la route MCP (par défaut `/mcp`).
    #[must_use]
    pub fn route(mut self, route: &str) -> Self {
        self.route = route.to_string();
        self
    }

    /// Origines `Origin` autorisées (clients navigateur).
    ///
    /// Par défaut, toute requête portant un en-tête `Origin` est refusée
    /// (garde DNS-rebinding) — les clients CLI n'en envoient pas.
    #[must_use]
    pub fn allow_origins(mut self, origins: &[&str]) -> Self {
        self.allow_origins = origins.iter().map(|o| (*o).to_string()).collect();
        self
    }

    /// Enregistre un outil.
    ///
    /// La closure reçoit `arguments` (objet JSON) et renvoie la valeur
    /// structurée, ou un message d'erreur lisible par le client.
    /// Réenregistrer un nom existant remplace silencieusement l'outil
    /// précédent (le dernier enregistré gagne).
    pub fn tool<F>(mut self, name: &str, description: &str, f: F) -> Self
    where
        F: Fn(&Value) -> ToolResult + Send + Sync + 'static,
    {
        let wrapped = move |args: &Value| f(args).map_err(ToolFailure::Tool);
        self.tools.insert(
            name.to_string(),
            ToolDef {
                description: Some(description.to_string()),
                schema: None,
                annotations: None,
                f: Arc::new(wrapped),
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
    /// Réenregistrer un nom existant remplace silencieusement l'outil
    /// précédent.
    pub fn tool_typed<S, F>(mut self, name: &str, description: &str, schema: S, f: F) -> Self
    where
        S: Schema + 'static,
        F: Fn(&Value) -> ToolResult + Send + Sync + 'static,
    {
        let wrapped = move |args: &Value| f(args).map_err(ToolFailure::Tool);
        self.tools.insert(
            name.to_string(),
            ToolDef {
                description: Some(description.to_string()),
                schema: Some(Arc::new(schema)),
                annotations: None,
                f: Arc::new(wrapped),
            },
        );
        self
    }

    /// Comme [`ServerBuilder::tool_typed`], avec un handler **typé** :
    /// les arguments validés par le schéma sont désérialisés dans `T`
    /// (généralement un struct `#[derive(Deserialize)]`). Réenregistrer un
    /// nom existant remplace silencieusement l'outil précédent.
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
                    Ok(parsed) => f(parsed).map_err(ToolFailure::Tool),
                    Err(error) => Err(ToolFailure::Protocol(
                        ErrorObject::INVALID_PARAMS,
                        format!("arguments invalides pour le type attendu : {error}"),
                    )),
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
    #[must_use]
    pub fn annotate(mut self, tool: &str, annotations: ToolAnnotations) -> Self {
        if let Some(def) = self.tools.get_mut(tool) {
            def.annotations = Some(annotations);
        }
        self
    }

    /// Construit le cœur de dispatch (tests, intégrations custom).
    #[must_use]
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
            max_connections: self.max_connections,
            live_connections: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
    }
}
