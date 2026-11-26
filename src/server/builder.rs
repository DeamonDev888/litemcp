//! `ServerBuilder` : configuration, enregistrement et construction du noyau.

use super::authz::{apply_rights, push_right};
use super::dispatch::extract_mcp_param_defs;
use super::pool::{HandlerPool, MAX_RESIDENT_HANDLERS};
use super::*;

/// Un `inputSchema` d'outil MCP est nécessairement un **objet** JSON : un
/// schéma racine booléen (`true`/`false` — valide en JSON Schema pour dire
/// « tout passe / rien ne passe ») n'est pas un `inputSchema` MCP valide et
/// est refusé à l'enregistrement (erreur de programmation, même convention
/// que `from_json_schema`). Les schémas booléens restent acceptés partout
/// où JSON Schema les autorise — sous-schémas (`properties.*`, `items`…),
/// `output_schema`, documents bruts — seul l'usage comme inputSchema racine
/// d'un outil est refusé.
///
/// Le **type racine** est vérifié aussi : `CallToolRequestParams.arguments`
/// est toujours un objet, donc un inputSchema dont la racine n'admet aucun
/// objet (`{"type": "string"}`, `{"enum": ["a", "b"]}`, une énumération
/// dérivée par schemars…) rendrait chaque appel fautif en `-32602`. C'est
/// une erreur de programmation : elle est refusée au démarrage, pas
/// découverte à l'usage.
fn require_object_input_schema(tool: &str, schema: &dyn crate::zod::Schema) {
    let document = schema.json();
    assert!(
        document.is_object(),
        "litemcp/server : l'inputSchema de l'outil `{tool}` doit être un objet JSON — \
         un schéma booléen racine (`true`/`false`) n'est pas un inputSchema MCP valide ; \
         les schémas booléens restent autorisés comme sous-schémas"
    );
    let root = resolve_local_root(&document);
    assert!(
        root_admits_object(root, 0),
        "litemcp/server : l'inputSchema de l'outil `{tool}` n'admet aucun objet à sa racine \
         (type publié : {}) — les arguments d'un outil MCP sont toujours un objet JSON \
         (`CallToolRequestParams.arguments`), un tel outil serait inutilisable (tout appel \
         rendrait -32602)",
        published_root_kind(root)
    );
}

/// Résout les `$ref` **locaux** d'une racine de document (`#/…`) — même
/// politique que l'adaptateur `schemars` : seuls les pointeurs internes sont
/// suivis (aucune résolution réseau), une référence inconnue, distante ou
/// cyclique laisse le nœud courant tel quel.
fn resolve_local_root(document: &Value) -> &Value {
    let mut current = document;
    for _ in 0..16 {
        let Some(reference) = current.get("$ref").and_then(Value::as_str) else {
            break;
        };
        let Some(pointer) = reference.strip_prefix('#') else {
            break;
        };
        let mut target = document;
        let mut resolved = true;
        for segment in pointer.split('/').filter(|segment| !segment.is_empty()) {
            let segment = segment.replace("~1", "/").replace("~0", "~");
            match target.get(&segment) {
                Some(next) => target = next,
                None => {
                    resolved = false;
                    break;
                }
            }
        }
        if !resolved || std::ptr::eq(target, current) {
            break;
        }
        current = target;
    }
    current
}

/// La racine d'un `inputSchema` admet-elle un **objet** ?
///
/// Seuls les cas **prouvables** sont refusés : un `type` publié qui
/// n'inclut pas `object`, un `enum`/`const` dont aucune valeur n'est un
/// objet, ou des combinateurs dont aucune branche n'admet d'objet. Une
/// racine sans `type` (mots-clés structurels seuls : `properties`,
/// `$ref` non résolu, `not`…) est acceptée — rien ne prouve qu'elle exclut
/// les objets.
fn root_admits_object(node: &Value, depth: usize) -> bool {
    if depth > 8 {
        return true;
    }
    let Some(object) = node.as_object() else {
        // Schéma booléen : `true` admet tout (déjà refusé comme racine
        // d'inputSchema), toute autre forme n'est pas un schéma.
        return node.as_bool().unwrap_or(true);
    };
    match object.get("type") {
        Some(Value::String(kind)) => return kind == "object",
        Some(Value::Array(kinds)) => {
            return kinds.iter().any(|kind| kind.as_str() == Some("object"));
        }
        Some(_) => return true,
        None => {}
    }
    if let Some(values) = object.get("enum").and_then(Value::as_array) {
        return values.iter().any(Value::is_object);
    }
    if let Some(value) = object.get("const") {
        return value.is_object();
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(branches) = object.get(key).and_then(Value::as_array) {
            return branches
                .iter()
                .any(|branch| root_admits_object(branch, depth + 1));
        }
    }
    if let Some(branches) = object.get("allOf").and_then(Value::as_array) {
        return branches
            .iter()
            .all(|branch| root_admits_object(branch, depth + 1));
    }
    true
}

/// Description courte du type racine publié — pour le message d'erreur
/// développeur (jamais renvoyée à un client).
fn published_root_kind(root: &Value) -> String {
    if let Some(kind) = root.get("type") {
        return kind.to_string();
    }
    if root.get("enum").is_some() {
        return "enum sans objet".to_string();
    }
    if root.get("const").is_some() {
        return "const non objet".to_string();
    }
    "racine sans type".to_string()
}

/// Joint les candidats de la liste `Access-Control-Allow-Headers` du
/// préflight : dédupliqués **sans tenir compte de la casse** (les noms
/// d'en-têtes sont insensibles à la casse, RFC 9110 §5.1), la première
/// graphie gagne, et l'ordre d'annonce est l'ordre d'insertion (stable :
/// base, signature, catalogue dans l'ordre du `BTreeMap`, déclarations
/// utilisateur).
fn joindre_en_tetes_preflight(candidats: impl IntoIterator<Item = String>) -> String {
    let mut retenus: Vec<String> = Vec::new();
    for nom in candidats {
        if !retenus
            .iter()
            .any(|existant| existant.eq_ignore_ascii_case(&nom))
        {
            retenus.push(nom);
        }
    }
    retenus.join(", ")
}

impl ServerBuilder {
    /// Nouveau builder — `name`/`version` sont les `serverInfo` MCP.
    ///
    /// La clé HMAC par défaut des `requestState` n'est **pas** tirée ici : elle
    /// l'est au premier noyau construit ([`Self::core`], [`Self::bind`]), et
    /// jamais si l'appelant fournit la sienne avec [`Self::request_state_key`].
    /// Construire un builder ne consomme donc aucune entropie, et un builder
    /// abandonné avant tout noyau ne peut pas paniquer faute de CSPRNG.
    #[must_use]
    pub fn new(name: &str, version: &str) -> Self {
        Self {
            name: name.to_string(),
            version: version.to_string(),
            route: "/mcp".to_string(),
            allow_origins: Vec::new(),
            allow_preflight_headers: Vec::new(),
            allowed_hosts: Vec::new(),
            read_timeout: crate::http::READ_TIMEOUT,
            write_deadline: None,
            max_connections: crate::http::MAX_CONNECTIONS,
            tools: BTreeMap::new(),
            annotations: BTreeMap::new(),
            output_schemas: BTreeMap::new(),
            required_capabilities: BTreeMap::new(),
            tool_rights: BTreeMap::new(),
            resource_rights: BTreeMap::new(),
            prompt_rights: BTreeMap::new(),
            approval_required: BTreeSet::new(),
            verifier: None,
            approval_policy: None,
            auth_hooks: Vec::new(),
            #[cfg(feature = "tls")]
            tls: None,
            resources: BTreeMap::new(),
            resource_mime_types: BTreeMap::new(),
            resource_templates: BTreeMap::new(),
            resource_template_handlers: BTreeMap::new(),
            template_titles: BTreeMap::new(),
            completions: None,
            prompts: BTreeMap::new(),
            connection_ttl: crate::http::CONNECTION_TTL,
            tool_timeout: Some(DEFAULT_TOOL_TIMEOUT),
            hooks: Vec::new(),
            request_hooks: Vec::new(),
            max_response_bytes: MAX_RESPONSE_BYTES,
            protocol_profile: ProtocolProfile::Strict2026_07_28,
            request_state_key: Arc::new(std::sync::OnceLock::new()),
            request_state_ttl: DEFAULT_REQUEST_STATE_TTL,
            announce: false,
            anti_zombie: false,
            drain_timeout: crate::antizombie::DRAIN_TIMEOUT,
            inflight_handlers: None,
        }
    }

    /// Injecte le compteur d'inflight partagé — réservé au
    /// [`Gateway`] : tous les noyaux d'un même
    /// gateway partagent CE compteur (un par gateway, jamais entre deux
    /// gateways ni avec un serveur indépendant, qui garde son compteur
    /// frais par `core()`/`bind()`).
    pub(crate) fn share_inflight_counter(
        mut self,
        counter: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Self {
        self.inflight_handlers = Some(counter);
        self
    }

    /// Cache de fils de handlers du noyau en construction — un par noyau
    /// indépendant (comme le compteur d'inflight), jamais partagé.
    fn handler_pool(&self) -> Arc<HandlerPool> {
        Arc::new(HandlerPool::new(MAX_RESIDENT_HANDLERS))
    }

    /// Compteur d'inflight pour le noyau en construction : celui injecté
    /// par le [`Gateway`] s'il existe, sinon un
    /// compteur frais (chaque noyau indépendant compte ses propres threads).
    fn inflight_counter(&self) -> Arc<std::sync::atomic::AtomicUsize> {
        self.inflight_handlers
            .clone()
            .unwrap_or_else(|| Arc::new(std::sync::atomic::AtomicUsize::new(0)))
    }

    /// Annonce ce serveur au MCP Control : au bind réussi, un JSON
    /// (`%TEMP%/litemcp-registry/<nom>.json`) publie le port effectif, le PID
    /// et la ligne de commande du process — MCP Control peut alors le démarrer,
    /// l'arrêter et le relancer nativement, sans scan de ports ni saisie
    /// manuelle. L'annonce est retirée à la fin de vie du serveur lié —
    /// arrêt gracieux **ou** `drop` sans `serve()` (voir le `Drop` de
    /// `BoundServer`). Opt-in, zéro coût quand il n'est pas appelé.
    #[must_use]
    pub fn announce(mut self) -> Self {
        self.announce = true;
        self
    }

    /// Sert la route MCP en **TLS 1.3** (feature `tls`, `docs/auth.md` §8).
    ///
    /// Le certificat et sa clé vivent en mémoire ([`crate::tls::TlsConfig`]) :
    /// la lib n'écrit rien sur disque. TLS 1.3 seulement (un ClientHello 1.2
    /// échoue à la poignée de main, sans aucun octet HTTP), ALPN `http/1.1`,
    /// pas de mTLS. L'empreinte du certificat servi est publiée par
    /// [`crate::http::BoundServer::certificate_fingerprint`] pour l'épinglage
    /// client ([`auth::PinnedServer`]) et reprise dans l'annonce MCP Control
    /// quand [`Self::announce`] est actif.
    ///
    /// ```no_run
    /// use litemcp::prelude::*;
    ///
    /// # fn main() -> std::io::Result<()> {
    /// let tls = TlsConfig::self_signed_ephemeral("localhost")
    ///     .expect("certificat éphémère en mémoire");
    /// let bound = ServerBuilder::new("calc", "1.0.0")
    ///     .tool("add", "Additionne", |_args| Ok(serde_json::json!({})))
    ///     .tls(tls)
    ///     .bind("127.0.0.1:23080")?;
    /// bound.serve()?;
    /// # Ok(())
    /// # }
    /// ```
    #[cfg(feature = "tls")]
    #[must_use]
    pub fn tls(mut self, config: crate::tls::TlsConfig) -> Self {
        self.tls = Some(config);
        self
    }

    /// Active le module **anti-zombie** (voir [`crate::antizombie`]) :
    ///
    /// - **au bind**, le registre d'annonce partagé est passé au peigne fin —
    ///   les annonces de process morts (port sans réponse) et les débris
    ///   d'écritures interrompues sont retirés, les instances vivantes et
    ///   les fichiers d'un schéma inconnu sont respectés ;
    /// - **à la fin de vie** du serveur lié (arrêt gracieux ou `drop`), les
    ///   threads de connexion sont drainés — attente bornée par
    ///   [`Self::drain_timeout`] — et le bilan
    ///   ([`crate::antizombie::ZombieReport`]) devient lisible via
    ///   [`crate::http::ServerHandle::zombie_report`].
    ///
    /// Opt-in strict : sans cet appel, aucun sweep, aucun drain, aucun code
    /// de plus sur les chemins chauds.
    #[must_use]
    pub fn anti_zombie(mut self) -> Self {
        self.anti_zombie = true;
        self
    }

    /// Durée maximale du **drain** anti-zombie à la fin de vie
    /// (défaut [`crate::antizombie::DRAIN_TIMEOUT`] — 2 s). Les threads ne
    /// sont jamais tués (Rust ne le permet pas) : au-delà de la durée, le
    /// bilan signale `drained: false` avec le compte de connexions
    /// restantes. `0` : bilan immédiat, sans attente. Sans effet si
    /// [`Self::anti_zombie`] n'est pas actif.
    #[must_use]
    pub fn drain_timeout(mut self, timeout: Duration) -> Self {
        self.drain_timeout = timeout;
        self
    }

    /// Définit le profil de protocole du serveur ([`ProtocolProfile::Strict2026_07_28`] par défaut).
    #[must_use]
    pub fn protocol_profile(mut self, profile: ProtocolProfile) -> Self {
        self.protocol_profile = profile;
        self
    }

    /// Active ou désactive le profil legacy 2025-11-25.
    #[must_use]
    pub fn legacy(mut self, enabled: bool) -> Self {
        self.protocol_profile = if enabled {
            ProtocolProfile::Legacy
        } else {
            ProtocolProfile::Strict2026_07_28
        };
        self
    }

    /// Exige qu'un client déclare la capacité `capability` dans
    /// `_meta["io.modelcontextprotocol/clientCapabilities"]` pour appeler
    /// l'outil `tool_name`.
    ///
    /// Si la capacité est absente ou non déclarée, l'appel est rejeté avec
    /// [`ErrorObject::CLIENT_CAPABILITY_REQUIRED`] (`-32021`) et HTTP 400.
    #[must_use]
    pub fn require_client_capability(mut self, tool_name: &str, capability: &str) -> Self {
        self.required_capabilities
            .entry(tool_name.to_string())
            .or_default()
            .push(capability.to_string());
        if let Some(tool) = self.tools.get_mut(tool_name) {
            if !tool.required_capabilities.contains(&capability.to_string()) {
                tool.required_capabilities.push(capability.to_string());
            }
        }
        self
    }

    /// Clé HMAC-SHA256 qui signe les `requestState` MRTR.
    ///
    /// Par défaut, une clé est récoltée au démarrage du builder : elle ne
    /// survit ni au redémarrage ni à une seconde instance. Un déploiement en
    /// flotte (gateways, répliques, reprises après redémarrage) doit fournir
    /// la **même** clé partout, sinon les jetons émis avant ne se vérifient
    /// plus (`-32602`) — c'est le seul mécanisme de reprise, aucun état
    /// serveur n'existe par ailleurs. Clé recommandée : ≥ 32 octets.
    ///
    /// # Panique
    ///
    /// Une clé vide est un bogue de configuration : panique explicite.
    ///
    /// ```no_run
    /// # use litemcp::prelude::*;
    /// let builder = ServerBuilder::new("mrtr", "1.0.0")
    ///     .request_state_key(b"une cle partagee par toute la flotte 32+");
    /// ```
    #[must_use]
    pub fn request_state_key(mut self, key: &[u8]) -> Self {
        assert!(
            !key.is_empty(),
            "request_state_key: la clé HMAC ne doit pas être vide"
        );
        self.request_state_key = Arc::new(std::sync::OnceLock::from(Arc::new(key.to_vec())));
        self
    }

    /// Durée de vie des `requestState` MRTR — un jeton présenté après son
    /// expiration est refusé en `-32602` (spec : « a short expiry (TTL) »).
    /// Défaut : 10 minutes.
    #[must_use]
    pub fn request_state_ttl(mut self, ttl: Duration) -> Self {
        self.request_state_ttl = ttl;
        self
    }

    /// Enregistre un outil **MRTR** (multi round-trip) : le handler reçoit
    /// [`ToolOutcome::InputRequired`] pour demander une entrée client et est
    /// **réappelé** au rejeu avec les `inputResponses` (non fiables) et
    /// l'état vérifié ([`InputRound`]).
    ///
    /// Le cycle est entièrement stateless : tout ce qui traverse les tours
    /// tient dans le `requestState` signé (HMAC-SHA256, TTL, liaison
    /// méthode/paramètres saillants/tour, identité de l'appelant sous
    /// autorisation) que le client renvoie octet pour octet avec un
    /// **id JSON-RPC différent** — aucune dépendance à une connexion, une
    /// session ou un process ; le rejeu peut atterrir sur une autre instance
    /// partageant la clé. Les autres arguments (`tool`, `tool_typed`…) ne
    /// changent pas ; `Mcp-Param-*` et `require_client_capability` restent
    /// applicables avant l'enregistrement.
    pub fn tool_mrtr<F>(mut self, name: &str, description: &str, f: F) -> Self
    where
        F: Fn(&Value, &InputRound) -> Result<ToolOutcome, ToolError> + Send + Sync + 'static,
    {
        let (output_schema, annotations, required_capabilities) = self.take_tool_pending(name);
        let mut def = ToolDef {
            description: Some(description.to_string()),
            schema: None,
            output_schema,
            output_object_rooted: false,
            annotations,
            required_capabilities,
            required_rights: Vec::new(),
            mcp_params: Vec::new(),
            handler: ToolHandler::Mrtr(Arc::new(f)),
        };
        def.sync_output_shape();
        self.tools.insert(name.to_string(), def);
        self
    }

    /// Comme [`Self::tool_mrtr`], avec un **schéma à la Zod** exécuteur
    /// ([`crate::zod`]) : l'`inputSchema` est publié dans `tools/list` et les
    /// arguments sont validés (avec injection des défauts et normalisation
    /// des entiers) avant chaque passage du handler MRTR.
    pub fn tool_mrtr_typed<S, F>(mut self, name: &str, description: &str, schema: S, f: F) -> Self
    where
        S: Schema + 'static,
        F: Fn(&Value, &InputRound) -> Result<ToolOutcome, ToolError> + Send + Sync + 'static,
    {
        let schema_arc = Arc::new(schema);
        require_object_input_schema(name, schema_arc.as_ref());
        let mcp_params = extract_mcp_param_defs(&schema_arc.json())
            .expect("invalid x-mcp-header in tool schema");
        let (output_schema, annotations, required_capabilities) = self.take_tool_pending(name);
        let mut def = ToolDef {
            description: Some(description.to_string()),
            schema: Some(schema_arc),
            output_schema,
            output_object_rooted: false,
            annotations,
            required_capabilities,
            required_rights: Vec::new(),
            mcp_params,
            handler: ToolHandler::Mrtr(Arc::new(f)),
        };
        def.sync_output_shape();
        self.tools.insert(name.to_string(), def);
        self
    }

    /// Enregistre un prompt **MRTR** — même cycle que [`Self::tool_mrtr`]
    /// sur `prompts/get` (méthode supportée par la spec) : le handler rend
    /// [`PromptOutcome::Messages`] ou [`PromptOutcome::InputRequired`].
    pub fn prompt_mrtr<F>(self, name: &str, description: &str, f: F) -> Self
    where
        F: Fn(&Value, &InputRound) -> Result<PromptOutcome, String> + Send + Sync + 'static,
    {
        self.prompt_mrtr_with_error(name, description, move |args, round| {
            f(args, round).map_err(ToolError::from)
        })
    }

    /// Comme [`Self::prompt_mrtr`], avec une erreur **structurée**
    /// ([`ToolError`]) : la classification part dans `error.data`.
    #[must_use]
    pub fn prompt_mrtr_with_error<F>(mut self, name: &str, description: &str, f: F) -> Self
    where
        F: Fn(&Value, &InputRound) -> Result<PromptOutcome, ToolError> + Send + Sync + 'static,
    {
        self.prompts.insert(
            name.to_string(),
            PromptDef {
                description: Some(description.to_string()),
                schema: None,
                required_rights: Vec::new(),
                handler: PromptHandler::Mrtr(Arc::new(f)),
            },
        );
        self
    }

    /// Comme [`Self::prompt_mrtr`], avec un **schéma à la Zod** exécuteur :
    /// les arguments sont publiés dans `prompts/list` et validés avant le
    /// handler MRTR.
    pub fn prompt_mrtr_typed<S, F>(self, name: &str, description: &str, schema: S, f: F) -> Self
    where
        S: Schema + 'static,
        F: Fn(&Value, &InputRound) -> Result<PromptOutcome, String> + Send + Sync + 'static,
    {
        self.prompt_mrtr_typed_with_error(name, description, schema, move |args, round| {
            f(args, round).map_err(ToolError::from)
        })
    }

    /// Comme [`Self::prompt_mrtr_typed`], avec une erreur **structurée**
    /// ([`ToolError`]).
    #[must_use]
    pub fn prompt_mrtr_typed_with_error<S, F>(
        mut self,
        name: &str,
        description: &str,
        schema: S,
        f: F,
    ) -> Self
    where
        S: Schema + 'static,
        F: Fn(&Value, &InputRound) -> Result<PromptOutcome, ToolError> + Send + Sync + 'static,
    {
        self.prompts.insert(
            name.to_string(),
            PromptDef {
                description: Some(description.to_string()),
                schema: Some(Arc::new(schema)),
                required_rights: Vec::new(),
                handler: PromptHandler::Mrtr(Arc::new(f)),
            },
        );
        self
    }

    /// Enregistre une ressource **MRTR** — même cycle que [`Self::tool_mrtr`]
    /// sur `resources/read` (méthode supportée par la spec) : le handler
    /// rend [`ResourceOutcome::Data`] ou [`ResourceOutcome::InputRequired`].
    /// L'entrée apparaît dans `resources/list` comme les autres ressources.
    pub fn resource_mrtr<F>(self, uri: &str, name: &str, description: &str, f: F) -> Self
    where
        F: Fn(&InputRound) -> Result<ResourceOutcome, String> + Send + Sync + 'static,
    {
        self.resource_mrtr_with_error(uri, name, description, move |round| {
            f(round).map_err(ToolError::from)
        })
    }

    /// Comme [`Self::resource_mrtr`], avec une erreur **structurée**
    /// ([`ToolError`]) : la classification (`code`, `retryable`,
    /// `retryAfterMs`) part dans `error.data`, comme pour les outils.
    #[must_use]
    pub fn resource_mrtr_with_error<F>(
        mut self,
        uri: &str,
        name: &str,
        description: &str,
        f: F,
    ) -> Self
    where
        F: Fn(&InputRound) -> Result<ResourceOutcome, ToolError> + Send + Sync + 'static,
    {
        let mime_type = self.take_resource_mime_type(uri);
        self.resources.insert(
            uri.to_string(),
            ResourceDef {
                name: name.to_string(),
                description: Some(description.to_string()),
                mime_type,
                required_rights: Vec::new(),
                handler: ResourceHandler::Mrtr(Arc::new(f)),
            },
        );
        self
    }

    /// Hook d'**observabilité** : un callback appelé après le traitement
    /// complet de chaque message JSON-RPC (succès, échec d'outil, erreur de
    /// protocole, notification).
    ///
    /// Un seul callback donne compteurs par outil, latence et taux d'erreur
    /// — sans `MOLTBOOK_DEBUG` ni `eprintln` dans votre client :
    ///
    /// ```no_run
    /// use litemcp::prelude::*;
    ///
    /// # fn main() -> std::io::Result<()> {
    /// ServerBuilder::new("calc", "1.0.0")
    ///     .tool("add", "Additionne", |_args| Ok(serde_json::json!({})))
    ///     .on_call(|event| {
    ///         eprintln!(
    ///             "{} {} → {:?} ({:?})",
    ///             event.method,
    ///             event.name.unwrap_or("-"),
    ///             event.status,
    ///             event.duration
    ///         );
    ///     })
    ///     .bind("127.0.0.1:23080")?
    ///     .serve()?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// - Un événement par message, **après** son traitement —
    ///   [`CallEvent::duration`] couvre le dispatch complet (validation
    ///   comprise) et l'exécution du handler ;
    /// - les corps JSON illisibles ne produisent **pas** d'événement (il
    ///   n'y a ni méthode ni outil à rapporter) ;
    /// - une panique du hook est attrapée : l'observation n'interrompt
    ///   jamais le service ;
    /// - les appels sont cumulables (chaque `.on_call` ajoute un hook) ;
    ///   le hook ne doit pas bloquer (il s'exécute dans le fil de la
    ///   requête).
    #[must_use]
    pub fn on_call<F>(mut self, f: F) -> Self
    where
        F: Fn(&CallEvent<'_>) + Send + Sync + 'static,
    {
        self.hooks.push(Arc::new(f));
        self
    }

    /// Verrou d'accès par requête — **auth** et **rate-limit** en process,
    /// avant toute exécution : le hook reçoit
    /// [`RequestContext`] (méthode, chemin,
    /// query, adresse du pair, en-têtes) et rend `Ok(())` ou un
    /// [`RequestRejection`] (`401`, `403`,
    /// `429` + `Retry-After`, statut arbitraire). Le premier rejet gagne.
    ///
    /// ```no_run
    /// use litemcp::prelude::*;
    ///
    /// const TOKEN: &str = "s3cret";
    ///
    /// # fn main() -> std::io::Result<()> {
    /// ServerBuilder::new("calc", "1.0.0")
    ///     .tool("add", "Additionne", |_args| Ok(serde_json::json!({})))
    ///     .on_request(|ctx| {
    ///         match ctx.header("Authorization").and_then(|v| v.strip_prefix("Bearer ")) {
    ///             Some(token) if token == TOKEN => Ok(()),
    ///             _ => Err(RequestRejection::unauthorized("missing or invalid bearer token")
    ///                 .with_header("WWW-Authenticate", "Bearer")),
    ///         }
    ///     })
    ///     .bind("127.0.0.1:23080")?
    ///     .serve()?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// - le hook s'exécute **avant** la lecture du corps et avant le
    ///   `Expect: 100-continue` : un rejet ne transporte jamais le payload ;
    /// - il ne s'exécute **pas** sur `/health` (les probes k8s ne
    ///   s'authentifient pas) ni sur une route inconnue (`404`) ni sur les
    ///   requêtes déjà refusées par le durcissement de transport (`400`,
    ///   `431`, `403` Origin, `411`, `413`) — le verrou utilisateur ne fait
    ///   qu'autoriser la route MCP ;
    /// - il s'exécute **avant** le verdict de méthode (`405`) : une requête
    ///   non autorisée ne doit pas apprendre que la route existe autrement
    ///   que par un refus d'authentification (`401`/`403` gagne sur `405`) ;
    /// - un rejet répond le statut avec le message en corps JSON et **ferme
    ///   la connexion** (le corps non lu ne peut pas être réinterprété) ;
    /// - les en-têtes supplémentaires ([`RequestRejection::with_header`])
    ///   sont assainis au départ : un CRLF injecté dans une valeur est
    ///   écarté, jamais réémis ;
    /// - une panique du hook est contenue (`500`, connexion fermée, le
    ///   serveur continue) ;
    /// - le hook tourne dans le fil de la connexion : gardez-le rapide
    ///   (vérification locale de signature/JWT) — un appel réseau distant
    ///   (introspection OAuth) doit porter **son propre** timeout, sinon il
    ///   retient le slot de connexion ;
    /// - derrière un reverse proxy, [`RequestContext::peer`](crate::http::RequestContext)
    ///   est l'adresse du proxy — pour un rate-limit par client final,
    ///   lisez `X-Forwarded-For` **après** avoir vérifié que `peer` est bien
    ///   votre proxy ;
    /// - les hooks sont cumulables (chaque `.on_request` ajoute un verdict,
    ///   exécuté dans l'ordre d'enregistrement) ; sans aucun hook, le
    ///   transport ne retient aucun en-tête et le chemin chaud est inchangé.
    #[must_use]
    pub fn on_request<F>(mut self, f: F) -> Self
    where
        F: Fn(&RequestContext<'_>) -> Result<(), RequestRejection> + Send + Sync + 'static,
    {
        self.request_hooks.push(Arc::new(f));
        self
    }

    /// Durée de vie maximale d'une connexion keep-alive — défaut 5 min.
    ///
    /// Borne le temps d'occupation d'un slot du plafond de connexions : sans
    /// elle, un client qui envoie une requête valide toutes les 29 s (sous
    /// le `read_timeout`) retient son slot **indéfiniment**, et 256 clients
    /// lents suffisent à renvoyer `503` à tous les clients légitimes.
    ///
    /// Valeur `0` : pas de TTL (comportement historique).
    #[must_use]
    pub fn connection_ttl(mut self, ttl: Duration) -> Self {
        self.connection_ttl = ttl;
        self
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

    /// Plafond de l'échéance **globale** d'écriture d'une réponse.
    ///
    /// Par défaut, l'envoi complet d'une réponse est borné à
    /// `read_timeout × 2`, borné à [`crate::http::WRITE_DEADLINE_MIN`]..=
    /// [`crate::http::WRITE_DEADLINE_MAX`] (soit 60 s avec les valeurs par
    /// défaut) : une réponse de 16 Mio exige donc environ 2,2 Mbit/s
    /// soutenus. Cette borne protège des clients qui lisent au compte-gouttes
    /// (un `write_vectored` seul ne les coupe jamais), mais elle tronque une
    /// réponse légitime sur un lien plus lent.
    ///
    /// Cette méthode **remplace** la valeur dérivée :
    /// `.write_deadline(Duration::from_secs(300))` laisse passer une réponse
    /// lente que le défaut couperait. Les valeurs inférieures à
    /// [`crate::http::WRITE_DEADLINE_MIN`] sont relevées à ce minimum (une
    /// échéance nulle couperait toutes les réponses).
    ///
    /// La borne ne concerne que l'**écriture** ; le budget de lecture
    /// (en-têtes + corps) reste celui de [`Self::read_timeout`].
    #[must_use]
    pub fn write_deadline(mut self, deadline: Duration) -> Self {
        self.write_deadline = Some(deadline);
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
    ///
    /// La valeur est **validée au [`ServerBuilder::bind`]** : elle doit
    /// commencer par `/`, ne pas contenir de `?` (la query est ignorée au
    /// routage) et ne pas masquer la sonde de vie
    /// ([`crate::http::HEALTH_ROUTE`]) — sans ce contrôle, un serveur
    /// configurerait silencieusement une route injoignable.
    #[must_use]
    pub fn route(mut self, route: &str) -> Self {
        self.route = route.to_string();
        self
    }

    /// Origines `Origin` autorisées (clients navigateur).
    ///
    /// Par défaut, toute requête portant un en-tête `Origin` non listé ici
    /// est refusée (garde DNS-rebinding) — les clients CLI n'en envoient
    /// pas. Exception par défaut : sur un listener lié à une adresse de
    /// boucle spécifique (`127.0.0.0/8`, `::1`), une origine `http://` dont
    /// l'autorité est identique au `Host` reçu **et** au littéral `ip:port`
    /// du listener est acceptée (client navigateur même-origine) — toute
    /// divergence de schéma/hôte/port, `Origin: null` et
    /// `Sec-Fetch-Site: cross-site` restent refusés. Sur un bind non local,
    /// cette liste est la seule porte (déploiements explicites/proxys).
    ///
    /// Une origine **listée** reçoit en plus les en-têtes CORS : le préflight
    /// `OPTIONS` de la route MCP rend `204` avec
    /// `Access-Control-Allow-Origin`, `Access-Control-Allow-Methods` et
    /// `Access-Control-Allow-Headers`, et les réponses `POST` portent
    /// `Access-Control-Allow-Origin` + `Vary: Origin` — `200` JSON, flux SSE
    /// **et** `202 Accepted` d'une notification (`notifications/initialized`
    /// est la première requête d'un client MCP après l'initialisation : sans
    /// ces en-têtes, le navigateur rejette le `202` et la session ne démarre
    /// pas). Un client navigateur cross-origin peut donc lire le résultat.
    /// Une origine absente de la liste reste refusée en `403` (filtre
    /// inchangé) et ne reçoit jamais d'en-tête CORS.
    #[must_use]
    pub fn allow_origins(mut self, origins: &[&str]) -> Self {
        self.allow_origins = origins.iter().map(|o| (*o).to_string()).collect();
        self
    }

    /// Annonce un **en-tête de requête** supplémentaire dans la réponse de
    /// préflight CORS (`OPTIONS`) — cumulable : chaque appel s'ajoute aux
    /// précédents.
    ///
    /// La liste `Access-Control-Allow-Headers` est calculée **une fois au
    /// bind** : liste de base (`Content-Type`, `Accept`,
    /// `MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Name`), complétée
    /// automatiquement par les en-têtes de signature (`Content-Digest`,
    /// `Signature-Input`, `Signature`) dès qu'un vérificateur
    /// ([`Self::verifier`]) est configuré, puis par les `Mcp-Param-<suffixe>`
    /// déclarés par les outils (SEP-2243). Cette méthode couvre le reste : un
    /// en-tête que **votre** application consomme mais que le crate ne peut
    /// pas deviner — un jeton `Authorization` lu par un hook
    /// [`Self::on_request`], une clé d'API maison. Un navigateur rejette au
    /// préflight toute requête portant un en-tête non annoncé : sans cette
    /// déclaration, le client cross-origin autorisé par
    /// [`Self::allow_origins`] restait bloqué.
    ///
    /// Les doublons sont éliminés sans tenir compte de la casse (la première
    /// graphie gagne) et déclarer un en-tête déjà couvert par la liste
    /// automatique est sans effet.
    ///
    /// # Panics
    ///
    /// Si `name` est vide ou contient autre chose que de l'ASCII (hors
    /// espaces et `:`) — même contrat que les `x-mcp-header` de [`crate::zod`].
    ///
    /// # Exemple
    ///
    /// ```no_run
    /// use litemcp::prelude::*;
    ///
    /// # fn main() -> std::io::Result<()> {
    /// ServerBuilder::new("calc", "1.0.0")
    ///     .tool("add", "Additionne", |_args| Ok(serde_json::json!({})))
    ///     .on_request(|ctx| {
    ///         ctx.header("Authorization")
    ///             .and_then(|v| v.strip_prefix("Bearer "))
    ///             .map(|_| ())
    ///             .ok_or_else(|| {
    ///                 RequestRejection::unauthorized("missing bearer token")
    ///             })
    ///     })
    ///     // Le hook lit `Authorization` : le préflight doit l'annoncer au
    ///     // navigateur, sinon la requête cross-origin est bloquée avant
    ///     // même d'atteindre le hook.
    ///     .allow_preflight_header("Authorization")
    ///     .bind("127.0.0.1:23080")?
    ///     .serve()?;
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn allow_preflight_header(mut self, name: &str) -> Self {
        assert!(
            !name.is_empty() && name.bytes().all(|b| b.is_ascii() && b != b' ' && b != b':'),
            "litemcp : en-tête de préflight `{name}` invalide — nom ASCII non vide, sans espace ni `:` requis"
        );
        if !self
            .allow_preflight_headers
            .iter()
            .any(|existant| existant.eq_ignore_ascii_case(name))
        {
            self.allow_preflight_headers.push(name.to_string());
        }
        self
    }

    /// Hôtes autorisés dans l'en-tête `Host` — garde anti-DNS-rebinding.
    ///
    /// Un `Host` hors liste est refusé en `421 Misdirected Request` **avant**
    /// le routage : une page hostile qui fait résoudre son propre nom vers
    /// `127.0.0.1` n'atteint plus le serveur, même quand elle n'envoie pas
    /// d'`Origin` — le cas que la garde `Origin`/`Sec-Fetch-Site` ne couvre
    /// pas. Les entrées sont comparées sans tenir compte de la casse, et le
    /// port de l'autorité est ignoré : `Host: mcp.exemple.com:443` est
    /// accepté par l'entrée `mcp.exemple.com` (c'est le **nom** qui décide de
    /// la résolution, donc de l'attaque).
    ///
    /// Par défaut (liste vide), seules les **autorités locales** sont
    /// acceptées : l'adresse IP effective du listener, un littéral de
    /// bouclage (`127.0.0.0/8`, `::1`, avec ou sans port) et **tout nom sans
    /// point** — la règle locale teste littéralement l'absence de `.`, donc
    /// un nom court non qualifié (`localhost`, `mon-service`, `test`) passe
    /// quel que soit son libellé. Tout nom **pleinement qualifié**, c'est-à-
    /// dire contenant au moins un point (`mcp.exemple.com`), ou tout littéral
    /// d'adresse étranger (`10.1.2.3`) est refusé en `421 Misdirected
    /// Request`. La règle s'appuie sur ce que le DNS rebinding **exige** : un
    /// nom résolvable depuis l'internet public, toujours qualifié (au moins
    /// un point) — un nom court ou un littéral de bouclage ne peut pas être
    /// détourné par un enregistrement DNS public.
    ///
    /// Une liste non vide **s'ajoute** aux autorités locales ci-dessus
    /// (elle ne les remplace pas : une configuration explicite ne doit pas
    /// casser l'exception loopback documentée par [`Self::allow_origins`]).
    /// C'est la forme attendue derrière un reverse proxy, où le `Host` reçu
    /// est le nom **public** : `.allowed_hosts(["mcp.exemple.com"])`.
    #[must_use]
    pub fn allowed_hosts<I, S>(mut self, hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allowed_hosts = hosts.into_iter().map(Into::into).collect();
        self
    }

    /// Plafond de taille d'une réponse JSON-RPC sérialisée (octets).
    ///
    /// Défaut : [`MAX_RESPONSE_BYTES`] (16 Mio). `0` = illimité (le contrôle
    /// est désactivé).
    ///
    /// Le verdict est appliqué sur la taille **totale** de la réponse
    /// sérialisée : il borne le transit, pas l'allocation. La sérialisation
    /// est néanmoins **bornée en mémoire** (writer borné, voir
    /// `BoundedReply`) : les octets au-delà du plafond ne sont jamais
    /// matérialisés — le writer cesse de stocker dès le plafond atteint et
    /// se contente de compter la taille exacte, que le message d'erreur
    /// rapporte (`response size (N bytes) …`, inchangé).
    ///
    /// La valeur `Value` rendue par le handler reste, elle, intégralement
    /// matérialisée **avant** sérialisation : ce plafond ne promet donc pas
    /// une borne totale du tas. Pour borner la mémoire, c'est la taille de
    /// la valeur rendue qu'il faut surveiller (un handler qui rend 100 Mo
    /// consomme 100 Mo avant d'être refusé) — ce plafond protège le
    /// réseau, pas le tas.
    #[must_use]
    pub fn max_response_bytes(mut self, max: usize) -> Self {
        self.max_response_bytes = max;
        self
    }

    /// Délai maximal d'exécution des **handlers** (outils, ressources,
    /// prompts) — défaut : [`DEFAULT_TOOL_TIMEOUT`] (30 s).
    ///
    /// Un handler qui dépasse le délai rend un résultat d'échec (`isError`
    /// pour un outil, erreur JSON-RPC `-32603` avec data `timeout` pour une
    /// ressource/prompt/complétion) et la requête est servie — le thread de
    /// connexion n'est plus retenu indéfiniment par du code utilisateur
    /// bloquant. Appeler cette méthode avec une valeur plus longue que le
    /// défaut ne fait que l'ajuster ; [`Self::no_tool_timeout`] le désactive.
    ///
    /// **Limite assumée** : le thread du handler ne peut pas être tué
    /// (Rust n'a pas d'annulation de thread) — il termine à son rythme en
    /// arrière-plan et son résultat est ignoré. N'utilisez pas ce délai
    /// comme mécanisme d'annulation d'effets de bord : il borne la
    /// **réponse**, pas l'exécution. Quand il est configuré (c'est le défaut),
    /// les arguments d'un `tools/call` sont clonés une fois par appel (le
    /// handler s'exécute dans un thread qui doit les posséder) et un appel
    /// d'outil **imbriqué** dans un handler déjà borné s'exécute sur le fil
    /// de son appelant (il hérite de son échéance — pas de pile de threads
    /// pour un dispatch imbriqué, et la garde anti-récursion de
    /// [`Gateway`](crate::Gateway) reste effective). Pour un effet de bord
    /// annulable, préférez le drapeau
    /// coopératif [`crate::cancel::is_cancelled`], levé quand le flux SSE du
    /// client se ferme.
    #[must_use]
    pub fn tool_timeout(mut self, timeout: Duration) -> Self {
        self.tool_timeout = Some(timeout);
        self
    }

    /// Désactive le délai d'exécution des handlers — comportement **illimité**.
    ///
    /// Le défaut est [`DEFAULT_TOOL_TIMEOUT`] (30 s) ; cette méthode revient
    /// au comportement antérieur : un handler bloquant retient indéfiniment
    /// son thread de connexion, [`MAX_INFLIGHT_HANDLERS`] n'est plus appliqué
    /// (le plafond ne s'applique qu'aux exécutions bornées) et un appel
    /// d'outil imbriqué n'a plus de coût de thread. À ne poser que si votre
    /// protocole de sortie garantit lui-même la borne.
    #[must_use]
    pub fn no_tool_timeout(mut self) -> Self {
        self.tool_timeout = None;
        self
    }

    /// Attache un **schéma de sortie** à un outil (module [`crate::zod`]).
    ///
    /// Le schéma est publié dans `tools/list` (`outputSchema`) **et
    /// appliqué** : un `structuredContent` non conforme devient un résultat
    /// `isError` — le schéma de sortie est exécuteur comme celui d'entrée.
    /// Comme [`Self::annotate`], l'ordre d'appel n'a pas d'importance.
    #[must_use]
    pub fn output_schema<S: Schema + 'static>(mut self, tool: &str, schema: S) -> Self {
        let schema = Arc::new(schema);
        if let Some(def) = self.tools.get_mut(tool) {
            def.output_schema = Some(schema);
            // La forme du `structuredContent` dépend du schéma de sortie :
            // elle change avec lui.
            def.sync_output_shape();
        } else {
            self.output_schemas.insert(tool.to_string(), schema);
        }
        self
    }

    /// Enregistre une **ressource** MCP (méthodes `resources/list` /
    /// `resources/read`).
    ///
    /// La clé est l'`uri` ; le handler produit le contenu **texte** à chaque
    /// lecture (rien n'est mis en cache par la lib). Pour du **binaire**
    /// (`blob` base64), utilisez [`Self::resource_data`] — jamais un texte
    /// qui simulerait le binaire. Réenregistrer une `uri` existante remplace
    /// silencieusement la définition. `resources/list` annonce
    /// `uri`/`name`/`description` ; le `mimeType` vient du
    /// [`crate::protocol::ResourceContents`] rendu par le handler.
    #[must_use]
    pub fn resource<F>(self, uri: &str, name: &str, description: &str, f: F) -> Self
    where
        F: Fn() -> Result<ResourceContents, String> + Send + Sync + 'static,
    {
        self.resource_with_error(uri, name, description, move || f().map_err(ToolError::from))
    }

    /// Récupère le `mime_type` statique en attente ou existant pour une
    /// ressource (`uri`), indépendamment de l'ordre d'appel.
    fn take_resource_mime_type(&mut self, uri: &str) -> Option<String> {
        let previous = self
            .resources
            .get(uri)
            .and_then(|def| def.mime_type.clone());
        self.resource_mime_types.remove(uri).or(previous)
    }

    /// Récupère `(title, mime_type)` en attente ou existants pour un gabarit
    /// (`uri_template`), indépendamment de l'ordre d'appel.
    fn take_template_meta(&mut self, uri_template: &str) -> (Option<String>, Option<String>) {
        let previous_title = self
            .resource_templates
            .get(uri_template)
            .and_then(|tpl| tpl.title.clone());
        let previous_mime = self
            .resource_templates
            .get(uri_template)
            .and_then(|tpl| tpl.mime_type.clone());
        let title = self.template_titles.remove(uri_template).or(previous_title);
        let mime_type = self
            .resource_mime_types
            .remove(uri_template)
            .or(previous_mime);
        (title, mime_type)
    }

    /// Déclare le **type MIME statique** d'une ressource (`resources/list`) ou
    /// d'un gabarit de ressource (`resources/templates/list`) — indépendant de
    /// l'ordre d'appel (avant ou après `.resource(...)` / `.resource_template(...)`).
    ///
    /// Les clients MCP connaissent ainsi le `mimeType` dès le listing, avant
    /// d'appeler `resources/read`.
    #[must_use]
    pub fn resource_mime_type(mut self, uri_or_template: &str, mime_type: &str) -> Self {
        let mut applied = false;
        if let Some(def) = self.resources.get_mut(uri_or_template) {
            def.mime_type = Some(mime_type.to_string());
            applied = true;
        }
        if let Some(tpl) = self.resource_templates.get_mut(uri_or_template) {
            tpl.mime_type = Some(mime_type.to_string());
            applied = true;
        }
        if !applied {
            self.resource_mime_types
                .insert(uri_or_template.to_string(), mime_type.to_string());
        }
        self
    }

    /// Déclare le **titre lisible** (`title`) d'un gabarit de ressource publié
    /// dans `resources/templates/list` — indépendant de l'ordre d'appel.
    #[must_use]
    pub fn resource_template_title(mut self, uri_template: &str, title: &str) -> Self {
        if let Some(tpl) = self.resource_templates.get_mut(uri_template) {
            tpl.title = Some(title.to_string());
        } else {
            self.template_titles
                .insert(uri_template.to_string(), title.to_string());
        }
        self
    }

    /// Comme [`Self::resource`], avec une erreur **structurée**
    /// ([`ToolError`]) : `code`, `retryable` et `retryAfterMs` partent dans
    /// `error.data` — même niveau que les outils, sans parser de prose.
    #[must_use]
    pub fn resource_with_error<F>(mut self, uri: &str, name: &str, description: &str, f: F) -> Self
    where
        F: Fn() -> Result<ResourceContents, ToolError> + Send + Sync + 'static,
    {
        let mime_type = self.take_resource_mime_type(uri);
        self.resources.insert(
            uri.to_string(),
            ResourceDef {
                name: name.to_string(),
                description: Some(description.to_string()),
                mime_type,
                required_rights: Vec::new(),
                handler: ResourceHandler::Plain(Arc::new(move || f().map(ResourceData::Text))),
            },
        );
        self
    }

    /// Enregistre une **ressource MCP** dont le handler choisit à chaque
    /// lecture entre contenu texte ([`ResourceData::text`]) et contenu
    /// **binaire** ([`ResourceData::binary`] / [`ResourceData::blob`] —
    /// `blob` base64, la forme `BlobResourceContents` de la spec).
    ///
    /// Même contrat que [`Self::resource`] (clé = `uri`, lecture à chaque
    /// appel, échec = `String`) ; l'entrée apparaît dans `resources/list`
    /// de la même façon. Réenregistrer une `uri` existante remplace la
    /// définition.
    ///
    /// ```no_run
    /// use litemcp::prelude::*;
    ///
    /// # fn main() -> std::io::Result<()> {
    /// ServerBuilder::new("assets", "1.0.0")
    ///     .resource_data("app://icon", "Icône", "Icône PNG de l'app", || {
    ///         Ok(ResourceData::binary(
    ///             "app://icon",
    ///             Some("image/png".into()),
    ///             &[0x89, 0x50, 0x4E, 0x47],
    ///         ))
    ///     })
    ///     .bind("127.0.0.1:23080")?
    ///     .serve()?;
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn resource_data<F>(self, uri: &str, name: &str, description: &str, f: F) -> Self
    where
        F: Fn() -> Result<ResourceData, String> + Send + Sync + 'static,
    {
        self.resource_data_with_error(uri, name, description, move || f().map_err(ToolError::from))
    }

    /// Comme [`Self::resource_data`], avec une erreur **structurée**
    /// ([`ToolError`]) : la classification part dans `error.data`.
    #[must_use]
    pub fn resource_data_with_error<F>(
        mut self,
        uri: &str,
        name: &str,
        description: &str,
        f: F,
    ) -> Self
    where
        F: Fn() -> Result<ResourceData, ToolError> + Send + Sync + 'static,
    {
        let mime_type = self.take_resource_mime_type(uri);
        self.resources.insert(
            uri.to_string(),
            ResourceDef {
                name: name.to_string(),
                description: Some(description.to_string()),
                mime_type,
                required_rights: Vec::new(),
                handler: ResourceHandler::Plain(Arc::new(f)),
            },
        );
        self
    }

    /// Annonce un **gabarit de ressource** (méthode
    /// `resources/templates/list`) — une URI template RFC 6570
    /// (`test://template/{id}/data`) décrit une famille de ressources.
    ///
    /// Le catalogue est un `BTreeMap` clé sur l'`uriTemplate` : réenregistrer
    /// un gabarit existant le remplace (en conservant son `title`/`mimeType`
    /// déclarés via [`Self::resource_template_title`] et
    /// [`Self::resource_mime_type`]). Pour servir aussi les lectures
    /// `resources/read` correspondant à un gabarit simple `{var}`, voir
    /// [`Self::resource_template_handler`].
    #[must_use]
    pub fn resource_template(mut self, uri_template: &str, name: &str, description: &str) -> Self {
        let (title, mime_type) = self.take_template_meta(uri_template);
        self.resource_templates.insert(
            uri_template.to_string(),
            ResourceTemplate {
                uri_template: uri_template.to_string(),
                name: name.to_string(),
                title,
                description: Some(description.to_string()),
                mime_type,
            },
        );
        self
    }

    /// Comme [`Self::resource_template`], avec un **handler de lecture
    /// dynamique** pour un gabarit RFC 6570 simple (`{var}`, segment non vide
    /// sans `/`).
    ///
    /// Lors d'un `resources/read`, une ressource statique exacte
    /// ([`Self::resource`] / [`Self::resource_data`]) prime toujours ; sinon,
    /// le premier gabarit simple correspondant à l'URI (dans l'ordre du
    /// `BTreeMap`) exécute ce handler avec `(uri, params)` où `params` associe
    /// chaque nom de variable à son segment extrait.
    ///
    /// # Panics
    ///
    /// Panique si `uri_template` n'est pas un gabarit simple `{nom}` (ex.
    /// opérateurs `{+x}`, `{?q}`, expressions accolées `{a}{b}` ou accolade
    /// non fermée) — erreur de programmation détectée au démarrage.
    #[must_use]
    pub fn resource_template_handler<F>(
        self,
        uri_template: &str,
        name: &str,
        description: &str,
        f: F,
    ) -> Self
    where
        F: Fn(&str, &BTreeMap<String, String>) -> Result<ResourceData, String>
            + Send
            + Sync
            + 'static,
    {
        self.resource_template_handler_with_error(
            uri_template,
            name,
            description,
            move |uri, params| f(uri, params).map_err(ToolError::from),
        )
    }

    /// Comme [`Self::resource_template_handler`], avec une erreur
    /// **structurée** ([`ToolError`]).
    #[must_use]
    pub fn resource_template_handler_with_error<F>(
        mut self,
        uri_template: &str,
        name: &str,
        description: &str,
        f: F,
    ) -> Self
    where
        F: Fn(&str, &BTreeMap<String, String>) -> Result<ResourceData, ToolError>
            + Send
            + Sync
            + 'static,
    {
        assert!(
            super::authz::is_simple_template(uri_template),
            "litemcp/server : `resource_template_handler` n'accepte que des gabarits RFC 6570 \
             simples `{{var}}` (reçu : `{uri_template}`)"
        );
        self = self.resource_template(uri_template, name, description);
        self.resource_template_handlers
            .insert(uri_template.to_string(), Arc::new(f));
        self
    }

    /// Retire un **gabarit de ressource** du registre — le pendant retrait
    /// de [`Self::resource_template`] pour les gateways dynamiques.
    /// Retourne `true` si le gabarit existait.
    pub fn remove_resource_template(&mut self, uri_template: &str) -> bool {
        self.resource_rights.remove(uri_template);
        self.resource_mime_types.remove(uri_template);
        self.template_titles.remove(uri_template);
        self.resource_template_handlers.remove(uri_template);
        self.resource_templates.remove(uri_template).is_some()
    }

    /// Enregistre le **fournisseur de complétion** (`completion/complete`) :
    /// suggestions d'arguments pour les prompts (`ref/prompt`) et les
    /// gabarits de ressources (`ref/resource`).
    ///
    /// Le fournisseur reçoit une [`CompletionRequest`] validée (référence,
    /// argument en cours, variables déjà résolues) et rend au plus 100
    /// suggestions ([`Completion`] — la borne de la spec : au-delà, le
    /// noyau tronque et pose `hasMore: true`). Un réenregistrement remplace
    /// le fournisseur précédent. La capacité `completions` n'est annoncée
    /// (`server/discover`) que si un fournisseur est enregistré — une
    /// capacité annoncée est une capacité servie.
    ///
    /// ```no_run
    /// use litemcp::prelude::*;
    ///
    /// # fn main() -> std::io::Result<()> {
    /// ServerBuilder::new("calc", "1.0.0")
    ///     .completions(|request: &CompletionRequest| {
    ///         match &request.reference {
    ///             CompleteReference::Prompt { name, .. } if name == "saluer" => {
    ///                 Ok(Completion::new(vec!["Bonjour".into(), "Salut".into()]))
    ///             }
    ///             _ => Ok(Completion::new(vec![])),
    ///         }
    ///     })
    ///     .bind("127.0.0.1:23080")?
    ///     .serve()?;
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn completions<F>(self, f: F) -> Self
    where
        F: Fn(&CompletionRequest) -> Result<Completion, String> + Send + Sync + 'static,
    {
        self.completions_with_error(move |request| f(request).map_err(ToolError::from))
    }

    /// Comme [`Self::completions`], avec une erreur **structurée**
    /// ([`ToolError`]) : la classification part dans `error.data`.
    #[must_use]
    pub fn completions_with_error<F>(mut self, f: F) -> Self
    where
        F: Fn(&CompletionRequest) -> Result<Completion, ToolError> + Send + Sync + 'static,
    {
        self.completions = Some(Arc::new(f));
        self
    }

    /// Enregistre un **prompt** MCP (méthodes `prompts/list` /
    /// `prompts/get`).
    ///
    /// Le handler reçoit `arguments` (objet JSON — non-objet refusé en
    /// `-32602`) et rend les messages du prompt (voir
    /// [`crate::protocol::PromptMessage::user`]/`::assistant`).
    /// Réenregistrer un nom existant remplace silencieusement la
    /// définition.
    #[must_use]
    pub fn prompt<F>(self, name: &str, description: &str, f: F) -> Self
    where
        F: Fn(&Value) -> Result<Vec<PromptMessage>, String> + Send + Sync + 'static,
    {
        self.prompt_with_error(name, description, move |args| {
            f(args).map_err(ToolError::from)
        })
    }

    /// Comme [`Self::prompt`], avec une erreur **structurée**
    /// ([`ToolError`]) : la classification part dans `error.data`.
    #[must_use]
    pub fn prompt_with_error<F>(mut self, name: &str, description: &str, f: F) -> Self
    where
        F: Fn(&Value) -> Result<Vec<PromptMessage>, ToolError> + Send + Sync + 'static,
    {
        self.prompts.insert(
            name.to_string(),
            PromptDef {
                description: Some(description.to_string()),
                schema: None,
                required_rights: Vec::new(),
                handler: PromptHandler::Plain(Arc::new(f)),
            },
        );
        self
    }

    /// Comme [`ServerBuilder::prompt`], avec un **schéma à la Zod** (module
    /// [`crate::zod`]) : les `arguments` d'un `prompts/get` sont validés
    /// **avant** l'exécution du handler — hors schéma → `-32602` avec la
    /// liste des problèmes dans `error.data.issues`, le handler ne tourne
    /// jamais. C'est la variante utilisée par la macro `#[prompt]` (feature
    /// `macros`).
    pub fn prompt_typed<S, F>(self, name: &str, description: &str, schema: S, f: F) -> Self
    where
        S: Schema + 'static,
        F: Fn(&Value) -> Result<Vec<PromptMessage>, String> + Send + Sync + 'static,
    {
        self.prompt_typed_with_error(name, description, schema, move |args| {
            f(args).map_err(ToolError::from)
        })
    }

    /// Comme [`Self::prompt_typed`], avec une erreur **structurée**
    /// ([`ToolError`]) : la classification part dans `error.data`.
    pub fn prompt_typed_with_error<S, F>(
        mut self,
        name: &str,
        description: &str,
        schema: S,
        f: F,
    ) -> Self
    where
        S: Schema + 'static,
        F: Fn(&Value) -> Result<Vec<PromptMessage>, ToolError> + Send + Sync + 'static,
    {
        self.prompts.insert(
            name.to_string(),
            PromptDef {
                description: Some(description.to_string()),
                schema: Some(Arc::new(schema)),
                required_rights: Vec::new(),
                handler: PromptHandler::Plain(Arc::new(f)),
            },
        );
        self
    }

    /// Comme [`Self::prompt_typed`], avec un handler **typé** : les arguments
    /// validés par le schéma sont désérialisés dans `T` (`DeserializeOwned`) —
    /// le pendant de [`Self::tool_parse`] pour les prompts.
    pub fn prompt_parse<S, T, F>(self, name: &str, description: &str, schema: S, f: F) -> Self
    where
        S: Schema + 'static,
        T: DeserializeOwned,
        F: Fn(T) -> Result<Vec<PromptMessage>, String> + Send + Sync + 'static,
    {
        self.prompt_parse_with_error(name, description, schema, move |args| {
            f(args).map_err(ToolError::from)
        })
    }

    /// Comme [`Self::prompt_parse`], avec une erreur **structurée**
    /// ([`ToolError`]).
    pub fn prompt_parse_with_error<S, T, F>(
        mut self,
        name: &str,
        description: &str,
        schema: S,
        f: F,
    ) -> Self
    where
        S: Schema + 'static,
        T: DeserializeOwned,
        F: Fn(T) -> Result<Vec<PromptMessage>, ToolError> + Send + Sync + 'static,
    {
        let handler = Arc::new(move |value: &Value| match serde_json::to_vec(value) {
            Err(error) => Err(ToolError::invalid_input(format!(
                "unreadable arguments: {error}"
            ))),
            Ok(buffer) => match serde_json::from_slice::<T>(&buffer) {
                Ok(parsed) => f(parsed),
                Err(error) => Err(ToolError::invalid_input(format!(
                    "arguments do not match the expected type: {error}"
                ))),
            },
        });
        self.prompts.insert(
            name.to_string(),
            PromptDef {
                description: Some(description.to_string()),
                schema: Some(Arc::new(schema)),
                required_rights: Vec::new(),
                handler: PromptHandler::Plain(handler),
            },
        );
        self
    }

    /// Enchaîne un enregistrement généré par une macro (`#[tool]`,
    /// `#[resource]`, `#[prompt]` — macros incluses par défaut) :
    /// `builder.register(register_add)` ou `builder.register(register_doc)`.
    ///
    /// Alias lisible de [`Self::with_tool`] pour les ressources et prompts.
    #[must_use]
    pub fn register(self, register: impl FnOnce(Self) -> Self) -> Self {
        register(self)
    }

    /// Enchaîne un enregistrement généré par une macro (`#[tool]`,
    /// `#[resource]`, `#[prompt]` — macros incluses par défaut) :
    /// `builder.with_tool(register_add)` où `register_add` est la fonction
    /// générée à côté de votre fonction annotée.
    #[must_use]
    pub fn with_tool(self, register: impl FnOnce(Self) -> Self) -> Self {
        register(self)
    }

    /// Récupère le schéma de sortie et les annotations d'un outil.
    ///
    /// Les déclarations **en attente** (`.output_schema()` / `.annotate()`
    /// appelées avant l'enregistrement de l'outil) priment ; à défaut, ce
    /// sont ceux de l'outil **déjà enregistré** qui sont repris.
    ///
    /// Sans cette conservation, un ré-enregistrement
    /// (`.tool("x", …).tool("x", …)`) perdait silencieusement les
    /// annotations et l'`outputSchema` déjà déclarés — le `tools/list`
    /// publié ne décrivait alors plus l'outil réellement servi.
    fn take_tool_pending(
        &mut self,
        name: &str,
    ) -> (
        Option<Arc<dyn Schema>>,
        Option<ToolAnnotations>,
        Vec<String>,
    ) {
        let previous_output = self
            .tools
            .get(name)
            .and_then(|def| def.output_schema.clone());
        let previous_annotations = self.tools.get(name).and_then(|def| def.annotations.clone());
        let previous_caps = self
            .tools
            .get(name)
            .map(|def| def.required_capabilities.clone())
            .unwrap_or_default();
        let output_schema = self.output_schemas.remove(name).or(previous_output);
        let annotations = self.annotations.remove(name).or(previous_annotations);
        let mut required_capabilities = self.required_capabilities.remove(name).unwrap_or_default();
        for cap in previous_caps {
            if !required_capabilities.contains(&cap) {
                required_capabilities.push(cap);
            }
        }
        (output_schema, annotations, required_capabilities)
    }

    /// Retire un outil du registre — le socle des gateways dynamiques
    /// (module [`crate::gateway`]) : un serveur détecté disparaît, son outil
    /// doit disparaître de `tools/list` sans redémarrer le process.
    ///
    /// Retourne `true` si l'outil existait. Les annotations et schémas de
    /// sortie **en attente** pour ce nom sont purgés en même temps : un
    /// ré-enregistrement ultérieur du même nom repart propre, sans
    /// ressusciter des métadonnées que l'opérateur a explicitement retirées.
    /// Un outil absent → `false`, aucun autre changement.
    pub fn remove_tool(&mut self, name: &str) -> bool {
        let existed = self.tools.remove(name).is_some();
        self.annotations.remove(name);
        self.output_schemas.remove(name);
        self.required_capabilities.remove(name);
        self.tool_rights.remove(name);
        self.approval_required.remove(name);
        existed
    }

    /// Retire une **ressource** du registre — le pendant retrait de
    /// [`Self::remove_tool`] pour les ressources : un serveur détecté
    /// disparaît, sa ressource disparaît de `resources/list`.
    ///
    /// Retourne `true` si la ressource existait. Une ressource absente →
    /// `false`, aucun autre changement.
    pub fn remove_resource(&mut self, uri: &str) -> bool {
        self.resource_rights.remove(uri);
        self.resource_mime_types.remove(uri);
        self.resources.remove(uri).is_some()
    }

    /// Retire un **prompt** du registre — le pendant retrait de
    /// [`Self::remove_tool`] pour les prompts.
    ///
    /// Retourne `true` si le prompt existait. Un prompt absent → `false`,
    /// aucun autre changement.
    pub fn remove_prompt(&mut self, name: &str) -> bool {
        self.prompt_rights.remove(name);
        self.prompts.remove(name).is_some()
    }

    /// Noms des outils enregistrés, dans l'ordre alphabétique (ordre du
    /// `tools/list` servi). Utile aux tests et aux gateways qui veulent
    /// comparer le registre à leur état détecté sans sérialiser.
    #[must_use]
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// Enregistre un outil.
    ///
    /// La closure reçoit `arguments` (objet JSON — un `arguments` non-objet
    /// est refusé en `-32602` avant tout appel du handler) et renvoie la
    /// valeur structurée, ou un message d'erreur lisible par le client.
    /// Réenregistrer un nom existant remplace l'outil précédent (le
    /// dernier enregistré gagne) — **les annotations et l'`outputSchema`
    /// déjà déclarés sont conservés**.
    pub fn tool<F>(mut self, name: &str, description: &str, f: F) -> Self
    where
        F: Fn(&Value) -> ToolResult + Send + Sync + 'static,
    {
        let wrapped =
            move |args: &Value| f(args).map(ToolOutcome::Value).map_err(ToolFailure::Tool);
        let (output_schema, annotations, required_capabilities) = self.take_tool_pending(name);
        let mut def = ToolDef {
            description: Some(description.to_string()),
            schema: None,
            output_schema,
            output_object_rooted: false,
            annotations,
            required_capabilities,
            required_rights: Vec::new(),
            mcp_params: Vec::new(),
            handler: ToolHandler::Plain(Arc::new(wrapped)),
        };
        def.sync_output_shape();
        self.tools.insert(name.to_string(), def);
        self
    }

    /// Enregistre un outil dont le handler produit **le résultat d'appel
    /// complet** — le chemin des contenus riches MCP : texte, image, audio,
    /// `resource_link`, ressource embarquée, en nombre et mélange libres
    /// ([`CallToolResult::from_content`]), avec `structuredContent`
    /// explicite si besoin.
    ///
    /// Le chemin historique ([`Self::tool`], [`ToolResult`]) reste inchangé
    /// : la valeur devient `structuredContent` avec l'enveloppe habituelle.
    /// Ici, au contraire, la sortie part **telle que construite** — seuls
    /// l'`outputSchema` (validé sur un `structuredContent` explicite) et le
    /// `_meta` d'identité serveur (SEP-2575) sont posés par le noyau.
    ///
    /// ```no_run
    /// use litemcp::prelude::*;
    ///
    /// # fn main() -> std::io::Result<()> {
    /// ServerBuilder::new("ecran", "1.0.0")
    ///     .tool_result("screenshot", "Capture d'écran", |_args| {
    ///         Ok(CallToolResult::from_content([
    ///             ContentBlock::text("Voici la capture :"),
    ///             ContentBlock::image_bytes(&[0x89, 0x50, 0x4E, 0x47], "image/png"),
    ///         ]))
    ///     })
    ///     .bind("127.0.0.1:23080")?
    ///     .serve()?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Réenregistrer un nom existant remplace l'outil précédent — les
    /// annotations et l'`outputSchema` déjà déclarés sont conservés, comme
    /// pour [`Self::tool`].
    pub fn tool_result<F>(mut self, name: &str, description: &str, f: F) -> Self
    where
        F: Fn(&Value) -> Result<CallToolResult, ToolError> + Send + Sync + 'static,
    {
        let wrapped =
            move |args: &Value| f(args).map(ToolOutcome::Result).map_err(ToolFailure::Tool);
        let (output_schema, annotations, required_capabilities) = self.take_tool_pending(name);
        let mut def = ToolDef {
            description: Some(description.to_string()),
            schema: None,
            output_schema,
            output_object_rooted: false,
            annotations,
            required_capabilities,
            required_rights: Vec::new(),
            mcp_params: Vec::new(),
            handler: ToolHandler::Plain(Arc::new(wrapped)),
        };
        def.sync_output_shape();
        self.tools.insert(name.to_string(), def);
        self
    }

    /// Comme [`Self::tool_result`], avec un **schéma à la Zod** exécuteur
    /// ([`crate::zod`]) : l'`inputSchema` est publié dans `tools/list` et les
    /// arguments sont validés avant l'appel du handler riche (`CallToolResult`).
    pub fn tool_result_typed<S, F>(mut self, name: &str, description: &str, schema: S, f: F) -> Self
    where
        S: Schema + 'static,
        F: Fn(&Value) -> Result<CallToolResult, ToolError> + Send + Sync + 'static,
    {
        let wrapped =
            move |args: &Value| f(args).map(ToolOutcome::Result).map_err(ToolFailure::Tool);
        let schema_arc = Arc::new(schema);
        require_object_input_schema(name, schema_arc.as_ref());
        let mcp_params = extract_mcp_param_defs(&schema_arc.json())
            .expect("invalid x-mcp-header in tool schema");
        let (output_schema, annotations, required_capabilities) = self.take_tool_pending(name);
        let mut def = ToolDef {
            description: Some(description.to_string()),
            schema: Some(schema_arc),
            output_schema,
            output_object_rooted: false,
            annotations,
            required_capabilities,
            required_rights: Vec::new(),
            mcp_params,
            handler: ToolHandler::Plain(Arc::new(wrapped)),
        };
        def.sync_output_shape();
        self.tools.insert(name.to_string(), def);
        self
    }

    /// Comme [`Self::tool_result_typed`], avec un handler **typé** : les
    /// arguments validés par le schéma sont désérialisés dans `T`
    /// (`DeserializeOwned`) — combinaison de [`Self::tool_parse`] et
    /// [`Self::tool_result`].
    pub fn tool_result_parse<S, T, F>(
        mut self,
        name: &str,
        description: &str,
        schema: S,
        f: F,
    ) -> Self
    where
        S: Schema + 'static,
        T: DeserializeOwned,
        F: Fn(T) -> Result<CallToolResult, ToolError> + Send + Sync + 'static,
    {
        let handler = Arc::new(move |value: &Value| match serde_json::to_vec(value) {
            Err(error) => Err(ToolFailure::Protocol(
                ErrorObject::INVALID_PARAMS,
                format!("unreadable arguments: {error}"),
            )),
            Ok(buffer) => match serde_json::from_slice::<T>(&buffer) {
                Ok(parsed) => f(parsed)
                    .map(ToolOutcome::Result)
                    .map_err(ToolFailure::Tool),
                Err(error) => Err(ToolFailure::Protocol(
                    ErrorObject::INVALID_PARAMS,
                    format!("arguments do not match the expected type: {error}"),
                )),
            },
        });
        let schema_arc = Arc::new(schema);
        require_object_input_schema(name, schema_arc.as_ref());
        let mcp_params = extract_mcp_param_defs(&schema_arc.json())
            .expect("invalid x-mcp-header in tool schema");
        let (output_schema, annotations, required_capabilities) = self.take_tool_pending(name);
        let mut def = ToolDef {
            description: Some(description.to_string()),
            schema: Some(schema_arc),
            output_schema,
            output_object_rooted: false,
            annotations,
            required_capabilities,
            required_rights: Vec::new(),
            mcp_params,
            handler: ToolHandler::Plain(handler),
        };
        def.sync_output_shape();
        self.tools.insert(name.to_string(), def);
        self
    }
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
        let wrapped =
            move |args: &Value| f(args).map(ToolOutcome::Value).map_err(ToolFailure::Tool);
        let schema_arc = Arc::new(schema);
        require_object_input_schema(name, schema_arc.as_ref());
        let mcp_params = extract_mcp_param_defs(&schema_arc.json())
            .expect("invalid x-mcp-header in tool schema");
        let (output_schema, annotations, required_capabilities) = self.take_tool_pending(name);
        let mut def = ToolDef {
            description: Some(description.to_string()),
            schema: Some(schema_arc),
            output_schema,
            output_object_rooted: false,
            annotations,
            required_capabilities,
            required_rights: Vec::new(),
            mcp_params,
            handler: ToolHandler::Plain(Arc::new(wrapped)),
        };
        def.sync_output_shape();
        self.tools.insert(name.to_string(), def);
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
    ///     .bind("127.0.0.1:23080")?
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
        // Pas de `value.clone()` : `serde_json::from_value` consomme le
        // `Value`. On sérialise donc une fois vers un buffer, puis on
        // désérialise depuis ce buffer — aucune copie de l'arbre JSON des
        // arguments (le clone coûtait jusqu'à 1 Mio par appel).
        let handler = Arc::new(move |value: &Value| match serde_json::to_vec(value) {
            Err(error) => Err(ToolFailure::Protocol(
                ErrorObject::INVALID_PARAMS,
                format!("unreadable arguments: {error}"),
            )),
            Ok(buffer) => match serde_json::from_slice::<T>(&buffer) {
                Ok(parsed) => f(parsed).map(ToolOutcome::Value).map_err(ToolFailure::Tool),
                Err(error) => Err(ToolFailure::Protocol(
                    ErrorObject::INVALID_PARAMS,
                    format!("arguments do not match the expected type: {error}"),
                )),
            },
        });
        let schema_arc = Arc::new(schema);
        require_object_input_schema(name, schema_arc.as_ref());
        let mcp_params = extract_mcp_param_defs(&schema_arc.json())
            .expect("invalid x-mcp-header in tool schema");
        let (output_schema, annotations, required_capabilities) = self.take_tool_pending(name);
        let mut def = ToolDef {
            description: Some(description.to_string()),
            schema: Some(schema_arc),
            output_schema,
            output_object_rooted: false,
            annotations,
            required_capabilities,
            required_rights: Vec::new(),
            mcp_params,
            handler: ToolHandler::Plain(handler),
        };
        def.sync_output_shape();
        self.tools.insert(name.to_string(), def);
        self
    }

    /// Attache des annotations MCP (2026-07-28) à un outil : `title`,
    /// `readOnlyHint`, `destructiveHint`, `idempotentHint`, `openWorldHint`.
    ///
    /// **L'ordre d'appel n'a plus d'importance** : les annotations sont
    /// conservées à part et fusionnées dans l'outil au moment de son
    /// enregistrement. Appeler `.annotate()` avant `.tool()` fonctionne donc
    /// exactement comme l'inverse.
    ///
    /// Si le nom ne correspond à aucun outil enregistré, l'annotation est
    /// **conservée** : elle sera appliquée si l'outil est enregistré plus
    /// tard. (Elle est ignorée si l'outil n'est jamais enregistré.)
    #[must_use]
    pub fn annotate(mut self, tool: &str, annotations: ToolAnnotations) -> Self {
        // Applique immédiatement si l'outil existe déjà, sinon met en attente.
        if let Some(def) = self.tools.get_mut(tool) {
            def.annotations = Some(annotations);
        } else {
            self.annotations.insert(tool.to_string(), annotations);
        }
        self
    }

    /// Fusionne dans `tools` les annotations et schémas de sortie **en
    /// attente** — déclarés avant l'enregistrement de l'outil, ou liés à un
    /// outil réenregistré — et recalcule la forme du `structuredContent`
    /// associée (elle dépend du schéma de sortie). Utilisé par
    /// [`Self::core`] et [`Self::bind`].
    fn merge_pending(&self, tools: &mut BTreeMap<String, ToolDef>) {
        for (name, def) in tools.iter_mut() {
            if let Some(annotations) = self.annotations.get(name) {
                def.annotations = Some(annotations.clone());
            }
            if let Some(output) = self.output_schemas.get(name) {
                def.output_schema = Some(Arc::clone(output));
            }
            if let Some(caps) = self.required_capabilities.get(name) {
                for cap in caps {
                    if !def.required_capabilities.contains(cap) {
                        def.required_capabilities.push(cap.clone());
                    }
                }
            }
            def.sync_output_shape();
        }
        apply_rights(&self.tool_rights, tools.iter_mut(), |def| {
            &mut def.required_rights
        });
    }

    /// Déclare un **droit exigé** pour appeler cet outil — même style que
    /// [`Self::annotate`], indépendant de l'ordre d'appel.
    ///
    /// Plusieurs droits sur un même outil sont **tous** exigés (ET) ; un
    /// droit est une chaîne opaque, comparée exactement (aucun joker, aucune
    /// hiérarchie). Sans autorisation active (aucun [`Self::verifier`]), la
    /// déclaration est **inerte** : le comportement du serveur reste
    /// inchangé.
    ///
    /// Dès qu'un vérificateur est configuré, un outil **sans droit déclaré**
    /// est refusé (`-32023` / `right_missing`) et n'apparaît plus dans
    /// `tools/list` — échec fermé.
    ///
    /// ```no_run
    /// # use litemcp::prelude::*;
    /// # fn main() {
    /// # let _builder =
    /// ServerBuilder::new("db", "1.0.0")
    ///     .tool("db.query", "Lit la base", |_args| Ok(serde_json::json!({})))
    ///     .require_right("db.query", "db.read")
    ///     .require_right("db.query", "tenant.acme")
    /// # ;
    /// # }
    /// ```
    #[must_use]
    pub fn require_right(mut self, tool: &str, right: &str) -> Self {
        push_right(self.tool_rights.entry(tool.to_string()).or_default(), right);
        self
    }

    /// Déclare un **droit exigé** pour lire cette ressource (`resources/read`)
    /// ou ce gabarit (`resources/templates/list`).
    ///
    /// La clé est l'`uri` d'une ressource enregistrée ou l'`uriTemplate` d'un
    /// gabarit : les deux catalogues sont indépendants (un gabarit n'a pas de
    /// handler de lecture dans ce crate, son droit ne porte donc que sur sa
    /// présence dans la liste). Mêmes règles que [`Self::require_right`] :
    /// tous exigés, inerte sans vérificateur, refus fermé sans droit déclaré.
    #[must_use]
    pub fn require_resource_right(mut self, uri: &str, right: &str) -> Self {
        push_right(
            self.resource_rights.entry(uri.to_string()).or_default(),
            right,
        );
        self
    }

    /// Déclare un **droit exigé** pour rendre ce prompt (`prompts/get`) —
    /// mêmes règles que [`Self::require_right`].
    #[must_use]
    pub fn require_prompt_right(mut self, name: &str, right: &str) -> Self {
        push_right(
            self.prompt_rights.entry(name.to_string()).or_default(),
            right,
        );
        self
    }

    /// Active l'**autorisation** : ce vérificateur établit l'identité de
    /// chaque requête (le transport l'appelle via
    /// [`ServerCore::authenticate`]).
    ///
    /// C'est la seule activation : **sans** vérificateur, ni droits, ni
    /// listes filtrées, ni confirmation ne s'appliquent — le comportement
    /// du crate est strictement celui d'avant.
    ///
    /// Une panique du vérificateur est contenue et vaut refus (voir
    /// [`auth::verify_or_refuse`]).
    ///
    /// # Transport HTTP
    ///
    /// Le transport HTTP intégré est branché sur
    /// [`ServerCore::authenticate`] : sur la route MCP, après tous les
    /// contrôles de transport (cadrage, `Host`, `Origin`, méthode), il
    /// construit la [`auth::VerifiedRequest`] — méthode, chemin, autorité
    /// `Host`, en-têtes bruts retenus pour l'occasion, corps — et appelle
    /// le vérificateur. Un refus reçoit `401 Unauthorized` avec l'en-tête
    /// [`auth::WWW_AUTHENTICATE`] et un corps **constant** : le motif
    /// précis ne sort jamais sur le réseau, il ne part que vers le journal
    /// applicatif ([`Self::on_auth_decision`]). Un succès établit la
    /// [`auth::AuthContext`] et le dispatch est fait avec
    /// ([`ServerCore::dispatch_authenticated`], variante SSE comprise) :
    /// droits, listes filtrées et confirmation s'appliquent. Sans
    /// vérificateur, ce chemin n'existe pas et le dispatch historique est
    /// servi tel quel. Les hôtes qui possèdent leur propre socket passent
    /// par [`ServerCore::authenticate`] puis
    /// [`ServerCore::dispatch_authenticated`] — ou, en un seul appel, par
    /// [`ServerCore::dispatch_verified`].
    #[must_use]
    pub fn verifier(mut self, verifier: Arc<dyn auth::Verifier>) -> Self {
        self.verifier = Some(verifier);
        self
    }

    /// Marque cet outil comme exigeant une **confirmation externe** avant
    /// exécution (voir [`Self::approval_policy`]).
    ///
    /// La politique est interrogée **après** les droits et **avant** le
    /// handler, avec les arguments exacts de l'appel. Sans politique
    /// configurée, l'outil est **refusé** (échec fermé : un outil marqué
    /// sensible ne s'exécute pas faute de décideur). Sans vérificateur,
    /// l'autorisation est inactive et cette déclaration est inerte.
    #[must_use]
    pub fn require_approval(mut self, tool: &str) -> Self {
        self.approval_required.insert(tool.to_string());
        self
    }

    /// Fournit la **politique de confirmation** interrogée pour les outils
    /// marqués par [`Self::require_approval`].
    ///
    /// `Approved` laisse le handler s'exécuter ; `Denied` et `Pending`
    /// rendent `403` / `-32023` sans rien exécuter (ni file d'attente, ni
    /// état conservé). Une panique de la politique est traitée comme un
    /// refus.
    #[must_use]
    pub fn approval_policy(mut self, policy: Arc<dyn auth::ApprovalPolicy>) -> Self {
        self.approval_policy = Some(policy);
        self
    }

    /// Enregistre un hook d'**audit** des décisions d'autorisation —
    /// cumulable (plusieurs hooks, appelés dans l'ordre d'enregistrement).
    ///
    /// L'événement [`auth::AuthDecisionEvent`] porte le verdict et le motif
    /// **précis** (`right_missing`, `approval_denied`, `approval_policy_panicked`…),
    /// que le réseau ne voit jamais. Une panique du hook est contenue : le
    /// service continue.
    ///
    /// ```no_run
    /// # use litemcp::prelude::*;
    /// # use std::sync::Arc;
    /// # fn main() {
    /// # let verificateur: Arc<dyn litemcp::auth::Verifier> =
    /// #     Arc::new(litemcp::auth::TestVerifier::accepting(
    /// #         litemcp::auth::AuthContext::new("demo", litemcp::auth::ProofMethod::Application)));
    /// # let _builder =
    /// ServerBuilder::new("db", "1.0.0")
    ///     .verifier(verificateur)
    ///     .on_auth_decision(|event| {
    ///         eprintln!(
    ///             "{} {:?} client={:?} -> {:?} ({:?})",
    ///             event.method, event.name, event.client, event.verdict, event.reason
    ///         );
    ///     })
    /// # ;
    /// # }
    /// ```
    #[must_use]
    pub fn on_auth_decision<F>(mut self, f: F) -> Self
    where
        F: Fn(&auth::AuthDecisionEvent<'_>) + Send + Sync + 'static,
    {
        self.auth_hooks.push(Arc::new(f));
        self
    }

    /// Construit le cœur de dispatch (tests, intégrations custom).
    ///
    /// **Instantané** : le cœur capture les outils présents à cet instant.
    /// Un outil enregistré *après* l'appel n'y figure pas — utilisez
    /// [`ServerBuilder::bind`] pour un serveur complet.
    ///
    /// # Panique
    ///
    /// Tire la clé HMAC par défaut des `requestState` (`harvest_key`,
    /// `getrandom`) si aucune clé n'a été fournie par
    /// [`Self::request_state_key`] : sans entropie système (seccomp, cible non
    /// supportée), la construction du noyau panique — le crate n'a pas de repli
    /// faible par choix.
    #[must_use]
    pub fn core(&self) -> ServerCore {
        // Point 3.5 : la clé est tirée **ici** (première construction du
        // noyau), pas dans `new()` — un builder inutilisé ne consomme pas
        // d'entropie, et une clé explicite n'en consomme jamais.
        let request_state_key = Arc::clone(
            self.request_state_key
                .get_or_init(|| Arc::new(harvest_key())),
        );
        let mut tools = self.tools.clone();
        self.merge_pending(&mut tools);
        let mut resources = self.resources.clone();
        apply_rights(&self.resource_rights, resources.iter_mut(), |def| {
            &mut def.required_rights
        });
        let mut prompts = self.prompts.clone();
        apply_rights(&self.prompt_rights, prompts.iter_mut(), |def| {
            &mut def.required_rights
        });
        let mut templates = self.templates();
        apply_rights(&self.resource_rights, templates.iter_mut(), |def| {
            &mut def.required_rights
        });
        let auth_gate = self.auth_gate();
        let meta = json!({
            protocol::META_SERVER_INFO: ServerInfo {
                name: self.name.clone(),
                version: self.version.clone(),
            }
        });
        ServerCore {
            name: self.name.clone(),
            version: self.version.clone(),
            meta,
            tools: Arc::new(tools),
            resources: Arc::new(resources),
            templates: Arc::new(templates),
            completions: self.completions.clone(),
            prompts: Arc::new(prompts),
            tool_timeout: self.tool_timeout,
            max_inflight_handlers: MAX_INFLIGHT_HANDLERS,
            inflight_handlers: self.inflight_counter(),
            handler_pool: self.handler_pool(),
            max_response_bytes: self.max_response_bytes,
            hooks: Arc::new(self.hooks.clone()),
            auth_gate,
            protocol_profile: self.protocol_profile,
            request_state_key,
            request_state_ttl: self.request_state_ttl,
            tools_list_cache: std::sync::OnceLock::new(),
            resources_list_cache: std::sync::OnceLock::new(),
            templates_list_cache: std::sync::OnceLock::new(),
            prompts_list_cache: std::sync::OnceLock::new(),
            discover_cache: std::sync::OnceLock::new(),
            initialize_cache: std::sync::OnceLock::new(),
            ping_cache: std::sync::OnceLock::new(),
        }
    }

    /// État d'autorisation du noyau à construire — `None` sans vérificateur
    /// (autorisation inactive, comportement historique).
    fn auth_gate(&self) -> Option<Arc<AuthGate>> {
        self.verifier.as_ref().map(|verifier| {
            Arc::new(AuthGate {
                verifier: Arc::clone(verifier),
                approval_policy: self.approval_policy.clone(),
                approval_required: self.approval_required.clone(),
                decision_hooks: self.auth_hooks.clone(),
            })
        })
    }

    /// Catalogue des gabarits avec le droit déclaré et le handler éventuel de chacun.
    fn templates(&self) -> BTreeMap<String, TemplateDef> {
        self.resource_templates
            .iter()
            .map(|(key, template)| {
                (
                    key.clone(),
                    TemplateDef {
                        template: template.clone(),
                        required_rights: Vec::new(),
                        handler: self.resource_template_handlers.get(key).cloned(),
                    },
                )
            })
            .collect()
    }

    /// Bind le socket TCP — puis [`crate::http::BoundServer::serve`] bloque.
    ///
    /// La route est validée ici (voir [`ServerBuilder::route`]) : une route
    /// qui ne peut jamais matcher ou qui masque la sonde de vie est refusée
    /// en `InvalidInput` plutôt que de démarrer un serveur injoignable.
    pub fn bind(self, addr: &str) -> std::io::Result<crate::http::BoundServer> {
        self.validate_route()?;
        let listener = std::net::TcpListener::bind(addr)?;
        self.finish(listener)
    }

    /// Bind sur `127.0.0.1` en **cherchant un port libre** dans la tranche
    /// LiteMCP ([`crate::DEFAULT_PORT_RANGE`] — `23080..=23099`) : le
    /// premier port disponible gagne, de sorte que le deuxième, le
    /// dixième… serveur LiteMCP d'une machine démarre sans coordination
    /// ni collision. Si la tranche entière est occupée, repli sur un
    /// **port éphémère** alloué par l'OS — jamais d'échec faute de place
    /// dans la tranche. Le port effectif est lisible via
    /// [`crate::http::BoundServer::local_addr`] et publié par
    /// [`ServerBuilder::announce`] si elle est active. La validation de
    /// route est identique à [`ServerBuilder::bind`].
    ///
    /// ```no_run
    /// use litemcp::prelude::*;
    ///
    /// # fn main() -> std::io::Result<()> {
    /// let bound = ServerBuilder::new("calc", "1.0.0")
    ///     .tool("add", "Additionne", |_args| Ok(serde_json::json!({})))
    ///     .bind_default()?;
    /// eprintln!("MCP sur http://{}", bound.local_addr()?);
    /// bound.serve()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn bind_default(self) -> std::io::Result<crate::http::BoundServer> {
        self.validate_route()?;
        for port in crate::DEFAULT_PORT_RANGE {
            match std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)) {
                Ok(listener) => return self.finish(listener),
                // Port pris : le suivant de la tranche — le cas nominal du
                // deuxième serveur LiteMCP sur la même machine.
                Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {}
                Err(error) => return Err(error),
            }
        }
        // Tranche entière occupée : port éphémère (0). L'OS ne peut pas
        // donner un port de la tranche — elle est sous toutes les plages
        // éphémères par défaut et ses vingt ports sont liés ci-dessus.
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        self.finish(listener)
    }

    /// Validation de route partagée par [`Self::bind`] et
    /// [`Self::bind_default`] — voir [`Self::route`].
    fn validate_route(&self) -> std::io::Result<()> {
        if !self.route.starts_with('/')
            || self.route.contains(['?', '#'])
            || self
                .route
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || !c.is_ascii())
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "route invalide {:?} : elle doit commencer par '/', être en ASCII visible sans espace et ne pas contenir '?' ni '#'",
                    self.route
                ),
            ));
        }
        if self.route == crate::http::HEALTH_ROUTE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "route invalide {:?} : elle masque la sonde de vie {}",
                    self.route,
                    crate::http::HEALTH_ROUTE
                ),
            ));
        }
        Ok(())
    }

    /// Construit le [`crate::http::BoundServer`] sur un listener **déjà
    /// lié** — corps commun de [`Self::bind`] et [`Self::bind_default`].
    fn finish(
        mut self,
        listener: std::net::TcpListener,
    ) -> std::io::Result<crate::http::BoundServer> {
        // Anti-zombie : le sweep du registre partagé précède notre propre
        // annonce — le dossier est validé avant lecture (rapport vide s'il
        // est hostile ou absent), donc propre au moment où nous y publions.
        let zombie = if self.anti_zombie {
            Some(Arc::new(crate::antizombie::ZombieState::new(
                crate::antizombie::sweep_registry(),
                self.drain_timeout,
            )))
        } else {
            None
        };
        // Identité d'instance : générée pour TOUT serveur lié (exposée par
        // `/health`), publiée dans l'annonce quand `.announce()` est actif.
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
        let instance_id = crate::registry::generate_instance_id(port);
        // TLS 1.3 (feature `tls`) : la configuration `rustls` est construite
        // une fois ici — une configuration refusée fait échouer le bind,
        // jamais un serveur qui démarrerait en clair par accident.
        #[cfg(feature = "tls")]
        let tls = match self.tls.as_ref() {
            Some(config) => Some(Arc::new(config.server_config().map_err(|error| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, error)
            })?)),
            None => None,
        };
        #[cfg(feature = "tls")]
        let certificate_fingerprint = self.tls.as_ref().map(crate::tls::TlsConfig::fingerprint);
        // Empreinte publiée dans l'annonce MCP Control (aucune sans TLS).
        #[cfg(feature = "tls")]
        let announced_fingerprint = self.tls.as_ref().map(|c| c.fingerprint().to_hex());
        #[cfg(not(feature = "tls"))]
        let announced_fingerprint: Option<String> = None;
        // Annonce MCP Control : le listener est lié — l'adresse effective
        // **connectable** est publiée (`addr`) avec l'identité (`instance`) :
        // le sweep juge l'adresse publiée (un bind multi-interface est
        // publié par son bouclage, un bind spécifique tel quel), jamais un
        // bouclage figé, et une identité vérifiable prouve quelle instance
        // occupe le port. Un échec d'écriture est silencieux (commodité
        // d'intégration, jamais une condition).
        let mut announce_name = None;
        if self.announce {
            if let Some(mut addr) = listener.local_addr().ok().filter(|a| a.port() != 0) {
                addr.set_ip(match addr.ip() {
                    std::net::IpAddr::V4(ip) if ip.is_unspecified() => {
                        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
                    }
                    std::net::IpAddr::V6(ip) if ip.is_unspecified() => {
                        std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
                    }
                    other => other,
                });
                crate::registry::announce_at_with(
                    &self.name,
                    &self.version,
                    addr,
                    instance_id,
                    self.verifier.is_some(),
                    announced_fingerprint.as_deref(),
                );
                announce_name = Some((self.name.clone(), addr.port()));
            }
        }
        // `mem::take` plutôt qu'un move direct : `merge_pending` emprunte
        // `self` (annotations en attente) — un move partiel de `self.tools`
        // rendrait ce prêt illégal. Le builder n'est plus utilisé ensuite.
        let mut tools = std::mem::take(&mut self.tools);
        // Fusionne les annotations et schémas de sortie en attente (outil
        // annoté avant d'être enregistré, ou réenregistré plus tard).
        self.merge_pending(&mut tools);
        // Droits déclarés : appliqués aux trois autres catalogues (les outils
        // passent par `merge_pending`).
        let mut resources = std::mem::take(&mut self.resources);
        apply_rights(&self.resource_rights, resources.iter_mut(), |def| {
            &mut def.required_rights
        });
        let mut prompts = std::mem::take(&mut self.prompts);
        apply_rights(&self.prompt_rights, prompts.iter_mut(), |def| {
            &mut def.required_rights
        });
        let mut templates = self.templates();
        apply_rights(&self.resource_rights, templates.iter_mut(), |def| {
            &mut def.required_rights
        });
        // B3 (audit 2026-11-25) : la liste `Access-Control-Allow-Headers`
        // du préflight CORS est calculée **une fois ici** (chemin froid du
        // bind) — la liste figée d'avant bloquait tout client navigateur qui
        // envoyait un en-tête légitime non annoncé : signature RFC 9421
        // (`Content-Digest`, `Signature-Input`, `Signature`) dès qu'un
        // vérificateur est configuré, jeton d'un hook utilisateur
        // (`Authorization`), paramètre promu en en-tête (`Mcp-Param-*`,
        // SEP-2243). Ordre annoncé : base, signature, paramètres promus
        // (catalogue parcouru dans l'ordre du `BTreeMap`, donc stable), puis
        // les déclarations utilisateur — dédupliqués sans tenir compte de la
        // casse par `joindre_en_tetes_preflight`.
        const EN_TETES_BASE: [&str; 5] = [
            "Content-Type",
            "Accept",
            "MCP-Protocol-Version",
            "Mcp-Method",
            "Mcp-Name",
        ];
        const EN_TETES_SIGNATURE: [&str; 3] = ["Content-Digest", "Signature-Input", "Signature"];
        let mut candidats: Vec<String> =
            Vec::with_capacity(EN_TETES_BASE.len() + EN_TETES_SIGNATURE.len() + tools.len());
        candidats.extend(EN_TETES_BASE.iter().map(|nom| (*nom).to_string()));
        if self.verifier.is_some() {
            candidats.extend(EN_TETES_SIGNATURE.iter().map(|nom| (*nom).to_string()));
        }
        for outil in tools.values() {
            for parametre in &outil.mcp_params {
                let suffixe = &parametre.header_suffix;
                candidats.push(format!("Mcp-Param-{suffixe}"));
            }
        }
        candidats.extend(self.allow_preflight_headers.iter().cloned());
        let allow_preflight_headers = joindre_en_tetes_preflight(candidats);
        let auth_gate = self.auth_gate();
        let meta = json!({
            protocol::META_SERVER_INFO: ServerInfo {
                name: self.name.clone(),
                version: self.version.clone(),
            }
        });
        // Compteur hissé AVANT les déplacements partiels de `self` ci-dessous.
        let inflight_handlers = self.inflight_counter();
        let handler_pool = self.handler_pool();
        // Même raison pour la clé `requestState` : tirée ici si l'appelant n'a
        // rien fourni (point 3.5 — jamais dans `new()`).
        let request_state_key = Arc::clone(
            self.request_state_key
                .get_or_init(|| Arc::new(harvest_key())),
        );
        let core = Arc::new(ServerCore {
            name: self.name,
            version: self.version,
            meta,
            tools: Arc::new(tools),
            resources: Arc::new(resources),
            templates: Arc::new(templates),
            completions: self.completions,
            prompts: Arc::new(prompts),
            tool_timeout: self.tool_timeout,
            max_inflight_handlers: MAX_INFLIGHT_HANDLERS,
            inflight_handlers,
            handler_pool,
            max_response_bytes: self.max_response_bytes,
            hooks: Arc::new(self.hooks),
            auth_gate,
            protocol_profile: self.protocol_profile,
            request_state_key,
            request_state_ttl: self.request_state_ttl,
            tools_list_cache: std::sync::OnceLock::new(),
            resources_list_cache: std::sync::OnceLock::new(),
            templates_list_cache: std::sync::OnceLock::new(),
            prompts_list_cache: std::sync::OnceLock::new(),
            discover_cache: std::sync::OnceLock::new(),
            initialize_cache: std::sync::OnceLock::new(),
            ping_cache: std::sync::OnceLock::new(),
        });
        Ok(crate::http::BoundServer {
            listener,
            core,
            route: Arc::new(self.route),
            allow_origins: Arc::new(self.allow_origins),
            allow_preflight_headers,
            allowed_hosts: Arc::new(self.allowed_hosts),
            request_hooks: Arc::new(self.request_hooks),
            read_timeout: self.read_timeout,
            write_deadline: self.write_deadline,
            max_connections: self.max_connections,
            connection_ttl: self.connection_ttl,
            live_connections: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            refusals_in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            announce_name,
            instance_id,
            zombie,
            #[cfg(feature = "tls")]
            tls,
            #[cfg(feature = "tls")]
            certificate_fingerprint,
        })
    }
}

#[cfg(test)]
mod tests_l3_tool_timeout {
    use super::*;

    /// Le défaut est **non nul** (30 s) et `.no_tool_timeout()` le désactive —
    /// c'est ce qui active `MAX_INFLIGHT_HANDLERS` sans configuration.
    #[test]
    fn le_defaut_est_de_30_s_et_no_tool_timeout_le_desactive() {
        assert_eq!(DEFAULT_TOOL_TIMEOUT, Duration::from_secs(30));

        let defaut = ServerBuilder::new("defaut", "1.0.0").core();
        assert_eq!(
            defaut.tool_timeout,
            Some(DEFAULT_TOOL_TIMEOUT),
            "un builder neuf doit être borné par défaut"
        );

        let illimite = ServerBuilder::new("illimite", "1.0.0")
            .no_tool_timeout()
            .core();
        assert_eq!(illimite.tool_timeout, None, "désactivation explicite");

        let court = ServerBuilder::new("court", "1.0.0")
            .tool_timeout(Duration::from_millis(5))
            .core();
        assert_eq!(court.tool_timeout, Some(Duration::from_millis(5)));
    }

    /// Le mécanisme du défaut borne réellement un handler bloqué : même
    /// chemin borné, délai court (le défaut vaut 30 s — inutile d'attendre).
    #[test]
    fn le_chemin_du_defaut_borne_un_handler_bloque() {
        let core = ServerBuilder::new("bloque", "1.0.0")
            .tool_timeout(Duration::from_millis(50))
            .tool("boucle", "bloque", |_args| {
                std::thread::sleep(Duration::from_secs(5));
                Ok(json!({}))
            })
            .core();
        let started = Instant::now();
        let reply = core
            .dispatch_unchecked(
                br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"boucle","arguments":{}}}"#,
            )
            .expect("tools/call doit répondre");
        let v: Value = serde_json::from_slice(&reply).unwrap();
        assert_eq!(v["result"]["isError"], true, "{v}");
        assert_eq!(
            v["result"]["structuredContent"]["error"]["code"], "timeout",
            "{v}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "le handler doit être borné, pas attendu"
        );
    }
}

#[cfg(test)]
mod tests_cle_request_state_paresseuse {
    use super::*;

    /// Point 3.5 de la revue : la clé HMAC des `requestState` est tirée au
    /// **premier noyau** ([`ServerBuilder::core`]), plus à `new()` — et jamais
    /// quand l'appelant fournit la sienne.
    #[test]
    fn la_cle_est_tiree_a_core_et_jamais_a_new() {
        let avant = crate::request_state::draws();
        let builder = ServerBuilder::new("paresse", "1.0.0");
        assert_eq!(
            crate::request_state::draws(),
            avant,
            "ServerBuilder::new ne doit pas tirer la clé HMAC"
        );

        let _core = builder.core();
        assert_eq!(
            crate::request_state::draws(),
            avant + 1,
            "core() tire la clé une seule fois"
        );

        let avant = crate::request_state::draws();
        let builder = ServerBuilder::new("paresse", "1.0.0")
            .request_state_key(b"cle explicite de trente-deux octets");
        let _core = builder.core();
        assert_eq!(
            crate::request_state::draws(),
            avant,
            "une clé explicite ne consomme aucune entropie"
        );
    }

    /// Le tirage paresseux ne casse pas le partage : un clone (snapshot de
    /// gateway) et les appels successifs à `core()` réutilisent la clé déjà
    /// tirée — sinon un jeton émis par un snapshot ne se vérifierait plus dans
    /// le suivant.
    #[test]
    fn clones_et_appels_partagent_la_cle_tiree() {
        let builder = ServerBuilder::new("paresse", "1.0.0");
        let clone = builder.clone();
        let a = builder.core();
        let b = clone.core();
        assert_eq!(
            &*a.request_state_key, &*b.request_state_key,
            "un clone doit partager la clé tirée paresseusement"
        );
        let c = builder.core();
        assert_eq!(
            &*a.request_state_key, &*c.request_state_key,
            "deux core() du même builder partagent la clé"
        );
    }
}

#[cfg(test)]
mod tests_b3_preflight {
    use super::*;

    /// B3 : déduplication insensible à la casse (première graphie gagnante),
    /// ordre d'insertion stable, doublons éliminés entre la base, les
    /// paramètres promus et les déclarations utilisateur.
    #[test]
    fn la_jointure_dedoublonne_sans_tenir_compte_de_la_casse_et_garde_l_ordre() {
        let liste = joindre_en_tetes_preflight(
            [
                "Content-Type",
                "Accept",
                "Content-Type",
                "Mcp-Param-count",
                "content-type",
                "Authorization",
                "authorization",
            ]
            .iter()
            .map(|nom| (*nom).to_string()),
        );
        assert_eq!(
            liste, "Content-Type, Accept, Mcp-Param-count, Authorization",
            "chaque en-tête une seule fois, ordre d'insertion conservé"
        );
    }

    /// Cas limites : liste vide et candidat unique.
    #[test]
    fn la_jointure_est_minimale() {
        assert_eq!(joindre_en_tetes_preflight(std::iter::empty::<String>()), "");
        assert_eq!(
            joindre_en_tetes_preflight(["Authorization"].iter().map(|nom| (*nom).to_string())),
            "Authorization"
        );
    }
}
