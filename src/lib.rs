#![forbid(unsafe_code)]
//! # LiteMCP
//!
//! Le framework MCP (Model Context Protocol) le plus léger pour Rust :
//! **Streamable HTTP stateless sur un socket bindé — zéro runtime async,
//! `serde` + `serde_json` seulement.**
//!
//! Transport unique : Streamable HTTP — chaque message est un `POST /mcp`
//! sur un socket TCP bindé, la réponse arrive en JSON ou en flux SSE.
//! Aucun autre transport n'est implémenté.
//!
//! Schémas à la Zod ([`crate::zod`]) : dérivation de l'`inputSchema` et
//! validation des arguments avant exécution, handlers typés disponibles.
//! Le schéma publié est **exécuteur** : ce qu'il annonce est exactement ce
//! que la validation applique (pas de dérogation possible) — y compris les
//! **formats** (`.email()`, `.uuid()`, `.url()`, `.datetime()` : publiés
//! `format` et **appliqués**), les **valeurs par défaut**
//! ([`.default()`](crate::zod::Defaulted) : publiées **et** injectées),
//! les **dictionnaires** ([`record`](crate::zod::record)),
//! [`nullable`](crate::zod::nullable),
//! [`describe`](crate::zod::describe) et le
//! [`outputSchema`](crate::server::ServerBuilder::output_schema), qui
//! valide la **sortie** structurée avant envoi.
//!
//! Pour les schémas que le DSL n'exprime pas (`oneOf`, `not`,
//! `patternProperties`…), la feature `schemars` fournit
//! [`from_json_schema`](crate::schemars::from_json_schema) : un document
//! JSON Schema écrit à la main, publié tel quel **et** appliqué. La feature `schemars`
//! adapte en plus n'importe quel type `#[derive(JsonSchema)]`
//! ([`crate::schemars`]) avec les sémantiques JSON Schema de bout en bout.
//!
//! Trois surfaces MCP : **outils** (`tools/*`), **ressources**
//! (`resources/*`) et **prompts** (`prompts/*`) — chaque handler peut être
//! borné dans le temps ([`tool_timeout`](crate::server::ServerBuilder::tool_timeout)),
//! et le serveur s'arrête proprement
//! ([`handle`](crate::http::BoundServer::handle)/
//! [`shutdown`](crate::http::ServerHandle::shutdown)).
//! La feature `macros` (incluse par défaut) ajoute `#[tool]` : le schéma
//! d'entrée est dérivé des types des paramètres d'une fonction ordinaire.
//! Les contenus MCP riches sont servis à l'identique : `content[]` d'outil
//! **multiple et mixte** (texte, image, audio, `resource_link`, ressource
//! embarquée — [`tool_result`](crate::server::ServerBuilder::tool_result)),
//! messages de prompt avec image/ressource
//! ([`PromptMessage::user_content`](crate::protocol::PromptMessage::user_content)),
//! lectures de ressources **binaires** (`blob` base64,
//! [`resource_data`](crate::server::ServerBuilder::resource_data)),
//! `resources/templates/list`
//! ([`resource_template`](crate::server::ServerBuilder::resource_template))
//! et `completion/complete`
//! ([`completions`](crate::server::ServerBuilder::completions)).
//!
//! **Erreurs structurées** ([`ToolError`](crate::error)) : un échec d'outil
//! classé (`rate_limited`, `upstream`, `timeout`…) avec politique de
//! retente (`retryable`, `retryAfterMs`) part sur le fil sous
//! `structuredContent.error` — le client agent réagit programmatiquement
//! au lieu de parser la prose (les `Err(String)` restent acceptés).
//! Tous les messages destinés aux clients (validation, protocole, HTTP)
//! sont **en anglais**.
//!
//! **Observabilité** ([`on_call`](crate::server::ServerBuilder::on_call)) :
//! un callback par message dispatché — méthode, outil, durée, verdict —
//! pour compteurs, latence et taux d'erreur sans instrumenter votre
//! client.
//!
//! **Gateways** ([`Gateway`]) : le registre
//! d'outils devient vivant — `edit()` ajoute/retire des outils pendant que
//! le serveur sert, chaque requête part sur un snapshot immuable complet.
//! C'est la brique qui permet à un serveur MCP de refléter en direct des
//! serveurs détectés (outils proxy, catalogues qui apparaissent et
//! disparaissent) sans redémarrer ; le dispatch embarqué
//! ([`ServerCore`]) se monte alors dans la route
//! HTTP d'une app hôte qui possède déjà son propre socket durci.
//!
//! ```no_run
//! use litemcp::prelude::*;
//! use std::sync::Arc;
//!
//! # fn main() {
//! let gw = Gateway::new(
//!     ServerBuilder::new("gateway", "1.0.0")
//!         .tool("ping", "Sonner le gateway", |_a| Ok(serde_json::json!({ "pong": true }))),
//! );
//!
//! // Un serveur détecté apparaît : il est visible au prochain `tools/list`.
//! gw.edit(|b| b.tool("srv__echo", "Echo du serveur détecté", |a| Ok(a.clone())));
//!
//! // Il disparaît : l'outil est retiré du catalogue.
//! gw.edit(|mut b| {
//!     b.remove_tool("srv__echo");
//!     b
//! });
//! # }
//! ```
//!
//! **Anti-zombie**
//! ([`antizombie`], opt-in via
//! [`anti_zombie`](crate::server::ServerBuilder::anti_zombie)) : au bind, le
//! registre d'annonce partagé est débarrassé des annonces de process morts
//! et des débris d'écriture ; à la fin de vie, les connexions sont drainées
//! (attente bornée) et le bilan
//! [`ZombieReport`] — connexions restantes,
//! handlers encore vivants, sweep — est lisible via
//! [`zombie_report`](crate::http::ServerHandle::zombie_report).
//!
//! **Contrôle d'accès**
//! ([`on_request`](crate::server::ServerBuilder::on_request)) : un verdict
//! par requête HTTP — auth par token, rate-limit par source ou par clé —
//! rejeté en `401`/`403`/`429` **avant la lecture du corps**. TLS et OIDC
//! restent à la périphérie (gateway / mesh) ; le verrou applicatif, lui,
//! vit ici.
//!
//! Spec cible : MCP **2026-07-28** — protocole **stateless**, sans handshake :
//! chaque message est un `POST /mcp`, la réponse arrive en `application/json`
//! (ou en `text/event-stream` si le client ne demande que du SSE).
//!
//! - `server/discover` : implémentée (SEP-2575) — annonce les versions
//!   supportées, les capacités (`tools`, `resources`, `prompts` — toujours
//!   annoncées : les méthodes correspondantes sont servies même à registre
//!   vide) et l'identité du serveur ;
//! - `resultType` : présent dans **chaque** résultat (SEP-2322) ;
//! - `serverInfo` : le serveur s'identifie sous `_meta` de **chaque**
//!   résultat (SEP-2575) ;
//! - `ttlMs` / `cacheScope` : présents sur les résultats de liste et de
//!   lecture — `tools/list`, `resources/list`, `resources/templates/list`,
//!   `resources/read`, `prompts/list` (SEP-2549) ;
//! - contenus riches : `content[]` d'outil **multiple et mixte** (texte,
//!   image, audio, `resource_link`, ressource embarquée), messages de
//!   prompt image/ressource, lectures de ressources binaires (`blob` base64) ;
//! - `resources/templates/list` : catalogue des gabarits (URI templates
//!   RFC 6570) ;
//! - `completion/complete` : suggestions d'arguments — la capacité
//!   `completions` n'est annoncée que si un fournisseur est enregistré ;
//! - cycle multi round-trip (**MRTR**) : un handler d'outil, de prompt ou
//!   de ressource peut renvoyer un résultat `input_required`
//!   ([`InputRequiredSpec`](crate::protocol::InputRequiredSpec) —
//!   `inputRequests` + état opaque `requestState` **signé** HMAC-SHA256,
//!   TTL 10 min par défaut, liaison méthode/paramètres saillants) — le
//!   client réémet la requête sous un **id différent** avec
//!   `inputResponses` et l'écho exact du jeton ; une requête embarquée
//!   (`elicitation/create`, `sampling/createMessage`, `roots/list`) exige
//!   la capacité client correspondante (`-32021` sinon) ; l'état est borné
//!   (8 Kio) et vérifié à chaque tour (altéré, expiré, mal formé ou lié à
//!   une autre requête → `-32602`) ; le jeton est porteur et non consommé
//!   (la consommation unique, si requise, est à imposer dans le handler) ;
//!   **aucune capacité serveur supplémentaire n'est annoncée** — rien
//!   n'est servi au-delà des résultats `input_required` ;
//! - version de protocole : lue dans `_meta`, refusée en `-32022` si
//!   inconnue ; son absence est tolérée (compatibilité `2025-11-25`) ;
//! - `Mcp-Method` / `Mcp-Name` : acceptés et vérifiés contre le corps
//!   (SEP-2243) ;
//! - `initialize` et `ping` : tolérés (méthodes legacy `2025-11-25`).
//!
//! Tutoriel officiel pas à pas (du premier outil aux schémas, ressources
//! et prompts) : <https://deamondev888.github.io/litemcp-tuto/>.
//!
//! ```no_run
//! use litemcp::prelude::*;
//!
//! ServerBuilder::new("calc", "1.0.0")
//!     .tool("add", "Additionne deux nombres", |args| {
//!         let a = args["a"].as_i64().ok_or("a manquant")?;
//!         let b = args["b"].as_i64().ok_or("b manquant")?;
//!         Ok(serde_json::json!({ "somme": a + b }))
//!     })
//!     .tool("heavy", "Peut manquer de quota", |_args| {
//!         Err(ToolError::rate_limited("quota exhausted").retry_after_ms(500))
//!     })
//!     .on_call(|event| {
//!         eprintln!("{} {} -> {:?} in {:?}", event.method,
//!             event.name.unwrap_or("-"), event.status, event.duration);
//!     })
//!     .bind_default()?  // premier port libre de la tranche 23080–23099, repli éphémère
//!     .serve()?;        // bloque : POST /mcp → JSON ou SSE
//! # Ok::<(), std::io::Error>(())
//! ```

/// Anti-zombie — registre sans fantômes, drain de fin de vie, bilan
/// vérifiable (`ZombieReport`).
pub mod antizombie;
pub mod error;
/// Registre **dynamique** — la surface d'outils change pendant le service
/// ([`Gateway`]) : le socle des gateways qui
/// reflètent des serveurs détectés.
pub mod gateway;
pub mod http;
pub mod jsonrpc;
/// Macros `#[tool]`/`#[resource]`/`#[prompt]` (incluses par défaut) — voir
/// [`litemcp_macros`].
#[cfg(feature = "macros")]
pub mod macros {
    pub use litemcp_macros::{prompt, resource, tool};
}
pub mod protocol;
/// Annonce des serveurs au MCP Control (intégration native) —
/// opt-in via [`ServerBuilder::announce`].
pub mod registry;
/// Jetons `requestState` MRTR — état opaque **signé** (HMAC-SHA256), sans
/// état serveur : module interne du cycle multi round-trip (voir
/// [`crate::server::ServerBuilder::tool_mrtr`]).
mod request_state;
/// Adaptateur schemars (feature `schemars`) — voir [`crate::schemars`].
#[cfg(feature = "schemars")]
pub mod schemars;
pub mod server;
pub mod zod;

pub use antizombie::{SweepReport, ZombieReport, sweep_dir, sweep_registry};
pub use error::{ErrorCode, ToolError};
pub use gateway::Gateway;
pub use http::{RequestContext, RequestRejection};
pub use jsonrpc::ErrorObject;
pub use server::{
    CallEvent, CallStatus, DispatchResponse, HttpStatus, InputRound, McpHeaders, PromptOutcome,
    ProtocolProfile, ResourceOutcome, ServerBuilder, ServerCore, ToolOutcome, ToolResult,
};
pub use zod::WithHeader;

/// Version du crate.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Port par défaut des exemples et de la documentation — **23080**.
///
/// Un choix délibéré, opposé au conventionnel 8080 : c'est le port
/// HTTP-alternatif le plus squatté (proxys, Tomcat, serveurs de dev),
/// avec collisions garanties dans un arbre de processus local. **23080**
/// est en dessous de toutes les plages éphémères par défaut (Linux
/// `32768–60999`, Windows/macOS `49152–65535` — jamais alloué par l'OS
/// comme port source), non attribuée par l'IANA et sans serveur de
/// développement connu dessus. Pour plusieurs instances côte à côte,
/// ne choisissez rien à la main :
/// [`DEFAULT_PORT_RANGE`] + [`ServerBuilder::bind_default`] attribuent
/// automatiquement le premier port libre de la tranche.
pub const DEFAULT_PORT: u16 = 23080;

/// Adresse de bind explicite par défaut — [`DEFAULT_PORT`] sur le bouclage
/// local (pour [`ServerBuilder::bind`] ; l'auto-attribution vit dans
/// [`ServerBuilder::bind_default`]).
pub const DEFAULT_ADDRESS: &str = "127.0.0.1:23080";

/// Tranche de ports LiteMCP — **23080..=23099**.
///
/// Vingt ports pour vingt serveurs côte à côte (un par vendor d'infra,
/// par exemple) **sans coordination** : [`ServerBuilder::bind_default`]
/// prend le premier libre, et ne retombe sur un port éphémère alloué par
/// l'OS que si la tranche entière est occupée. La tranche est **sous**
/// toutes les plages éphémères par défaut — l'OS ne la donne donc jamais
/// spontanément à un socket sortant, et les serveurs LiteMCP y sont seuls.
pub const DEFAULT_PORT_RANGE: std::ops::RangeInclusive<u16> = 23080..=23099;

/// Tout ce qu'il faut pour écrire un serveur en quelques lignes.
pub mod prelude {
    pub use crate::antizombie::{SweepReport, ZombieReport, sweep_dir, sweep_registry};
    pub use crate::error::{ErrorCode, ToolError};
    pub use crate::gateway::Gateway;
    pub use crate::http::{RequestContext, RequestRejection, ServerHandle};
    #[cfg(feature = "macros")]
    pub use crate::macros::{prompt, resource, tool};
    pub use crate::protocol::{
        AudioContent, BlobResourceContents, CacheScope, CallToolResult, CompleteReference,
        Completion, CompletionArgument, CompletionRequest, ContentBlock, EmbeddedResourceContent,
        ImageContent, InputRequiredSpec, META_CLIENT_CAPABILITIES, META_PROTOCOL_VERSION,
        PROTOCOL_VERSION, PromptArgument, PromptMessage, ResourceData, ResourceLinkContent,
        ResourceTemplate, ResultType, SUPPORTED_VERSIONS, TextContent, ToolAnnotations,
    };
    #[cfg(feature = "schemars")]
    pub use crate::schemars::{from_json_schema, from_schemars};
    pub use crate::server::{
        CallEvent, CallStatus, DispatchResponse, HttpStatus, InputRound, McpHeaders, PromptOutcome,
        ProtocolProfile, ResourceOutcome, ServerBuilder, ToolOutcome, ToolResult,
    };
    pub use crate::zod::{self, Schema, WithHeader};
    pub use crate::{DEFAULT_ADDRESS, DEFAULT_PORT, DEFAULT_PORT_RANGE, VERSION};
}
