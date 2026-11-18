//! Exécution des handlers : outils, ressources, prompts, complétion et
//! cycles MRTR.

use super::authz::{forbidden_error, rights_held};
use super::pool::{HandlerDepthGuard, Outcome, in_handler, run_with_timeout};
use super::*;

/// `data` d'un échec de handler ressource / prompt / complétion : la cause
/// (`handler_failed`) puis la **classification** de l'erreur — mêmes champs
/// machine-lisibles que le `structuredContent.error` des outils (`code`,
/// `retryable`, `retryAfterMs` quand conseillé).
fn handler_failure_data(error: &ToolError) -> Value {
    let mut data = json!({
        "reason": "handler_failed",
        "code": error.code.as_str(),
        "retryable": error.retryable,
    });
    if let Some(ms) = error.retry_after_ms {
        data["retryAfterMs"] = json!(ms);
    }
    data
}

/// Neutralise les caractères de contrôle (ANSI, NUL, CR/LF) et tronque à
/// [`MAX_ECHO_CHARS`] tout identifiant issu du client avant de l'inclure
/// dans un message d'erreur.
pub(super) fn sanitize_echo(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(MAX_ECHO_CHARS + 4));
    for (idx, ch) in raw.chars().enumerate() {
        if idx >= MAX_ECHO_CHARS {
            out.push('…');
            break;
        }
        if ch.is_control() {
            out.push('\u{FFFD}');
        } else {
            out.push(ch);
        }
    }
    out
}

/// Verdict d'une **erreur de schéma interne** (validateur exécuteur
/// indisponible) : `-32603` avec un message neutre — jamais `-32602` (ce
/// n'est pas la faute du client), jamais le détail interne (URL de schéma,
/// nom de feature, message de bibliothèque) dans la réponse.
fn internal_schema_failure(id: &Value, cap: usize) -> (BoundedReply, CallStatus) {
    (
        Outgoing::failure(
            id.clone(),
            ErrorObject::new(ErrorObject::INTERNAL_ERROR, "internal error"),
        )
        .to_bounded(cap),
        CallStatus::ProtocolError(ErrorObject::INTERNAL_ERROR),
    )
}

/// Résultat d'échec d'exécution (`isError: true`) d'un outil, façonné selon
/// l'`outputSchema` publié — audit de conformance `2026-07-28`, MINEUR 3.
///
/// Page *Tools* : « If an output schema is provided: Servers **MUST**
/// provide structured results that conform to this schema ». La forme fil
/// de [`CallToolResult::from_tool_error`] (`structuredContent: {"error": …}`)
/// ne conforme pas à un schéma publié : quand l'outil en publie un, le champ
/// est **omis** — le détail de l'erreur reste dans `content` (message
/// texte). Sans `outputSchema` publié, la forme historique `{"error": …}`
/// est conservée : aucun contrat de sortie n'existe à violer.
fn tool_error_result(error: ToolError, output_schema: Option<&Arc<dyn Schema>>) -> CallToolResult {
    let mut result = CallToolResult::from(Err::<Value, ToolError>(error));
    if output_schema.is_some() {
        result.structured_content = None;
    }
    result
}

/// Dérive la liste `PromptArgument` de `prompts/list` à partir du JSON Schema
/// d'un prompt typé (`prompt_typed` / `#[prompt]`).
pub(super) fn derive_prompt_arguments(
    schema_json: &Value,
) -> Option<Vec<crate::protocol::PromptArgument>> {
    let properties = schema_json.get("properties")?.as_object()?;
    let required: Vec<&str> = schema_json
        .get("required")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let arguments = properties
        .iter()
        .map(|(name, prop)| crate::protocol::PromptArgument {
            name: name.clone(),
            description: prop
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            required: Some(required.contains(&name.as_str())),
        })
        .collect();
    Some(arguments)
}

/// Paramètres saillants liés au `requestState` MRTR — forme **canonique**
/// (indépendante de la présence/absence des champs optionnels) pour que le
/// digest du rejeu soit calculé sur exactement les mêmes octets qu'à
/// l'émission. `_meta` et les champs superflus n'y participent pas : les
/// capacités déclarées peuvent évoluer entre les tours, le contrat lie la
/// requête métier (méthode, nom/uri, arguments), pas son habillage.
fn salient_params(method: &str, params: &Value) -> Value {
    match method {
        protocol::method::TOOLS_CALL | protocol::method::PROMPTS_GET => json!({
            "method": method,
            "name": params.get("name").cloned().unwrap_or(Value::Null),
            "arguments": params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({})),
        }),
        protocol::method::RESOURCES_READ => json!({
            "method": method,
            "uri": params.get("uri").cloned().unwrap_or(Value::Null),
        }),
        _ => json!({ "method": method }),
    }
}

/// Validation de forme d'une requête embarquée produite par un handler —
/// l'émission d'une requête mal formée est un bogue serveur (`-32603`) :
/// elle ne doit jamais atteindre le fil (le client planterait dessus).
/// L'élicitation vérifie `message` (chaîne), `mode` (`"form"`/`"url"`,
/// défaut `"form"`), `requestedSchema` (objet, mode form) et `url`
/// (chaîne non vide, mode url) ; l'échantillonnage et les racines ne
/// vérifient que la forme objet (leurs champs applicatifs restent du
/// ressort du handler et du client).
fn validate_embedded_request(request: &Value) -> Result<(), String> {
    let Some(request) = request.as_object() else {
        return Err("must be an object".to_string());
    };
    let Some(method) = request.get("method").and_then(Value::as_str) else {
        return Err("`method` must be a string".to_string());
    };
    let Some(params) = request.get("params").and_then(Value::as_object) else {
        return Err("`params` must be an object".to_string());
    };
    match method {
        protocol::embedded_request::ELICIT_CREATE => {
            let mode = match params.get("mode") {
                None => "form",
                Some(Value::String(mode)) if mode == "form" => "form",
                Some(Value::String(mode)) if mode == "url" => "url",
                Some(_) => {
                    return Err("elicitation `mode` must be \"form\" or \"url\"".to_string());
                }
            };
            if !params.get("message").is_some_and(Value::is_string) {
                return Err("elicitation `message` must be a string".to_string());
            }
            match mode {
                "form" if !params.get("requestedSchema").is_some_and(Value::is_object) => {
                    Err("form elicitation requires an object `requestedSchema`".to_string())
                }
                "url"
                    if !params
                        .get("url")
                        .and_then(Value::as_str)
                        .is_some_and(|url| !url.is_empty()) =>
                {
                    Err("url elicitation requires a non-empty string `url`".to_string())
                }
                _ => Ok(()),
            }
        }
        protocol::embedded_request::SAMPLING_CREATE_MESSAGE
        | protocol::embedded_request::ROOTS_LIST => Ok(()),
        other => Err(format!("`{other}` is not an embeddable MCP method")),
    }
}

/// Vérifie si l'objet `clientCapabilities` déclare la capacité demandée.
///
/// Accepte les clés directes (`"sampling"`) ou les chemins pointés (`"experimental.feature"`).
fn has_client_capability(client_caps: &Value, cap: &str) -> bool {
    let Some(obj) = client_caps.as_object() else {
        return false;
    };
    if let Some(val) = obj.get(cap) {
        return !val.is_null() && val != false;
    }
    let mut current = client_caps;
    for part in cap.split('.') {
        if let Some(next) = current.get(part) {
            current = next;
        } else {
            return false;
        }
    }
    !current.is_null() && current != false
}

impl ServerCore {
    /// Vérifie le cycle MRTR d'une requête (`requestState` +
    /// `inputResponses`) **avant tout handler** — uniquement sur les trois
    /// méthodes supportées par la spec (`tools/call`, `prompts/get`,
    /// `resources/read`) ; toute autre méthode ignore ces champs (le noyau
    /// ne répond jamais `input_required` ailleurs et ne dépend d'aucun
    /// état de connexion).
    ///
    /// * Absence des deux champs → tour 1 ([`InputRound::first`]).
    /// * `inputResponses` sans `requestState` → `-32602` (rien n'est
    ///   vérifiable : refus net, le handler n'est pas atteint).
    /// * `requestState` présent → jeton vérifié (taille, structure
    ///   base64url, HMAC-SHA256, version, méthode liée, digest des
    ///   paramètres saillants, tour, échéance) ou refus `-32602` — un état
    ///   altéré, expiré, émis sous une autre clé ou lié à une autre requête
    ///   n'atteint jamais le handler.
    /// * `inputResponses` présent → structure validée (objet de
    ///   valeurs-objets ; les clés non reconnues sont ignorées, comme le
    ///   permet la spec), sinon `-32602`.
    ///
    /// Le jeton est **porteur** et **non consommé** : la consommation unique
    /// doit être imposée par le handler (aucun état serveur ici). L'`id`
    /// JSON-RPC est volontairement **non lié** (`_id` inutilisé) : la spec
    /// exige que le rejeu utilise un id différent.
    pub(super) fn verify_mrtr(
        &self,
        _id: &Value,
        method: &str,
        params: &Value,
    ) -> Result<InputRound, ErrorObject> {
        let has_state = params.get(protocol::mrtr::REQUEST_STATE).is_some();
        let has_responses = params.get(protocol::mrtr::INPUT_RESPONSES).is_some();
        if !has_state && !has_responses {
            return Ok(InputRound::first());
        }
        if !has_state {
            return Err(ErrorObject::new(
                ErrorObject::INVALID_PARAMS,
                "params.requestState is required when params.inputResponses is present",
            ));
        }
        let Some(token) = params
            .get(protocol::mrtr::REQUEST_STATE)
            .and_then(Value::as_str)
        else {
            return Err(ErrorObject::new(
                ErrorObject::INVALID_PARAMS,
                "params.requestState must be a string",
            ));
        };
        if token.len() > MAX_REQUEST_STATE_CHARS {
            return Err(ErrorObject::new(
                ErrorObject::INVALID_PARAMS,
                format!(
                    "params.requestState exceeds the maximum size ({MAX_REQUEST_STATE_CHARS} characters)"
                ),
            ));
        }
        let payload = verify_token(&self.request_state_key, token)
            .map_err(|error| ErrorObject::new(ErrorObject::INVALID_PARAMS, error.message()))?;
        let Some(payload_obj) = payload.as_object() else {
            return Err(ErrorObject::new(
                ErrorObject::INVALID_PARAMS,
                "requestState payload is malformed",
            ));
        };
        let invalid = |detail: &'static str| ErrorObject::new(ErrorObject::INVALID_PARAMS, detail);
        if payload_obj.get("v").and_then(Value::as_u64) != Some(1) {
            return Err(invalid("unsupported requestState payload version"));
        }
        if payload_obj.get("method").and_then(Value::as_str) != Some(method) {
            return Err(invalid(
                "requestState is bound to another method — replay it on the original method",
            ));
        }
        let Some(bound_digest) = payload_obj.get("digest").and_then(Value::as_str) else {
            return Err(invalid(
                "requestState payload is missing the request digest",
            ));
        };
        if bound_digest != digest_salient(&salient_params(method, params)) {
            return Err(invalid(
                "requestState is bound to the original request — replay it with unchanged parameters",
            ));
        }
        let turn = match payload_obj.get("turn").and_then(Value::as_u64) {
            Some(turn) if (1..=u64::from(u32::MAX)).contains(&turn) => turn as u32,
            _ => return Err(invalid("requestState payload turn is invalid")),
        };
        let Some(exp) = payload_obj.get("exp").and_then(Value::as_u64) else {
            return Err(invalid("requestState payload expiry is invalid"));
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if now >= exp {
            return Err(invalid("requestState has expired (short TTL)"));
        }
        let input_responses = match params.get(protocol::mrtr::INPUT_RESPONSES) {
            None => None,
            Some(responses) => {
                let Some(map) = responses.as_object() else {
                    return Err(invalid("params.inputResponses must be an object"));
                };
                for (key, value) in map {
                    if !value.is_object() {
                        return Err(ErrorObject::new(
                            ErrorObject::INVALID_PARAMS,
                            format!(
                                "params.inputResponses[\"{}\"] must be an object",
                                sanitize_echo(key)
                            ),
                        ));
                    }
                }
                Some(responses.clone())
            }
        };
        Ok(InputRound {
            turn: turn.saturating_add(1),
            input_responses,
            state: payload_obj.get("state").cloned(),
        })
    }

    /// Émet un résultat `input_required` (MRTR) produit par un handler
    /// d'outil, de prompt ou de ressource.
    ///
    /// Valide la spécification (au moins une requête embarquée ou un état —
    /// sinon `-32603` : bogue de handler, rien ne part sur le fil), chaque
    /// requête embarquée (méthode embarquable + forme fil, sinon `-32603`)
    /// et les capacités client déclarées dans `_meta` de la requête
    /// courante (`elicitation/create` → `elicitation`,
    /// `sampling/createMessage` → `sampling`, `roots/list` → `roots` ;
    /// capacité absente → `-32021` avec la forme officielle
    /// `data.requiredCapabilities`). Frappe ensuite un `requestState`
    /// opaque signé (HMAC-SHA256, TTL, liaison méthode/paramètres
    /// saillants/tour) portant l'état du handler — **aucun état de
    /// connexion** n'est retenu : le rejeu peut atterrir sur toute instance
    /// partageant la clé.
    ///
    /// Forme fil : `{ resultType: "input_required", inputRequests?,
    /// requestState, _meta }` en HTTP 200.
    fn emit_input_required(
        &self,
        id: &Value,
        method: &str,
        params: &Value,
        spec: InputRequiredSpec,
        round: &InputRound,
    ) -> (BoundedReply, CallStatus) {
        let cap = self.max_response_bytes;
        let internal = |message: String| {
            (
                Outgoing::failure(
                    id.clone(),
                    ErrorObject::new(ErrorObject::INTERNAL_ERROR, message),
                )
                .to_bounded(cap),
                CallStatus::ProtocolError(ErrorObject::INTERNAL_ERROR),
            )
        };
        // 1. Une spécification vide est un bogue de handler (spec : au
        //    moins un des deux champs dans chaque résultat).
        if spec.input_requests.is_empty() && spec.state.is_none() {
            return internal(
                "handler produced an input_required result with neither inputRequests nor state"
                    .to_string(),
            );
        }
        // 2. Requêtes embarquées : forme fil + méthodes embarquables —
        //    une requête mal formée ne doit jamais atteindre le client.
        for (key, request) in &spec.input_requests {
            if key.is_empty() {
                return internal(
                    "handler produced an input_required result with an empty request key"
                        .to_string(),
                );
            }
            if let Err(detail) = validate_embedded_request(request) {
                return internal(format!(
                    "handler produced an invalid embedded request `{key}`: {detail}"
                ));
            }
        }
        // 3. Capacités client déclarées — la spec interdit d'embarquer une
        //    requête que le client n'a pas annoncée dans ses capacités.
        let client_caps = params
            .get("_meta")
            .and_then(|meta| meta.get(protocol::META_CLIENT_CAPABILITIES));
        let mut missing: BTreeMap<&str, Value> = BTreeMap::new();
        for request in spec.input_requests.values() {
            let embedded_method = request
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if let Some(cap) = protocol::embedded_request::client_capability(embedded_method) {
                if !client_caps.is_some_and(|caps| has_client_capability(caps, cap)) {
                    missing.insert(cap, json!({}));
                }
            }
        }
        if !missing.is_empty() {
            let list = missing.keys().copied().collect::<Vec<_>>().join(", ");
            return (
                Outgoing::failure(
                    id.clone(),
                    ErrorObject::with_data(
                        ErrorObject::CLIENT_CAPABILITY_REQUIRED,
                        format!("missing required client capabilities: {list}"),
                        json!({ "requiredCapabilities": missing }),
                    ),
                )
                .to_bounded(cap),
                CallStatus::ProtocolError(ErrorObject::CLIENT_CAPABILITY_REQUIRED),
            );
        }
        // 4. Borne de taille de l'état opaque — un handler qui la dépasse
        //    est un bogue serveur (`-32603`), jamais une troncature.
        if let Some(state) = &spec.state {
            let size =
                serde_json::to_vec(state).map_or(MAX_STATE_PAYLOAD_BYTES + 1, |bytes| bytes.len());
            if size > MAX_STATE_PAYLOAD_BYTES {
                return internal(format!(
                    "handler state ({size} bytes) exceeds the maximum ({MAX_STATE_PAYLOAD_BYTES} bytes)"
                ));
            }
        }
        // 5. Jeton signé — le plus petit état stateless compatible : le
        //    jeton porte tout ce qu'il faut pour reprendre, rien ne vit en
        //    dehors de lui (ni session, ni connexion, ni process).
        let digest = digest_salient(&salient_params(method, params));
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let payload = MrtrStatePayload {
            v: 1,
            method,
            digest: &digest,
            turn: round.turn,
            exp: now.saturating_add(self.request_state_ttl.as_secs()),
            state: spec.state.as_ref(),
        };
        let token = mint_token(
            &self.request_state_key,
            &serde_json::to_value(&payload).expect("payload serialization is infallible"),
        );
        (
            Outgoing::success_bounded(
                id,
                &InputRequiredResultRef {
                    result_type: ResultType::InputRequired,
                    input_requests: if spec.input_requests.is_empty() {
                        None
                    } else {
                        Some(&spec.input_requests)
                    },
                    request_state: &token,
                    meta: &self.meta,
                },
                cap,
            ),
            CallStatus::Success,
        )
    }

    /// Exécute un handler de ressource, borné par `tool_timeout` — le tour
    /// MRTR ([`InputRound`]) est remis au handler à chaque passage.
    pub(super) fn read_resource(
        &self,
        id: &Value,
        params: &Value,
        round: &InputRound,
    ) -> (BoundedReply, CallStatus) {
        let cap = self.max_response_bytes;
        let Some(uri) = params.get("uri").and_then(Value::as_str) else {
            return (
                Outgoing::failure(
                    id.clone(),
                    ErrorObject::new(ErrorObject::INVALID_PARAMS, "missing params.uri"),
                )
                .to_bounded(cap),
                CallStatus::ProtocolError(ErrorObject::INVALID_PARAMS),
            );
        };
        let Some(def) = self.resources.get(uri) else {
            return (
                Outgoing::failure(
                    id.clone(),
                    ErrorObject::new(
                        ErrorObject::INVALID_PARAMS,
                        format!("Unknown resource: {}", sanitize_echo(uri)),
                    ),
                )
                .to_bounded(cap),
                CallStatus::ProtocolError(ErrorObject::INVALID_PARAMS),
            );
        };
        let handler = def.handler.clone();
        let owned_round = round.clone();
        let contents = match self.run_bounded(move || match &handler {
            ResourceHandler::Plain(f) => f().map(ResourceOutcome::Data),
            ResourceHandler::Mrtr(f) => f(&owned_round),
        }) {
            Ok(contents) => contents,
            Err(failure) => {
                let code = failure.code;
                return (
                    Outgoing::failure(id.clone(), failure).to_bounded(cap),
                    CallStatus::ProtocolError(code),
                );
            }
        };
        match contents {
            Ok(ResourceOutcome::InputRequired(spec)) => {
                // MRTR sur `resources/read` (méthode supportée par la spec).
                self.emit_input_required(id, protocol::method::RESOURCES_READ, params, spec, round)
            }
            Ok(ResourceOutcome::Data(contents)) => (
                Outgoing::success_bounded(
                    id,
                    &ReadResourceResultRef {
                        result_type: ResultType::Complete,
                        ttl_ms: DEFAULT_LIST_TTL_MS,
                        cache_scope: CacheScope::Private,
                        contents: std::slice::from_ref(&contents),
                        meta: &self.meta,
                    },
                    cap,
                ),
                CallStatus::Success,
            ),
            Err(message) => (
                Outgoing::failure(
                    id.clone(),
                    ErrorObject::with_data(
                        HANDLER_FAILURE,
                        format!("Resource read failed: {message}"),
                        handler_failure_data(&message),
                    ),
                )
                .to_bounded(cap),
                CallStatus::ProtocolError(HANDLER_FAILURE),
            ),
        }
    }

    /// Exécute un handler de prompt, borné par `tool_timeout` — le tour
    /// MRTR ([`InputRound`]) est remis au handler à chaque passage.
    pub(super) fn get_prompt(
        &self,
        id: &Value,
        params: &Value,
        round: &InputRound,
    ) -> (BoundedReply, CallStatus) {
        let cap = self.max_response_bytes;
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return (
                Outgoing::failure(
                    id.clone(),
                    ErrorObject::new(ErrorObject::INVALID_PARAMS, "missing params.name"),
                )
                .to_bounded(cap),
                CallStatus::ProtocolError(ErrorObject::INVALID_PARAMS),
            );
        };
        let Some(def) = self.prompts.get(name) else {
            return (
                Outgoing::failure(
                    id.clone(),
                    ErrorObject::new(
                        ErrorObject::INVALID_PARAMS,
                        format!("Unknown prompt: {}", sanitize_echo(name)),
                    ),
                )
                .to_bounded(cap),
                CallStatus::ProtocolError(ErrorObject::INVALID_PARAMS),
            );
        };
        // `arguments` : objet ou absent — même contrat que `tools/call`.
        let arguments = match params.get("arguments") {
            None => empty_object(),
            Some(value) if value.is_object() => value,
            Some(_) => {
                return (
                    Outgoing::failure(
                        id.clone(),
                        ErrorObject::new(
                            ErrorObject::INVALID_PARAMS,
                            "params.arguments must be an object",
                        ),
                    )
                    .to_bounded(cap),
                    CallStatus::ProtocolError(ErrorObject::INVALID_PARAMS),
                );
            }
        };
        // Injection des valeurs par défaut (même contrat que `tools/call`).
        let mut prepared: Option<Value> = match &def.schema {
            Some(schema) if schema.has_defaults() || schema.has_integers() => {
                let mut value = arguments.clone();
                if schema.has_defaults() {
                    schema.apply_defaults(&mut value);
                }
                Some(value)
            }
            _ => None,
        };
        let arguments_for_validation = prepared.as_ref().unwrap_or(arguments);
        // Schéma exécuteur : hors schéma → -32602 avec les issues, le
        // handler ne tourne jamais (même contrat que `tool_typed`).
        if let Some(schema) = &def.schema {
            let mut issues = Issues::new();
            let mut path = Path::root("arguments");
            schema.validate(&mut path, arguments_for_validation, &mut issues);
            // Schéma interne indisponible : erreur serveur neutre.
            if issues.is_internal() {
                return internal_schema_failure(id, cap);
            }
            if !issues.is_empty() {
                let rendered = issues.render();
                return (
                    Outgoing::failure(
                        id.clone(),
                        ErrorObject::with_data(
                            ErrorObject::INVALID_PARAMS,
                            format!("Invalid arguments: {}", rendered.join(" ; ")),
                            json!({ "issues": rendered }),
                        ),
                    )
                    .to_bounded(cap),
                    CallStatus::ProtocolError(ErrorObject::INVALID_PARAMS),
                );
            }
        }
        // **Normalisation des entiers intégraux** (même contrat que
        // `tools/call`) : après validation réussie uniquement.
        if let (Some(schema), Some(value)) = (&def.schema, prepared.as_mut()) {
            if schema.has_integers() {
                schema.normalize_integers(value);
            }
        }
        let arguments = prepared.as_ref().unwrap_or(arguments);
        let handler = def.handler.clone();
        let arguments = arguments.clone();
        let owned_round = round.clone();
        let messages = match self.run_bounded(move || match &handler {
            PromptHandler::Plain(f) => f(&arguments).map(PromptOutcome::Messages),
            PromptHandler::Mrtr(f) => f(&arguments, &owned_round),
        }) {
            Ok(messages) => messages,
            Err(failure) => {
                let code = failure.code;
                return (
                    Outgoing::failure(id.clone(), failure).to_bounded(cap),
                    CallStatus::ProtocolError(code),
                );
            }
        };
        match messages {
            Ok(PromptOutcome::InputRequired(spec)) => {
                // MRTR sur `prompts/get` (méthode supportée par la spec).
                self.emit_input_required(id, protocol::method::PROMPTS_GET, params, spec, round)
            }
            Ok(PromptOutcome::Messages(messages)) => (
                Outgoing::success_bounded(
                    id,
                    &GetPromptResultRef {
                        result_type: ResultType::Complete,
                        description: def.description.as_deref(),
                        messages: &messages,
                        meta: &self.meta,
                    },
                    cap,
                ),
                CallStatus::Success,
            ),
            Err(message) => (
                Outgoing::failure(
                    id.clone(),
                    ErrorObject::with_data(
                        HANDLER_FAILURE,
                        format!("Prompt failed: {message}"),
                        handler_failure_data(&message),
                    ),
                )
                .to_bounded(cap),
                CallStatus::ProtocolError(HANDLER_FAILURE),
            ),
        }
    }

    /// Exécute le fournisseur de complétion, borné par `tool_timeout` —
    /// mêmes conventions que prompts/ressources : params mal formés ou
    /// fournisseur absent → `-32602`, échec du fournisseur → `-32603`
    /// (data `handler_failed`), timeout/saturation → erreur noyau `-32603`.
    ///
    /// **Autorisation** : `completion/complete` peut révéler les **noms
    /// d'arguments** d'un prompt ou d'une ressource (découvert au lot L1) ;
    /// la référence nommée est donc filtrée exactement comme les listes —
    /// élément sans droit déclaré ou droit manquant → `-32023` /
    /// `right_missing`, `403`, et le fournisseur n'est jamais appelé.
    pub(super) fn complete(&self, id: &Value, params: &Value) -> (BoundedReply, CallStatus) {
        let cap = self.max_response_bytes;
        let Some(provider) = &self.completions else {
            return (
                Outgoing::failure(
                    id.clone(),
                    ErrorObject::new(
                        ErrorObject::INVALID_PARAMS,
                        "completion is not supported: no completion provider is registered",
                    ),
                )
                .to_bounded(cap),
                CallStatus::ProtocolError(ErrorObject::INVALID_PARAMS),
            );
        };
        let reference = match params.get("ref") {
            Some(Value::Object(r)) => match r.get("type").and_then(Value::as_str) {
                Some("ref/prompt") => match r.get("name").and_then(Value::as_str) {
                    Some(name) => CompleteReference::Prompt {
                        name: name.to_string(),
                        title: r.get("title").and_then(Value::as_str).map(str::to_string),
                    },
                    None => {
                        return self.complete_invalid_params(
                            id,
                            "params.ref.name is required for ref/prompt",
                        );
                    }
                },
                Some("ref/resource") => match r.get("uri").and_then(Value::as_str) {
                    Some(uri) => CompleteReference::Resource {
                        uri: uri.to_string(),
                    },
                    None => {
                        return self.complete_invalid_params(
                            id,
                            "params.ref.uri is required for ref/resource",
                        );
                    }
                },
                Some(other) => {
                    return self.complete_invalid_params(
                        id,
                        format!(
                            "unknown params.ref.type: {} — expected `ref/prompt` or `ref/resource`",
                            sanitize_echo(other)
                        ),
                    );
                }
                None => {
                    return self.complete_invalid_params(id, "params.ref.type is required");
                }
            },
            Some(_) => {
                return self.complete_invalid_params(id, "params.ref must be an object");
            }
            None => {
                return self.complete_invalid_params(id, "missing params.ref");
            }
        };
        // Filtrage de la référence (voir le commentaire de la fonction) :
        // décidé avant toute lecture d'argument, donc avant le fournisseur.
        if self.auth_gate.is_some() {
            if let Some(required) = self.completion_rights(&reference) {
                let context = auth::current();
                let element = match &reference {
                    CompleteReference::Prompt { name, .. } => name.as_str(),
                    CompleteReference::Resource { uri } => uri.as_str(),
                };
                let refusal = match context.as_ref() {
                    None => Some(AuthRefusal {
                        verdict: auth::AuthVerdict::Unauthenticated,
                        reason: "unauthenticated",
                        wire_reason: "unauthenticated",
                        retry_after: None,
                    }),
                    Some(context) if !rights_held(context, &required) => Some(AuthRefusal {
                        verdict: auth::AuthVerdict::RightMissing,
                        reason: "right_missing",
                        wire_reason: "right_missing",
                        retry_after: None,
                    }),
                    Some(_) => None,
                };
                if let Some(refusal) = refusal {
                    self.emit_auth_decision(
                        protocol::method::COMPLETION_COMPLETE,
                        Some(element),
                        context.as_ref().map(AuthContext::client),
                        refusal.verdict,
                        Some(refusal.reason),
                    );
                    return (
                        Outgoing::failure(id.clone(), forbidden_error(&refusal)).to_bounded(cap),
                        CallStatus::ProtocolError(ErrorObject::FORBIDDEN),
                    );
                }
                self.emit_auth_decision(
                    protocol::method::COMPLETION_COMPLETE,
                    Some(element),
                    context.as_ref().map(AuthContext::client),
                    auth::AuthVerdict::Allowed,
                    None,
                );
            }
        }
        let argument = match params.get("argument") {
            Some(Value::Object(a)) => {
                match (
                    a.get("name").and_then(Value::as_str),
                    a.get("value").and_then(Value::as_str),
                ) {
                    (Some(name), Some(value)) => CompletionArgument {
                        name: name.to_string(),
                        value: value.to_string(),
                    },
                    (Some(_), None) => {
                        return self.complete_invalid_params(
                            id,
                            "params.argument.value (string) is required",
                        );
                    }
                    (None, _) => {
                        return self.complete_invalid_params(
                            id,
                            "params.argument.name (string) is required",
                        );
                    }
                }
            }
            Some(_) => {
                return self.complete_invalid_params(id, "params.argument must be an object");
            }
            None => {
                return self.complete_invalid_params(id, "missing params.argument");
            }
        };
        // `context.arguments` : carte optionnelle de chaînes (valeurs
        // non-chaînes refusées — la spec attend un map de strings).
        let mut context = BTreeMap::new();
        if let Some(map) = params
            .get("context")
            .and_then(|c| c.get("arguments"))
            .and_then(Value::as_object)
        {
            for (key, value) in map {
                match value.as_str() {
                    Some(text) => {
                        context.insert(key.clone(), text.to_string());
                    }
                    None => {
                        return self.complete_invalid_params(
                            id,
                            format!(
                                "params.context.arguments values must be strings (offending key: {})",
                                sanitize_echo(key)
                            ),
                        );
                    }
                }
            }
        }
        let request = CompletionRequest {
            reference,
            argument,
            context,
        };
        let handler = Arc::clone(provider);
        let completion = match self.run_bounded(move || handler(&request)) {
            Ok(completion) => completion,
            Err(failure) => {
                let code = failure.code;
                return (
                    Outgoing::failure(id.clone(), failure).to_bounded(cap),
                    CallStatus::ProtocolError(code),
                );
            }
        };
        let completion = match completion {
            Ok(completion) => completion,
            Err(message) => {
                return (
                    Outgoing::failure(
                        id.clone(),
                        ErrorObject::with_data(
                            HANDLER_FAILURE,
                            format!("Completion failed: {message}"),
                            handler_failure_data(&message),
                        ),
                    )
                    .to_bounded(cap),
                    CallStatus::ProtocolError(HANDLER_FAILURE),
                );
            }
        };
        // Garde-fou spec : au plus 100 valeurs. Au-delà, troncature et
        // `hasMore: true` (sauf si le fournisseur l'a déjà posé).
        let mut completion = completion;
        if completion.values.len() > MAX_COMPLETION_VALUES {
            completion.values.truncate(MAX_COMPLETION_VALUES);
            if completion.has_more.is_none() {
                completion.has_more = Some(true);
            }
        }
        (
            Outgoing::success_bounded(
                id,
                &CompleteResult {
                    result_type: ResultType::Complete,
                    completion,
                    meta: Some(self.meta.clone()),
                },
                cap,
            ),
            CallStatus::Success,
        )
    }

    /// Réponse `-32602` de `completion/complete` — même convention que
    /// outils/ressources/prompts (cible ou forme de params invalide).
    fn complete_invalid_params(
        &self,
        id: &Value,
        message: impl Into<String>,
    ) -> (BoundedReply, CallStatus) {
        (
            Outgoing::failure(
                id.clone(),
                ErrorObject::new(ErrorObject::INVALID_PARAMS, message),
            )
            .to_bounded(self.max_response_bytes),
            CallStatus::ProtocolError(ErrorObject::INVALID_PARAMS),
        )
    }

    /// Exécute `f` borné par [`Self::tool_timeout`] (défaut
    /// [`DEFAULT_TOOL_TIMEOUT`]) quand il est actif ; traduit timeout/panique
    /// en erreur JSON-RPC.
    fn run_bounded<T: Send + 'static>(
        &self,
        f: impl FnOnce() -> Result<T, ToolError> + Send + 'static,
    ) -> Result<Result<T, ToolError>, ErrorObject> {
        // Deux raisons de rester sur le fil courant : aucun délai configuré
        // ([`ServerBuilder::no_tool_timeout`]) ou handler déjà borné plus haut
        // dans la pile (appel imbriqué — il hérite de l'échéance extérieure).
        let Some(timeout) = self.tool_timeout.filter(|_| !in_handler()) else {
            let _depth = HandlerDepthGuard::enter();
            return match std::panic::catch_unwind(AssertUnwindSafe(f)) {
                Ok(result) => Ok(result),
                Err(_) => Err(ErrorObject::new(
                    ErrorObject::INTERNAL_ERROR,
                    "internal error (handler panicked)",
                )),
            };
        };
        // Le handler peut s'exécuter dans un autre fil : le canal de
        // progression, le jeton d'annulation, la profondeur de dispatch
        // gateway et l'identité de l'appelant du dispatch y sont réinstallés
        // explicitement (les vars de fil ne traversent pas `spawn`).
        let ambient = crate::progress::current();
        let ambient_cancel = crate::cancel::token();
        let ambient_depth = crate::gateway::dispatch_depth();
        let ambient_auth = auth::current();
        match run_with_timeout(
            &self.handler_pool,
            timeout,
            &self.inflight_handlers,
            self.max_inflight_handlers,
            move || {
                let _cancel = crate::cancel::bind(ambient_cancel);
                let _progress = crate::progress::bind(ambient);
                let _gateway = crate::gateway::bind_dispatch_depth(ambient_depth);
                let _auth = auth::bind(ambient_auth);
                let _depth = HandlerDepthGuard::enter();
                f()
            },
        ) {
            Outcome::Done(result) => Ok(result),
            Outcome::Panicked => Err(ErrorObject::new(
                ErrorObject::INTERNAL_ERROR,
                "internal error (handler panicked)",
            )),
            Outcome::TimedOut => Err(ErrorObject::with_data(
                HANDLER_FAILURE,
                format!("execution timed out after {} ms", timeout.as_millis()),
                json!({ "reason": "timeout", "timeout_ms": timeout.as_millis() as u64 }),
            )),
            Outcome::Overloaded => Err(ErrorObject::with_data(
                HANDLER_FAILURE,
                "too many concurrent handlers in flight",
                json!({ "reason": "overloaded", "max_in_flight": self.max_inflight_handlers }),
            )),
        }
    }

    pub(super) fn call_tool(
        &self,
        id: &Value,
        params: &Value,
        round: &InputRound,
    ) -> (BoundedReply, CallStatus) {
        // `response_cap` (et non `cap`) : la boucle des capacités requises
        // plus bas itère déjà sous le nom `cap`.
        let response_cap = self.max_response_bytes;
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return (
                Outgoing::failure(
                    id.clone(),
                    ErrorObject::new(ErrorObject::INVALID_PARAMS, "missing params.name"),
                )
                .to_bounded(response_cap),
                CallStatus::ProtocolError(ErrorObject::INVALID_PARAMS),
            );
        };
        let Some(tool) = self.tools.get(name) else {
            return (
                Outgoing::failure(
                    id.clone(),
                    ErrorObject::new(
                        ErrorObject::INVALID_PARAMS,
                        format!("Unknown tool: {}", sanitize_echo(name)),
                    ),
                )
                .to_bounded(response_cap),
                CallStatus::ProtocolError(ErrorObject::INVALID_PARAMS),
            );
        };
        if !tool.required_capabilities.is_empty() {
            let client_caps = params
                .get("_meta")
                .and_then(|m| m.get(protocol::META_CLIENT_CAPABILITIES));
            for cap in &tool.required_capabilities {
                let satisfied = client_caps.is_some_and(|caps| has_client_capability(caps, cap));
                if !satisfied {
                    // SEP-2575 : `MissingRequiredClientCapabilityError` porte
                    // `error.data.requiredCapabilities` en **objet** indexé par
                    // la capacité manquante (ex. `{ "sampling": {} }`) — pas une
                    // liste de noms (le scénario officiel `server-stateless`
                    // valide cette forme exacte).
                    let mut required = serde_json::Map::new();
                    required.insert(cap.clone(), json!({}));
                    return (
                        Outgoing::failure(
                            id.clone(),
                            ErrorObject::with_data(
                                ErrorObject::CLIENT_CAPABILITY_REQUIRED,
                                format!("missing required client capability: {cap}"),
                                json!({ "requiredCapabilities": required }),
                            ),
                        )
                        .to_bounded(response_cap),
                        CallStatus::ProtocolError(ErrorObject::CLIENT_CAPABILITY_REQUIRED),
                    );
                }
            }
        }
        // `arguments` est un **objet** (MCP : `CallToolRequestParams.arguments`
        // est `{ [key: string]: unknown }`). Un scalaire/tableau/`null` est
        // refusé **même sans schéma** : le contrat de l'API (« la closure
        // reçoit `arguments` (objet JSON) ») doit tenir inconditionnellement.
        let arguments = match params.get("arguments") {
            None => empty_object(),
            Some(value) if value.is_object() => value,
            Some(_) => {
                return (
                    Outgoing::failure(
                        id.clone(),
                        ErrorObject::new(
                            ErrorObject::INVALID_PARAMS,
                            "params.arguments must be an object",
                        ),
                    )
                    .to_bounded(response_cap),
                    CallStatus::ProtocolError(ErrorObject::INVALID_PARAMS),
                );
            }
        };
        // Préparation des arguments — **une seule copie** quand le schéma
        // porte des défauts à injecter ou des entiers à normaliser, zéro
        // copie sinon (chemin nominal). La normalisation des entiers
        // intégraux n'a lieu qu'après une validation réussie (tout intégral
        // présent est donc dans la plage exacte publiée) : le handler reçoit
        // des entiers exacts, `as_i64()` réussit toujours quand le schéma
        // passe.
        let mut prepared: Option<Value> = match &tool.schema {
            Some(schema) if schema.has_defaults() || schema.has_integers() => {
                let mut value = arguments.clone();
                if schema.has_defaults() {
                    schema.apply_defaults(&mut value);
                }
                Some(value)
            }
            _ => None,
        };
        let arguments_for_validation = prepared.as_ref().unwrap_or(arguments);
        if let Some(schema) = &tool.schema {
            let mut issues = Issues::new();
            let mut path = Path::root("arguments");
            schema.validate(&mut path, arguments_for_validation, &mut issues);
            // Schéma interne indisponible : erreur serveur neutre (-32603),
            // jamais -32602 imputée au client, jamais de détail interne.
            if issues.is_internal() {
                return internal_schema_failure(id, response_cap);
            }
            if !issues.is_empty() {
                let rendered = issues.render();
                return (
                    Outgoing::failure(
                        id.clone(),
                        ErrorObject::with_data(
                            ErrorObject::INVALID_PARAMS,
                            format!("Invalid arguments: {}", rendered.join(" ; ")),
                            json!({ "issues": rendered }),
                        ),
                    )
                    .to_bounded(response_cap),
                    CallStatus::ProtocolError(ErrorObject::INVALID_PARAMS),
                );
            }
        }
        // **Normalisation des entiers intégraux** — après une validation
        // réussie (tout intégral présent est donc dans la plage exacte
        // publiée) : `3.0`/`1e2` deviennent des entiers exacts, le handler
        // reçoit ce que `as_i64()` sait lire. Aucune conversion hors plage,
        // aucune valeur non entière touchée.
        if let (Some(schema), Some(value)) = (&tool.schema, prepared.as_mut()) {
            if schema.has_integers() {
                schema.normalize_integers(value);
            }
        }
        let arguments = prepared.as_ref().unwrap_or(arguments);
        // ------------------------------------------------------------------
        // Confirmation externe (C5) : **après** les droits (déjà vérifiés par
        // le dispatch), **avant** le handler, avec exactement les arguments
        // que le handler va recevoir. Rien n'est mis en file ni mémorisé : un
        // `Pending` est rendu au client, qui réessaie.
        // ------------------------------------------------------------------
        if let Some(gate) = self.auth_gate.as_deref() {
            if gate.approval_required.contains(name) {
                let context = auth::current();
                let refusal = match (gate.approval_policy.as_deref(), context.as_ref()) {
                    // `require_approval` sans politique : échec fermé — un outil
                    // marqué sensible ne s'exécute pas faute de décideur.
                    (None, _) => Some(AuthRefusal {
                        verdict: auth::AuthVerdict::ApprovalDenied,
                        reason: "approval_policy_missing",
                        wire_reason: "approval_denied",
                        retry_after: None,
                    }),
                    // Aucune identité sur le fil : ne devrait pas arriver (le
                    // dispatch a refusé en amont) — échec fermé malgré tout.
                    (Some(_), None) => Some(AuthRefusal {
                        verdict: auth::AuthVerdict::Unauthenticated,
                        reason: "unauthenticated",
                        wire_reason: "unauthenticated",
                        retry_after: None,
                    }),
                    (Some(policy), Some(context)) => {
                        let request = auth::ApprovalRequest {
                            client: context.client(),
                            tool: name,
                            arguments,
                            request_id: id,
                        };
                        // Une panique de la politique est un refus, jamais une
                        // interruption du service.
                        match std::panic::catch_unwind(AssertUnwindSafe(|| policy.decide(&request)))
                        {
                            Ok(auth::ApprovalVerdict::Approved) => None,
                            Ok(auth::ApprovalVerdict::Denied) => Some(AuthRefusal {
                                verdict: auth::AuthVerdict::ApprovalDenied,
                                reason: "approval_denied",
                                wire_reason: "approval_denied",
                                retry_after: None,
                            }),
                            Ok(auth::ApprovalVerdict::Pending { retry_after }) => {
                                Some(AuthRefusal {
                                    verdict: auth::AuthVerdict::ApprovalPending,
                                    reason: "approval_pending",
                                    wire_reason: "approval_pending",
                                    retry_after,
                                })
                            }
                            Err(_) => Some(AuthRefusal {
                                verdict: auth::AuthVerdict::ApprovalDenied,
                                reason: "approval_policy_panicked",
                                wire_reason: "approval_denied",
                                retry_after: None,
                            }),
                        }
                    }
                };
                if let Some(refusal) = refusal {
                    self.emit_auth_decision(
                        protocol::method::TOOLS_CALL,
                        Some(name),
                        context.as_ref().map(|c| c.client()),
                        refusal.verdict,
                        Some(refusal.reason),
                    );
                    return (
                        Outgoing::failure(id.clone(), forbidden_error(&refusal))
                            .to_bounded(response_cap),
                        CallStatus::ProtocolError(ErrorObject::FORBIDDEN),
                    );
                }
            }
        }
        // Exécution du handler, bornée par `tool_timeout` (défaut 30 s) — sauf
        // appel imbriqué dans un handler déjà borné (fil de l'appelant, échéance
        // héritée) et sauf `no_tool_timeout`. En mode borné, les arguments sont
        // clonés une fois (le handler s'exécute dans un thread qui doit les
        // posséder) — coût assumé, uniquement sur ce chemin opt-in. Une panique
        // y est rattrapée dans le thread du handler et rendue en `-32603` (sans
        // timeout, elle remonte au `catch_unwind` du transport — même verdict).
        let execution = match self.tool_timeout.filter(|_| !in_handler()) {
            None => {
                let _depth = HandlerDepthGuard::enter();
                match std::panic::catch_unwind(AssertUnwindSafe(|| match &tool.handler {
                    ToolHandler::Plain(f) => f(arguments),
                    ToolHandler::Mrtr(f) => f(arguments, round).map_err(ToolFailure::Tool),
                })) {
                    Ok(result) => result,
                    Err(_) => Err(ToolFailure::Protocol(
                        ErrorObject::INTERNAL_ERROR,
                        "internal error (handler panicked)".to_string(),
                    )),
                }
            }
            Some(timeout) => {
                let owned = arguments.clone();
                let owned_round = round.clone();
                let handler = tool.handler.clone();
                let ambient = crate::progress::current();
                let ambient_cancel = crate::cancel::token();
                let ambient_depth = crate::gateway::dispatch_depth();
                let ambient_auth = auth::current();
                match run_with_timeout(
                    &self.handler_pool,
                    timeout,
                    &self.inflight_handlers,
                    self.max_inflight_handlers,
                    move || {
                        let _cancel = crate::cancel::bind(ambient_cancel);
                        let _progress = crate::progress::bind(ambient);
                        let _gateway = crate::gateway::bind_dispatch_depth(ambient_depth);
                        let _auth = auth::bind(ambient_auth);
                        let _depth = HandlerDepthGuard::enter();
                        match &handler {
                            ToolHandler::Plain(f) => f(&owned),
                            ToolHandler::Mrtr(f) => {
                                f(&owned, &owned_round).map_err(ToolFailure::Tool)
                            }
                        }
                    },
                ) {
                    Outcome::Done(result) => result,
                    Outcome::Panicked => Err(ToolFailure::Protocol(
                        ErrorObject::INTERNAL_ERROR,
                        "internal error (handler panicked)".to_string(),
                    )),
                    // Le délai dépassé est classé `timeout` (retentable) :
                    // le client agent sait qu'il peut rejouer la requête.
                    Outcome::TimedOut => Err(ToolFailure::Tool(ToolError::timeout(format!(
                        "tool execution timed out after {} ms",
                        timeout.as_millis()
                    )))),
                    Outcome::Overloaded => Err(ToolFailure::Tool(ToolError::rate_limited(
                        "too many concurrent handlers in flight",
                    ))),
                }
            }
        };
        let (result, status) = match execution {
            Ok(ToolOutcome::InputRequired(spec)) => {
                // MRTR : le handler demande une entrée client — le noyau
                // valide les requêtes embarquées, frappe le `requestState`
                // signé et répond `input_required` (HTTP 200) ; aucun état
                // implicite n'est retenu de ce côté.
                return self.emit_input_required(
                    id,
                    protocol::method::TOOLS_CALL,
                    params,
                    spec,
                    round,
                );
            }
            Ok(ToolOutcome::Value(value)) => {
                // `outputSchema` est exécuteur comme l'input : la valeur
                // `structuredContent` réellement envoyée est validée avant
                // de partir — non conforme → résultat `isError` (le handler
                // a exécuté, c'est sa sortie qui viole le contrat publié).
                //
                // **Forme de la sortie** : le `structuredContent` doit
                // conformer au schéma **publié**. L'enveloppe
                // `{ "result": … }` (choix d'API : `structuredContent`
                // toujours objet) n'est applicable que si le schéma
                // l'autorise — sinon elle serait elle-même non conforme
                // et aucun outil à `outputSchema` de racine non-objet ne
                // pourrait jamais réussir.
                match &tool.output_schema {
                    None => (
                        CallToolResult::from(Ok::<Value, ToolError>(value)),
                        CallStatus::Success,
                    ),
                    Some(output) => {
                        // Le schéma est-il d'objet à la racine ? Alors
                        // l'enveloppe invariante est candidate ; sinon la
                        // valeur brute est envoyée telle quelle (la spec
                        // 2026-07-28 / SEP-2106 autorise toute valeur JSON
                        // dans `structuredContent`).
                        //
                        // Le drapeau est **précalculé** à l'enregistrement
                        // (`ToolDef::output_object_rooted`) : le schéma
                        // n'est donc pas re-sérialisé à chaque appel.
                        let structured = if tool.output_object_rooted && value.is_object() {
                            value
                        } else if tool.output_object_rooted {
                            serde_json::json!({ "result": value })
                        } else {
                            value
                        };
                        let mut issues = Issues::new();
                        let mut path = Path::root("output");
                        output.validate(&mut path, &structured, &mut issues);
                        if issues.is_empty() {
                            (
                                CallToolResult::from_structured(structured),
                                CallStatus::Success,
                            )
                        } else {
                            // Violation du contrat de sortie publié : échec
                            // interne (non retentable — retenter produirait
                            // la même sortie non conforme).
                            let rendered = issues.render().join(" ; ");
                            let error = ToolError::internal(format!(
                                "structured output does not conform to the declared outputSchema: {rendered}"
                            ));
                            let code = error.code;
                            (
                                tool_error_result(error, Some(output)),
                                CallStatus::ToolFailure(code),
                            )
                        }
                    }
                }
            }
            Ok(ToolOutcome::Result(result)) => {
                // Chemin riche : le handler a construit le résultat complet
                // (contenus mixtes, `structuredContent` explicite). Deux
                // seuls ajustements noyau :
                // 1. l'`outputSchema` reste exécuteur sur un
                //    `structuredContent` explicite — validé **tel quel**
                //    (sans enveloppe : le handler a choisi la forme finale,
                //    l'envelopper la trahirait) ;
                // 2. un `isError: true` explicite est rapporté aux hooks
                //    comme échec d'exécution (classification inconnue du
                //    noyau : le handler n'a pas fourni de `ToolError`).
                if let (Some(output), Some(structured)) =
                    (&tool.output_schema, &result.structured_content)
                {
                    let mut issues = Issues::new();
                    let mut path = Path::root("output");
                    output.validate(&mut path, structured, &mut issues);
                    if issues.is_empty() {
                        (result, CallStatus::Success)
                    } else {
                        let rendered = issues.render().join(" ; ");
                        let error = ToolError::internal(format!(
                            "structured output does not conform to the declared outputSchema: {rendered}"
                        ));
                        let code = error.code;
                        (
                            tool_error_result(error, Some(output)),
                            CallStatus::ToolFailure(code),
                        )
                    }
                } else if result.is_error == Some(true) {
                    (result, CallStatus::ToolFailure(ErrorCode::Unclassified))
                } else {
                    (result, CallStatus::Success)
                }
            }
            Err(ToolFailure::Tool(error)) => {
                let code = error.code;
                (
                    tool_error_result(error, tool.output_schema.as_ref()),
                    CallStatus::ToolFailure(code),
                )
            }
            Err(ToolFailure::Protocol(code, message)) => {
                return (
                    Outgoing::failure(id.clone(), ErrorObject::new(code, message))
                        .to_bounded(response_cap),
                    CallStatus::ProtocolError(code),
                );
            }
        };
        // SEP-2575 : le serveur s'identifie dans chaque résultat — succès
        // comme échec d'exécution (`isError: true` reste un `result`).
        // Sérialisation directe vers `Vec<u8>` par référence sans passer par
        // un arbre `serde_json::Value` intermédiaire ni cloner `self.meta`.
        (
            Outgoing::success_bounded(
                id,
                &CallToolResultRef {
                    result_type: result.result_type,
                    content: &result.content,
                    is_error: result.is_error,
                    structured_content: result.structured_content.as_ref(),
                    meta: &self.meta,
                },
                response_cap,
            ),
            status,
        )
    }
}
