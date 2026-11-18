//! Dispatch JSON-RPC : point d'entrée, authentification, décodage des
//! en-têtes `Mcp-Param-*` et corps de `dispatch_internal`.

use super::authz::{audit_target, is_element_method, rights_held};
use super::catalog::reponse_pre_serialisee;
use super::handlers::sanitize_echo;
use super::*;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpParamType {
    String,
    Integer,
    Boolean,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(super) struct McpParamDef {
    property_name: String,
    header_suffix: String,
    param_type: McpParamType,
}

pub(super) fn extract_mcp_param_defs(schema: &Value) -> Result<Vec<McpParamDef>, String> {
    let mut defs = Vec::new();
    let Some(obj) = schema.as_object() else {
        return Ok(defs);
    };
    let Some(props) = obj.get("properties").and_then(Value::as_object) else {
        return Ok(defs);
    };
    for (prop_name, prop_val) in props {
        if let Some(header_val) = prop_val.get("x-mcp-header") {
            let header_str = header_val
                .as_str()
                .ok_or_else(|| format!("property `{prop_name}`: x-mcp-header must be a string"))?;
            if header_str.is_empty() {
                return Err(format!(
                    "property `{prop_name}`: x-mcp-header cannot be empty"
                ));
            }
            if !header_str
                .bytes()
                .all(|b| b.is_ascii() && b != b' ' && b != b':')
            {
                return Err(format!(
                    "property `{prop_name}`: x-mcp-header `{header_str}` must be valid ASCII without spaces or colons"
                ));
            }
            // `type` peut être publié sous forme de tableau — `[T, "null"]`
            // pour un champ optionnel (même contrat que `Option<T>`) : le
            // paramètre de transport reste `T`, `null` n'ayant pas de valeur
            // d'en-tête.
            let type_str = match prop_val.get("type") {
                Some(Value::String(kind)) => kind.as_str(),
                Some(Value::Array(kinds)) => kinds
                    .iter()
                    .filter_map(Value::as_str)
                    .find(|kind| *kind != "null")
                    .unwrap_or(""),
                _ => "",
            };
            let param_type = match type_str {
                "string" => McpParamType::String,
                "integer" => McpParamType::Integer,
                "boolean" => McpParamType::Boolean,
                other => {
                    return Err(format!(
                        "property `{prop_name}` with x-mcp-header has unsupported type `{other}` (only string, integer, boolean allowed, number is forbidden)"
                    ));
                }
            };
            let lower_suffix = header_str.to_ascii_lowercase();
            if defs
                .iter()
                .any(|d: &McpParamDef| d.header_suffix == lower_suffix)
            {
                return Err(format!(
                    "duplicate x-mcp-header `{header_str}` (case-insensitive) in tool schema"
                ));
            }
            defs.push(McpParamDef {
                property_name: prop_name.clone(),
                header_suffix: lower_suffix,
                param_type,
            });
        }
    }
    Ok(defs)
}

#[allow(dead_code)]
fn decode_base64(input: &str) -> Result<Vec<u8>, &'static str> {
    if input.is_empty() {
        return Ok(Vec::new());
    }
    let bytes = input.as_bytes();
    if bytes.len() % 4 != 0 {
        return Err("base64 length must be a multiple of 4");
    }
    let mut out = Vec::with_capacity((bytes.len() / 4) * 3);
    for chunk in bytes.chunks_exact(4) {
        let mut b = [0u8; 4];
        let mut pad_count = 0;
        for i in 0..4 {
            let c = chunk[i];
            if c == b'=' {
                pad_count += 1;
                if i < 2 {
                    return Err("invalid padding position");
                }
            } else if pad_count > 0 {
                return Err("character after padding");
            } else {
                b[i] = match c {
                    b'A'..=b'Z' => c - b'A',
                    b'a'..=b'z' => c - b'a' + 26,
                    b'0'..=b'9' => c - b'0' + 52,
                    b'+' => 62,
                    b'/' => 63,
                    _ => return Err("invalid base64 character"),
                };
            }
        }
        if pad_count == 0 {
            out.push((b[0] << 2) | (b[1] >> 4));
            out.push((b[1] << 4) | (b[2] >> 2));
            out.push((b[2] << 6) | b[3]);
        } else if pad_count == 1 {
            out.push((b[0] << 2) | (b[1] >> 4));
            out.push((b[1] << 4) | (b[2] >> 2));
        } else if pad_count == 2 {
            out.push((b[0] << 2) | (b[1] >> 4));
        } else {
            return Err("too many padding characters");
        }
    }
    Ok(out)
}

#[allow(dead_code)]
fn decode_mcp_header_value(raw: &str) -> Result<String, &'static str> {
    let raw_trimmed = raw.trim();
    if raw_trimmed.len() >= 11
        && raw_trimmed[..9].eq_ignore_ascii_case("=?base64?")
        && raw_trimmed.ends_with("?=")
    {
        let encoded = &raw_trimmed[9..raw_trimmed.len() - 2];
        let bytes = decode_base64(encoded)?;
        String::from_utf8(bytes).map_err(|_| "invalid utf-8 in base64 header value")
    } else {
        Ok(raw_trimmed.to_string())
    }
}

/// Nom d'outil/prompt ou `uri` de ressource d'une requête, pour
/// l'observation ([`CallEvent::name`]) — extraction best-effort depuis
/// `params`, `None` si la méthode n'en porte pas. Pour
/// `completion/complete`, la référence (`ref.name` ou `ref.uri`) sert de
/// nom.
fn requested_name<'a>(method: &str, params: &'a Value) -> Option<&'a str> {
    if method == protocol::method::COMPLETION_COMPLETE {
        return params
            .get("ref")
            .and_then(|r| r.get("name").or_else(|| r.get("uri")))
            .and_then(Value::as_str);
    }
    let key = if method == protocol::method::RESOURCES_READ {
        "uri"
    } else {
        "name"
    };
    params.get(key).and_then(Value::as_str)
}

impl ServerCore {
    /// Traite un corps de requête JSON-RPC.
    ///
    /// Renvoie `None` pour une notification (aucune réponse attendue —
    /// le transport répondra `202 Accepted` sans corps).
    ///
    /// Conforme JSON-RPC : `id: null` explicite reçoit une réponse avec
    /// `id: null` ; version `jsonrpc` invalide ou lot → `-32600` ; JSON
    /// illisible → `-32700`. **L'`id` lisible est toujours repris** dans la
    /// réponse d'erreur (JSON-RPC 2.0 §5).
    ///
    /// Conforme MCP `2026-07-28` : `server/discover` est implémentée, chaque
    /// `result` porte `resultType`, et une version de protocole non supportée
    /// est refusée en `-32022`.
    ///
    /// **Le profil configuré s'applique** à tout ce qui ne dépend pas des
    /// en-têtes HTTP : en profil strict (défaut), un corps sans `_meta` est
    /// refusé en `-32602` (message `missing required _meta object in params`), une
    /// version de protocole inconnue en `-32022`, un `id: null` en `-32600`.
    /// C'est exactement le verdict de [`Self::dispatch_request`] appelé sans
    /// en-têtes — un même noyau, un même corps, un même verdict. Les
    /// contrôles qui n'ont de sens qu'avec un transport HTTP
    /// (`MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Name`) s'appliquent dès
    /// que des en-têtes sont fournis ([`Self::dispatch_with_headers`],
    /// [`Self::dispatch_request`]).
    ///
    /// Un hôte qui garantit déjà des corps strictement conformes (dispatch
    /// embarqué d'un gateway, tests unitaires de handlers) dispose de
    /// [`Self::dispatch_unchecked`] — nom explicite, jamais un défaut
    /// silencieux.
    ///
    /// **Autorisation** : dès qu'un [`ServerBuilder::verifier`] est
    /// configuré, ce point d'entrée **n'apporte aucune identité** et la
    /// requête est donc refusée (`-32023` / `unauthenticated`, HTTP 403) —
    /// c'est l'échec fermé : il n'existe pas de chemin simple qui contourne
    /// la vérification. Utilisez [`Self::dispatch_authenticated`].
    pub fn dispatch(&self, body: &[u8]) -> Option<Vec<u8>> {
        self.dispatch_request(body, None).map(|r| r.body)
    }

    /// Idem [`ServerCore::dispatch`], avec les en-têtes de routage MCP
    /// (SEP-2243) pour vérifier leur cohérence avec le corps de la requête.
    ///
    /// Le profil configuré s'applique comme pour [`Self::dispatch`] ; les
    /// en-têtes fournis sont en plus confrontés au corps (méthode, nom,
    /// version). Sans en-têtes (`None`), seuls les contrôles de corps
    /// s'appliquent.
    pub fn dispatch_with_headers(
        &self,
        body: &[u8],
        headers: Option<&McpHeaders>,
    ) -> Option<Vec<u8>> {
        self.dispatch_request(body, headers).map(|r| r.body)
    }

    /// Idem [`Self::dispatch_with_headers`], avec une identité **déjà
    /// établie** par le transport.
    ///
    /// C'est le point d'entrée d'une application qui possède son propre
    /// socket HTTP (MCP Control, passerelle déjà authentifiée) : le transport
    /// prouve l'identité ([`AuthContext`] ou vérificateur), le noyau décide
    /// des droits et de la confirmation.
    ///
    /// Le contexte est lié au fil pour la durée du dispatch : un dispatch
    /// imbriqué (handler appelant un [`crate::Gateway`]) en hérite, et il est
    /// lisible depuis un handler par [`crate::auth::current`].
    ///
    /// Renvoie `None` pour une notification, comme [`Self::dispatch_request`].
    pub fn dispatch_authenticated(
        &self,
        body: &[u8],
        headers: Option<&McpHeaders>,
        context: &AuthContext,
    ) -> Option<DispatchResponse> {
        self.dispatch_internal(
            body,
            headers,
            self.protocol_profile == ProtocolProfile::Strict2026_07_28,
            None,
            Some(context),
        )
    }

    /// Vérifie l'identité d'une requête brute par le [`auth::Verifier`]
    /// configuré.
    ///
    /// * `None` : aucun vérificateur — l'autorisation est **inactive**, le
    ///   transport dispatche sans identité (comportement historique) ;
    /// * `Some(Ok(context))` : identité établie, à passer à
    ///   [`Self::dispatch_authenticated`] ;
    /// * `Some(Err(error))` : refus — le transport rend un `401` générique et
    ///   ne publie le motif qu'à l'application.
    ///
    /// Une panique du vérificateur est **contenue** et traitée comme un refus
    /// (voir [`auth::verify_or_refuse`]) : une entrée réseau ne fait jamais
    /// tomber le service.
    #[must_use]
    pub fn authenticate(
        &self,
        request: &auth::VerifiedRequest<'_>,
    ) -> Option<Result<AuthContext, auth::AuthError>> {
        self.auth_gate
            .as_ref()
            .map(|gate| auth::verify_or_refuse(gate.verifier.as_ref(), request))
    }

    /// **Vérifie puis dispatche** une requête brute — le point d'entrée
    /// unique d'une application qui possède son propre socket HTTP
    /// (MCP Control, contrat C8).
    ///
    /// La vérification est **inconditionnelle** dès qu'un
    /// [`ServerBuilder::verifier`] est configuré : il n'existe pas de variante
    /// de cette fonction qui l'omettrait, et les autres points d'entrée
    /// ([`Self::dispatch`], [`Self::dispatch_request`],
    /// [`Self::dispatch_unchecked`]) refusent la requête en `-32023` /
    /// `unauthenticated` (403) quand l'autorisation est active. Le dispatch
    /// « authentifié » reste, lui, l'API d'un **transport** qui vient de
    /// prouver l'identité : c'est le contrat de [`auth::Verifier`].
    ///
    /// Sortie : `401` + `WWW-Authenticate` et corps **constant** sur refus
    /// d'identité ([`auth::AuthResponse::unauthorized`]), `403` + corps
    /// `-32023` sur refus d'autorisation, `202` sans corps pour une
    /// notification vérifiée, sinon le statut du dispatch (`200`, `400`,
    /// `404`, `500`).
    ///
    /// Les en-têtes de routage MCP (SEP-2243) sont extraits des en-têtes
    /// bruts ([`McpHeaders::from_headers`]) : la cohérence avec le corps est
    /// vérifiée par le noyau, comme sur le transport intégré.
    ///
    /// Sans vérificateur configuré, la fonction équivaut à
    /// [`Self::dispatch_request`] (autorisation inactive) et rend `202` pour
    /// une notification.
    #[must_use]
    pub fn dispatch_verified(&self, request: &auth::VerifiedRequest<'_>) -> auth::AuthResponse {
        let headers = McpHeaders::from_headers(request.headers);
        let Some(outcome) = self.authenticate(request) else {
            // Autorisation inactive : chemin historique, aucune allocation
            // d'identité.
            return match self.dispatch_request(request.body, Some(&headers)) {
                Some(reply) => auth::AuthResponse::new(reply.status.code(), Vec::new(), reply.body),
                None => auth::AuthResponse::new(202, Vec::new(), Vec::new()),
            };
        };
        let context = match outcome {
            Ok(context) => context,
            Err(error) => {
                // Le motif précis reste côté application (hook d'audit) ; le
                // réseau ne reçoit que le 401 constant.
                let (method, name) = audit_target(request.body, &headers);
                self.report_authentication_refusal(&method, name.as_deref(), error.reason());
                return auth::AuthResponse::unauthorized();
            }
        };
        match self.dispatch_authenticated(request.body, Some(&headers), &context) {
            Some(reply) => auth::AuthResponse::new(reply.status.code(), Vec::new(), reply.body),
            None => auth::AuthResponse::new(202, Vec::new(), Vec::new()),
        }
    }

    /// Publie un refus d'authentification au journal de l'application
    /// ([`ServerBuilder::on_auth_decision`]) — le motif **précis** n'existe
    /// que là.
    ///
    /// Appelé par le transport intégré et par [`Self::dispatch_verified`] ;
    /// une application qui vérifie elle-même ses requêtes peut l'appeler
    /// pour tenir le même journal. No-op sans hook ni autorisation.
    pub fn report_authentication_refusal(&self, method: &str, name: Option<&str>, reason: &str) {
        self.emit_auth_decision(
            method,
            name,
            None,
            auth::AuthVerdict::Unauthenticated,
            Some(reason),
        );
    }

    /// L'autorisation est-elle active sur ce noyau (un [`ServerBuilder::verifier`]
    /// a-t-il été configuré) ?
    #[must_use]
    pub fn authorization_active(&self) -> bool {
        self.auth_gate.is_some()
    }

    /// Traite un corps JSON-RPC **sans aucun contrôle de profil ni
    /// d'en-tête** — ni `_meta`, ni version de protocole, ni `id: null`.
    ///
    /// Réservé aux appelants qui **garantissent** déjà des corps conformes
    /// au profil strict `2026-07-28` : tests unitaires de handlers, dispatch
    /// embarqué d'un gateway, harnais de mesure. Ne l'exposez jamais à une
    /// entrée non fiable : c'est le seul point d'entrée qui n'applique pas
    /// le profil configuré. Pour un corps de client, utilisez
    /// [`Self::dispatch`] ou [`Self::dispatch_request`].
    ///
    /// Renvoie `None` pour une notification, comme [`Self::dispatch`].
    ///
    /// **Autorisation** : comme [`Self::dispatch`], aucun contexte n'est
    /// fourni — refus dès qu'un vérificateur est configuré.
    pub fn dispatch_unchecked(&self, body: &[u8]) -> Option<Vec<u8>> {
        self.dispatch_internal(body, None, false, None, None)
            .map(|r| r.body)
    }

    /// Traite un corps de requête JSON-RPC avec un **canal de progression**
    /// request-scoped.
    ///
    /// Le canal (voir [`crate::progress`]) reçoit les
    /// `notifications/progress` émises par les handlers pendant l'exécution :
    /// c'est au transport de les livrer (SSE du POST) et de reprendre le
    /// jeton `progressToken` de la requête. Sans canal — ou si le client n'a
    /// pas fourni de jeton — [`crate::progress::report`] est une non-action.
    ///
    /// Renvoie `None` pour une notification, comme [`Self::dispatch_request`].
    pub fn dispatch_request_with_progress(
        &self,
        body: &[u8],
        headers: Option<&McpHeaders>,
        channel: Arc<dyn crate::progress::ProgressChannel>,
    ) -> Option<DispatchResponse> {
        self.dispatch_internal(
            body,
            headers,
            self.protocol_profile == ProtocolProfile::Strict2026_07_28,
            Some(channel),
            None,
        )
    }

    /// Idem [`Self::dispatch_request_with_progress`], avec une identité
    /// **déjà établie** par le transport — le chemin d'une requête servie en
    /// flux SSE par un hôte qui a authentifié la requête lui-même.
    ///
    /// Renvoie `None` pour une notification, comme [`Self::dispatch_request`].
    pub fn dispatch_authenticated_with_progress(
        &self,
        body: &[u8],
        headers: Option<&McpHeaders>,
        context: &AuthContext,
        channel: Arc<dyn crate::progress::ProgressChannel>,
    ) -> Option<DispatchResponse> {
        self.dispatch_internal(
            body,
            headers,
            self.protocol_profile == ProtocolProfile::Strict2026_07_28,
            Some(channel),
            Some(context),
        )
    }

    /// Traite un corps de requête JSON-RPC et produit une réponse structurée
    /// contenant le corps et le statut HTTP associé.
    ///
    /// Renvoie `None` pour une notification (aucune réponse attendue —
    /// le transport répondra `202 Accepted` sans corps).
    pub fn dispatch_request(
        &self,
        body: &[u8],
        headers: Option<&McpHeaders>,
    ) -> Option<DispatchResponse> {
        self.dispatch_internal(
            body,
            headers,
            self.protocol_profile == ProtocolProfile::Strict2026_07_28,
            None,
            None,
        )
    }

    fn dispatch_internal(
        &self,
        body: &[u8],
        headers: Option<&McpHeaders>,
        strict_profile: bool,
        progress_channel: Option<Arc<dyn crate::progress::ProgressChannel>>,
        auth_context: Option<&AuthContext>,
    ) -> Option<DispatchResponse> {
        // Canal de progression lié au fil du dispatch : les handlers
        // synchrones l'atteignent via `progress::report`, y compris sur un
        // retour anticipé (le garde le délie au `Drop`).
        let _progress = crate::progress::bind(progress_channel);
        let incoming = match crate::jsonrpc::parse_incoming(body) {
            Ok(incoming) => incoming,
            Err(crate::jsonrpc::ParseFail::Syntax) => {
                return Some(DispatchResponse {
                    body: Outgoing::parse_error().to_vec(),
                    status: HttpStatus::BadRequest,
                });
            }
            Err(fail) => {
                // L'id est repris quand il a pu être lu (JSON-RPC 2.0 §5).
                let id = fail.id().cloned();
                let message = match fail {
                    crate::jsonrpc::ParseFail::Syntax => String::new(),
                    crate::jsonrpc::ParseFail::Invalid { message, .. } => message,
                };
                return Some(DispatchResponse {
                    body: Outgoing::invalid_request(id, format!("invalid request: {message}"))
                        .to_vec(),
                    status: HttpStatus::BadRequest,
                });
            }
        };
        // L'observation démarre après le parsing : un corps illisible n'a
        // ni méthode ni outil à rapporter.
        let started = Instant::now();
        // id réel, ou null explicite : les deux exigent une réponse ; l'id
        // null explicite est rendu tel quel (Value::Null).
        let id = incoming
            .id
            .as_ref()
            .map(RequestId::to_value)
            .unwrap_or(Value::Null);
        let params = incoming.params.as_ref().unwrap_or_else(|| empty_object());
        // Nom de l'outil/prompt ou uri de la ressource pour l'observation —
        // extraction best-effort, sans coût sur les méthodes sans nom.
        let name = requested_name(&incoming.method, params);

        // ------------------------------------------------------------------
        // Autorisation : l'identité et les droits se décident **avant** tout
        // diagnostic protocolaire et avant tout handler. Un appelant non
        // authentifié n'apprend donc rien du profil du serveur, et rien ne
        // s'exécute pour lui.
        //
        // Le contexte vient du transport ([`Self::dispatch_authenticated`]) ou,
        // à défaut, du fil courant : un dispatch imbriqué (handler appelant un
        // [`crate::Gateway`]) hérite ainsi de l'identité de la requête
        // extérieure, comme `progress` et `cancel` le font pour leurs canaux.
        // ------------------------------------------------------------------
        let context = auth_context.cloned().or_else(auth::current);
        // Lié au fil : handlers, hook `on_call` et dispatchs imbriqués le
        // lisent via [`crate::auth::current`] ; le garde le délie au retour.
        let _auth_scope = auth::bind(context.clone());
        if self.auth_gate.is_some() {
            let Some(context) = context.as_ref() else {
                // Échec fermé : autorisation active, aucune identité vérifiée.
                // Une notification n'a pas de réponse : son refus part au
                // journal ; une requête passe par `auth_failure`, qui publie
                // l'événement et la réponse.
                if incoming.is_notification() {
                    self.emit_auth_decision(
                        &incoming.method,
                        name,
                        None,
                        auth::AuthVerdict::Unauthenticated,
                        Some("unauthenticated"),
                    );
                    return None;
                }
                return Some(self.auth_failure(
                    &id,
                    &incoming,
                    name,
                    started,
                    AuthRefusal {
                        verdict: auth::AuthVerdict::Unauthenticated,
                        reason: "unauthenticated",
                        wire_reason: "unauthenticated",
                        retry_after: None,
                    },
                    None,
                ));
            };
            if is_element_method(&incoming.method) {
                if let Some(required) = self.element_rights(&incoming.method, params) {
                    if !rights_held(context, &required) {
                        return Some(self.auth_failure(
                            &id,
                            &incoming,
                            name,
                            started,
                            AuthRefusal {
                                verdict: auth::AuthVerdict::RightMissing,
                                reason: "right_missing",
                                wire_reason: "right_missing",
                                retry_after: None,
                            },
                            Some(context.client()),
                        ));
                    }
                    // Décision rendue pour un élément existant : publiée au
                    // journal de l'application (jamais au réseau).
                    self.emit_auth_decision(
                        &incoming.method,
                        name,
                        Some(context.client()),
                        auth::AuthVerdict::Allowed,
                        None,
                    );
                }
            }
        }

        // Cohérence des en-têtes de routage MCP (SEP-2243) : `MCP-Protocol-Version`,
        // `Mcp-Method`, `Mcp-Name` et `Mcp-Param-*`.
        let mut header_mismatch: Option<ErrorObject> = None;
        // Les contrôles d'en-têtes n'ont de sens que si des en-têtes sont
        // fournis : `dispatch` (et `dispatch_request` sans en-têtes)
        // n'applique que le profil de corps, pour qu'un même corps rende le
        // même verdict quel que soit le point d'entrée.
        let is_modern = strict_profile && headers.is_some();

        // 1. Validation de MCP-Protocol-Version
        let body_meta_version = params
            .get("_meta")
            .and_then(|m| m.get(protocol::META_PROTOCOL_VERSION))
            .and_then(Value::as_str);

        if is_modern {
            match headers.and_then(|h| h.protocol_version.as_deref()) {
                None | Some("") => {
                    header_mismatch = Some(ErrorObject::with_data(
                        ErrorObject::HEADER_MISMATCH,
                        "MCP-Protocol-Version header is required in modern MCP profile",
                        json!({}),
                    ));
                }
                Some(ver) => {
                    let ver = ver.trim();
                    if let Some(body_ver) = body_meta_version {
                        if ver != body_ver {
                            header_mismatch = Some(ErrorObject::with_data(
                                ErrorObject::HEADER_MISMATCH,
                                "MCP-Protocol-Version header does not match the request body",
                                json!({
                                    "header": ver,
                                    "body": body_ver,
                                }),
                            ));
                        }
                    } else if !protocol::SUPPORTED_VERSIONS.contains(&ver) {
                        header_mismatch = Some(ErrorObject::with_data(
                            ErrorObject::HEADER_MISMATCH,
                            "MCP-Protocol-Version header specifies an unsupported protocol version",
                            json!({ "header": ver }),
                        ));
                    }
                }
            }
        } else if let Some(ver) = headers.and_then(|h| h.protocol_version.as_deref()) {
            let ver = ver.trim();
            if let Some(body_ver) = body_meta_version {
                if ver != body_ver {
                    header_mismatch = Some(ErrorObject::with_data(
                        ErrorObject::HEADER_MISMATCH,
                        "MCP-Protocol-Version header does not match the request body",
                        json!({
                            "header": ver,
                            "body": body_ver,
                        }),
                    ));
                }
            }
        }

        // 2. Validation de Mcp-Method
        if header_mismatch.is_none() {
            if is_modern {
                match headers.and_then(|h| h.method.as_deref()) {
                    None | Some("") => {
                        header_mismatch = Some(ErrorObject::with_data(
                            ErrorObject::HEADER_MISMATCH,
                            "Mcp-Method header is required in modern MCP profile",
                            json!({}),
                        ));
                    }
                    Some(m) => {
                        let m = m.trim();
                        if m != incoming.method {
                            header_mismatch = Some(ErrorObject::with_data(
                                ErrorObject::HEADER_MISMATCH,
                                "Mcp-Method header does not match the request body",
                                json!({
                                    "header": m,
                                    "body": incoming.method,
                                }),
                            ));
                        }
                    }
                }
            } else if let Some(m) = headers.and_then(|h| h.method.as_deref()) {
                let m = m.trim();
                if m.is_empty() || m != incoming.method {
                    header_mismatch = Some(ErrorObject::with_data(
                        ErrorObject::HEADER_MISMATCH,
                        "Mcp-Method header does not match the request body",
                        json!({
                            "header": m,
                            "body": incoming.method,
                        }),
                    ));
                }
            }
        }

        // 3. Validation de Mcp-Name
        let (expects_name, body_name) = match incoming.method.as_str() {
            protocol::method::TOOLS_CALL | protocol::method::PROMPTS_GET => (
                true,
                params.get("name").and_then(Value::as_str).unwrap_or(""),
            ),
            protocol::method::RESOURCES_READ => (
                true,
                params.get("uri").and_then(Value::as_str).unwrap_or(""),
            ),
            _ => (
                false,
                params
                    .get("name")
                    .or_else(|| params.get("uri"))
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            ),
        };

        if header_mismatch.is_none() {
            if is_modern && expects_name {
                match headers.and_then(|h| h.name.as_deref()) {
                    None | Some("") => {
                        header_mismatch = Some(ErrorObject::with_data(
                            ErrorObject::HEADER_MISMATCH,
                            "Mcp-Name header is required for this method in modern MCP profile",
                            json!({ "method": incoming.method }),
                        ));
                    }
                    Some(raw_name) => match decode_mcp_header_value(raw_name) {
                        Err(err) => {
                            header_mismatch = Some(ErrorObject::with_data(
                                ErrorObject::HEADER_MISMATCH,
                                format!("Malformed Mcp-Name header: {err}"),
                                json!({ "header": raw_name }),
                            ));
                        }
                        Ok(decoded) => {
                            if body_name.is_empty() || decoded != body_name {
                                header_mismatch = Some(ErrorObject::with_data(
                                    ErrorObject::HEADER_MISMATCH,
                                    "Mcp-Name header does not match the request body",
                                    json!({
                                        "header": decoded,
                                        "body": body_name,
                                    }),
                                ));
                            }
                        }
                    },
                }
            } else if let Some(raw_name) = headers.and_then(|h| h.name.as_deref()) {
                let trimmed = raw_name.trim();
                if trimmed.is_empty() {
                    header_mismatch = Some(ErrorObject::with_data(
                        ErrorObject::HEADER_MISMATCH,
                        "Mcp-Name header cannot be empty",
                        json!({ "header": raw_name }),
                    ));
                } else if !expects_name {
                    header_mismatch = Some(ErrorObject::with_data(
                        ErrorObject::HEADER_MISMATCH,
                        "Mcp-Name header is not permitted for this method",
                        json!({ "method": incoming.method }),
                    ));
                } else {
                    match decode_mcp_header_value(trimmed) {
                        Err(err) => {
                            header_mismatch = Some(ErrorObject::with_data(
                                ErrorObject::HEADER_MISMATCH,
                                format!("Malformed Mcp-Name header: {err}"),
                                json!({ "header": raw_name }),
                            ));
                        }
                        Ok(decoded) => {
                            if body_name.is_empty() || decoded != body_name {
                                header_mismatch = Some(ErrorObject::with_data(
                                    ErrorObject::HEADER_MISMATCH,
                                    "Mcp-Name header does not match the request body",
                                    json!({
                                        "header": decoded,
                                        "body": body_name,
                                    }),
                                ));
                            }
                        }
                    }
                }
            }
        }

        // 4. Validation des Mcp-Param-*
        if header_mismatch.is_none() {
            if incoming.method.as_str() == protocol::method::TOOLS_CALL {
                if let Some(tool) = self.tools.get(body_name) {
                    let empty_map = serde_json::Map::new();
                    let args_obj = params
                        .get("arguments")
                        .and_then(Value::as_object)
                        .unwrap_or(&empty_map);

                    for param_def in &tool.mcp_params {
                        let arg_val = args_obj.get(&param_def.property_name);
                        let arg_present = arg_val.is_some_and(|v| !v.is_null());
                        let found_header = headers.and_then(|h| {
                            h.params
                                .iter()
                                .find(|(k, _)| k == &param_def.header_suffix)
                                .map(|(_, v)| v.as_str())
                        });

                        match (arg_present, found_header) {
                            (false, Some(_)) => {
                                header_mismatch = Some(ErrorObject::with_data(
                                    ErrorObject::HEADER_MISMATCH,
                                    format!(
                                        "Mcp-Param-{} header must be absent when argument `{}` is absent or null",
                                        param_def.header_suffix, param_def.property_name
                                    ),
                                    json!({ "param": param_def.property_name }),
                                ));
                                break;
                            }
                            (true, None) => {
                                header_mismatch = Some(ErrorObject::with_data(
                                    ErrorObject::HEADER_MISMATCH,
                                    format!(
                                        "Mcp-Param-{} header is required when argument `{}` is present",
                                        param_def.header_suffix, param_def.property_name
                                    ),
                                    json!({ "param": param_def.property_name }),
                                ));
                                break;
                            }
                            (true, Some(raw_val)) => match decode_mcp_header_value(raw_val) {
                                Err(err) => {
                                    header_mismatch = Some(ErrorObject::with_data(
                                        ErrorObject::HEADER_MISMATCH,
                                        format!(
                                            "Malformed Mcp-Param-{} header: {err}",
                                            param_def.header_suffix
                                        ),
                                        json!({ "header": raw_val }),
                                    ));
                                    break;
                                }
                                Ok(decoded) => {
                                    let val = arg_val.unwrap();
                                    let matched = match param_def.param_type {
                                        McpParamType::String => val.as_str() == Some(&decoded),
                                        McpParamType::Integer => {
                                            if let Ok(parsed_int) = decoded.trim().parse::<i64>() {
                                                if let Some(arg_i) = val.as_i64() {
                                                    arg_i == parsed_int
                                                } else if let Some(arg_f) = val.as_f64() {
                                                    arg_f.fract() == 0.0
                                                        && arg_f == parsed_int as f64
                                                } else {
                                                    false
                                                }
                                            } else {
                                                false
                                            }
                                        }
                                        McpParamType::Boolean => match decoded.trim() {
                                            "true" => val.as_bool() == Some(true),
                                            "false" => val.as_bool() == Some(false),
                                            _ => false,
                                        },
                                    };
                                    if !matched {
                                        header_mismatch = Some(ErrorObject::with_data(
                                            ErrorObject::HEADER_MISMATCH,
                                            format!(
                                                "Mcp-Param-{} header value does not match argument `{}`",
                                                param_def.header_suffix, param_def.property_name
                                            ),
                                            json!({
                                                "header": decoded,
                                                "body": val,
                                            }),
                                        ));
                                        break;
                                    }
                                }
                            },
                            (false, None) => {}
                        }
                    }

                    if header_mismatch.is_none() {
                        if let Some(headers) = headers {
                            for (k, _) in &headers.params {
                                if !tool.mcp_params.iter().any(|d| &d.header_suffix == k) {
                                    header_mismatch = Some(ErrorObject::with_data(
                                        ErrorObject::HEADER_MISMATCH,
                                        format!("Unexpected Mcp-Param-{k} header"),
                                        json!({ "header": k }),
                                    ));
                                    break;
                                }
                            }
                        }
                    }
                } else if let Some(headers) = headers {
                    if !headers.params.is_empty() {
                        header_mismatch = Some(ErrorObject::with_data(
                            ErrorObject::HEADER_MISMATCH,
                            "Mcp-Param-* headers are unexpected for unknown tool",
                            json!({}),
                        ));
                    }
                }
            } else if let Some(headers) = headers {
                if !headers.params.is_empty() {
                    header_mismatch = Some(ErrorObject::with_data(
                        ErrorObject::HEADER_MISMATCH,
                        "Mcp-Param-* headers are only permitted on tools/call",
                        json!({ "method": incoming.method }),
                    ));
                }
            }
        }
        if let Some(error) = header_mismatch {
            if incoming.is_notification() {
                self.emit_call(
                    &incoming.method,
                    name,
                    started.elapsed(),
                    CallStatus::ProtocolError(ErrorObject::HEADER_MISMATCH),
                );
                return None;
            }
            return Some(self.protocol_failure(
                &id,
                &incoming,
                name,
                started,
                error,
                HttpStatus::BadRequest,
            ));
        }

        if incoming.is_notification() {
            if protocol::is_action(&incoming.method) {
                return Some(self.protocol_failure(
                    &Value::Null,
                    &incoming,
                    name,
                    started,
                    ErrorObject::new(
                        ErrorObject::INVALID_REQUEST,
                        format!(
                            "notification rejected: `{}` executes server-side code — \
                             a request (with `id`) is required to obtain the result",
                            sanitize_echo(&incoming.method)
                        ),
                    ),
                    HttpStatus::BadRequest,
                ));
            }
            if !protocol::is_protocol_notification(&incoming.method) {
                return Some(self.protocol_failure(
                    &Value::Null,
                    &incoming,
                    name,
                    started,
                    ErrorObject::new(
                        ErrorObject::INVALID_REQUEST,
                        format!(
                            "unknown notification: `{}` — only `notifications/*` are accepted \
                             (action methods require a request with an `id`)",
                            sanitize_echo(&incoming.method)
                        ),
                    ),
                    HttpStatus::BadRequest,
                ));
            }
            self.emit_call(
                &incoming.method,
                None,
                started.elapsed(),
                CallStatus::Notification,
            );
            return None;
        }

        // Requête (non-notification) :
        // 1. Validation de l'id null en mode strict (rejet en -32600 + HTTP 400)
        if incoming.null_id && self.protocol_profile == ProtocolProfile::Strict2026_07_28 {
            return Some(self.protocol_failure(
                &Value::Null,
                &incoming,
                name,
                started,
                ErrorObject::new(
                    ErrorObject::INVALID_REQUEST,
                    "request id must be a string or a number (null is not permitted in modern MCP 2026-07-28)",
                ),
                HttpStatus::BadRequest,
            ));
        }

        // 2. Validation des métadonnées obligatoires en mode strict MCP 2026-07-28
        if strict_profile {
            if incoming.params.is_none() {
                return Some(self.protocol_failure(
                    &id,
                    &incoming,
                    name,
                    started,
                    ErrorObject::new(
                        ErrorObject::INVALID_PARAMS,
                        "params object is required and must contain `_meta`",
                    ),
                    HttpStatus::BadRequest,
                ));
            }
            let Some(meta) = params.get("_meta") else {
                return Some(self.protocol_failure(
                    &id,
                    &incoming,
                    name,
                    started,
                    ErrorObject::new(
                        ErrorObject::INVALID_PARAMS,
                        "missing required `_meta` object in params",
                    ),
                    HttpStatus::BadRequest,
                ));
            };
            let Some(meta_obj) = meta.as_object() else {
                return Some(self.protocol_failure(
                    &id,
                    &incoming,
                    name,
                    started,
                    ErrorObject::new(ErrorObject::INVALID_PARAMS, "`_meta` must be a JSON object"),
                    HttpStatus::BadRequest,
                ));
            };

            let Some(ver_val) = meta_obj.get(protocol::META_PROTOCOL_VERSION) else {
                return Some(self.protocol_failure(
                    &id,
                    &incoming,
                    name,
                    started,
                    ErrorObject::new(
                        ErrorObject::INVALID_PARAMS,
                        "missing required `io.modelcontextprotocol/protocolVersion` in `_meta`",
                    ),
                    HttpStatus::BadRequest,
                ));
            };
            let Some(version_str) = ver_val.as_str() else {
                return Some(self.protocol_failure(
                    &id,
                    &incoming,
                    name,
                    started,
                    ErrorObject::new(
                        ErrorObject::INVALID_PARAMS,
                        "`io.modelcontextprotocol/protocolVersion` must be a string",
                    ),
                    HttpStatus::BadRequest,
                ));
            };
            if !protocol::SUPPORTED_VERSIONS.contains(&version_str) {
                let safe = sanitize_echo(version_str);
                return Some(self.protocol_failure(
                    &id,
                    &incoming,
                    name,
                    started,
                    ErrorObject::with_data(
                        ErrorObject::UNSUPPORTED_PROTOCOL_VERSION,
                        format!("Unsupported protocol version: {safe}"),
                        json!({
                            "supported": protocol::SUPPORTED_VERSIONS,
                            "requested": safe,
                        }),
                    ),
                    HttpStatus::BadRequest,
                ));
            }

            let Some(caps_val) = meta_obj.get(protocol::META_CLIENT_CAPABILITIES) else {
                return Some(self.protocol_failure(
                    &id,
                    &incoming,
                    name,
                    started,
                    ErrorObject::new(
                        ErrorObject::INVALID_PARAMS,
                        "missing required `io.modelcontextprotocol/clientCapabilities` in `_meta`",
                    ),
                    HttpStatus::BadRequest,
                ));
            };
            if !caps_val.is_object() {
                return Some(self.protocol_failure(
                    &id,
                    &incoming,
                    name,
                    started,
                    ErrorObject::new(
                        ErrorObject::INVALID_PARAMS,
                        "`io.modelcontextprotocol/clientCapabilities` must be a JSON object",
                    ),
                    HttpStatus::BadRequest,
                ));
            }
        } else {
            // Mode legacy : check_protocol_version tolère l'absence mais refuse une version non supportée
            if let Some(err) = self.check_protocol_version(params) {
                return Some(self.protocol_failure(
                    &id,
                    &incoming,
                    name,
                    started,
                    err,
                    HttpStatus::BadRequest,
                ));
            }
        }

        let is_legacy = self.protocol_profile == ProtocolProfile::Legacy;
        // Cycle MRTR (multi round-trip, spec 2026-07-28) : `requestState` et
        // `inputResponses` sont vérifiés AVANT tout handler — uniquement sur
        // les trois méthodes supportées par la spec (`tools/call`,
        // `prompts/get`, `resources/read`) ; sur toute autre méthode, les
        // champs sont ignorés (le serveur ne répond jamais `input_required`
        // ailleurs et ne dépend d'aucun état de connexion). Un état qui
        // échoue à la vérification est refusé en `-32602` sans atteindre le
        // handler.
        let mrtr_round = if matches!(
            incoming.method.as_str(),
            protocol::method::TOOLS_CALL
                | protocol::method::RESOURCES_READ
                | protocol::method::PROMPTS_GET
        ) {
            match self.verify_mrtr(&id, &incoming.method, params) {
                Ok(round) => round,
                Err(error) => {
                    return Some(self.protocol_failure(
                        &id,
                        &incoming,
                        name,
                        started,
                        error,
                        HttpStatus::BadRequest,
                    ));
                }
            }
        } else {
            InputRound::first()
        };
        // Plafond de réponse : passé aux sérialisations bornées (le writer
        // cesse de stocker au-delà — voir `BoundedReply`), le contrôle
        // historique sur la taille totale reste le seul verdict.
        let cap = self.max_response_bytes;
        let (mut reply, mut status) = match incoming.method.as_str() {
            protocol::method::DISCOVER => (
                reponse_pre_serialisee(&id, self.discover_octets(), cap),
                CallStatus::Success,
            ),
            protocol::method::INITIALIZE if is_legacy => (
                Outgoing::success_bounded(&id, self.initialize_json(params), cap),
                CallStatus::Success,
            ),
            protocol::method::PING if is_legacy => (
                Outgoing::success_bounded(&id, self.ping_json(), cap),
                CallStatus::Success,
            ),
            protocol::method::TOOLS_LIST => (
                reponse_pre_serialisee(&id, &self.charge_outils(context.as_ref()), cap),
                CallStatus::Success,
            ),
            protocol::method::TOOLS_CALL => self.call_tool(&id, params, &mrtr_round),
            protocol::method::RESOURCES_LIST => (
                reponse_pre_serialisee(&id, &self.charge_ressources(context.as_ref()), cap),
                CallStatus::Success,
            ),
            protocol::method::RESOURCES_TEMPLATES_LIST => (
                reponse_pre_serialisee(&id, &self.charge_gabarits(context.as_ref()), cap),
                CallStatus::Success,
            ),
            protocol::method::RESOURCES_READ => self.read_resource(&id, params, &mrtr_round),
            protocol::method::PROMPTS_LIST => (
                reponse_pre_serialisee(&id, &self.charge_prompts(context.as_ref()), cap),
                CallStatus::Success,
            ),
            protocol::method::PROMPTS_GET => self.get_prompt(&id, params, &mrtr_round),
            protocol::method::COMPLETION_COMPLETE => self.complete(&id, params),
            other => {
                // Page *Versioning* (`2026-07-28`) : « A server that supports
                // only modern versions SHOULD name the protocol versions it
                // supports in any error it returns to an `initialize`
                // request ». Ce cas n'existe qu'en profil strict — en profil
                // legacy, `initialize` est servi plus haut (`INITIALIZE if
                // is_legacy`) et réussit. Les autres méthodes inconnues
                // restent sans `data` (même convention que le `-32022`, qui
                // ne porte `supported` que pour une version refusée).
                let error = if other == protocol::method::INITIALIZE {
                    ErrorObject::with_data(
                        ErrorObject::METHOD_NOT_FOUND,
                        format!("Unknown method: {}", sanitize_echo(other)),
                        json!({ "supported": protocol::SUPPORTED_VERSIONS }),
                    )
                } else {
                    ErrorObject::new(
                        ErrorObject::METHOD_NOT_FOUND,
                        format!("Unknown method: {}", sanitize_echo(other)),
                    )
                };
                (
                    Outgoing::failure(id.clone(), error).to_bounded(cap),
                    CallStatus::ProtocolError(ErrorObject::METHOD_NOT_FOUND),
                )
            }
        };
        if self.max_response_bytes > 0 && reply.len() > self.max_response_bytes {
            let actual = reply.len();
            reply = BoundedReply::Complete(
                Outgoing::failure(
                    id,
                    ErrorObject::new(
                        ErrorObject::INTERNAL_ERROR,
                        format!(
                            "response size ({actual} bytes) exceeds maximum ({} bytes)",
                            self.max_response_bytes
                        ),
                    ),
                )
                .to_vec(),
            );
            status = CallStatus::ProtocolError(ErrorObject::INTERNAL_ERROR);
        }
        self.emit_call(&incoming.method, name, started.elapsed(), status);

        let http_status = match status {
            CallStatus::Success | CallStatus::ToolFailure(_) | CallStatus::Notification => {
                HttpStatus::Ok
            }
            CallStatus::ProtocolError(code) => HttpStatus::from_error_code(code),
        };

        Some(DispatchResponse {
            body: reply.into_vec(),
            status: http_status,
        })
    }

    /// Construit la réponse d'erreur de protocole **et** émet l'événement
    /// d'observation correspondant — les sorties anticipées de
    /// [`ServerCore::dispatch_request`] passent toutes ici pour que
    /// chaque message produise exactement un événement.
    pub(super) fn protocol_failure(
        &self,
        id: &Value,
        incoming: &crate::jsonrpc::Incoming,
        name: Option<&str>,
        started: Instant,
        error: ErrorObject,
        http_status: HttpStatus,
    ) -> DispatchResponse {
        let code = error.code;
        let outgoing = Outgoing::failure(id.clone(), error);
        self.emit_call(
            &incoming.method,
            name,
            started.elapsed(),
            CallStatus::ProtocolError(code),
        );
        DispatchResponse {
            body: outgoing.to_vec(),
            status: http_status,
        }
    }
}

#[cfg(test)]
mod tests_compteur_inflight_2026_11_09 {
    use super::*;
    use crate::gateway::Gateway;

    /// Identité du compteur d'inflight — le contrat d'isolation du
    /// partage : UN compteur par gateway (tous ses snapshots), un compteur
    /// propre par noyau indépendant. Sans partage accidentel entre deux
    /// gateways ni entre deux serveurs indépendants.

    #[test]
    fn deux_noyaux_independants_deux_compteurs() {
        let builder = ServerBuilder::new("iso", "1.0.0");
        let c1 = builder.core();
        let c2 = builder.core();
        assert!(
            !Arc::ptr_eq(&c1.inflight_handlers, &c2.inflight_handlers),
            "deux `core()` du même builder doivent rester indépendants"
        );
    }

    #[test]
    fn builder_clone_serveurs_independants_compteurs_independants() {
        let b1 = ServerBuilder::new("s1", "1.0.0");
        let b2 = b1.clone();
        let c1 = b1.core();
        let c2 = b2.core();
        assert!(
            !Arc::ptr_eq(&c1.inflight_handlers, &c2.inflight_handlers),
            "cloner un builder pour deux serveurs ne doit pas partager le compteur"
        );
    }

    #[test]
    fn deux_gateways_deux_compteurs() {
        let g1 = Gateway::new(ServerBuilder::new("g1", "1.0.0"));
        let g2 = Gateway::new(ServerBuilder::new("g2", "1.0.0"));
        assert!(
            !Arc::ptr_eq(
                &g1.snapshot().inflight_handlers,
                &g2.snapshot().inflight_handlers
            ),
            "deux gateways ne doivent jamais partager leur compteur"
        );
    }

    #[test]
    fn un_gateway_un_compteur_partage_tous_ses_snapshots() {
        let gw = Gateway::new(ServerBuilder::new("g", "1.0.0").tool("t", "d", |_a| Ok(json!({}))));
        let initial = gw.snapshot();
        gw.edit(|b| b.tool("t2", "d", |_a| Ok(json!({}))));
        let apres_edit1 = gw.snapshot();
        gw.edit(|b| b);
        let apres_edit2 = gw.snapshot();
        assert!(
            Arc::ptr_eq(&initial.inflight_handlers, &apres_edit1.inflight_handlers),
            "l'edit doit conserver le compteur du snapshot initial"
        );
        assert!(
            Arc::ptr_eq(&initial.inflight_handlers, &apres_edit2.inflight_handlers),
            "chaque edit doit partager le MÊME compteur (anciens détachés comptés)"
        );
    }
}

#[cfg(test)]
mod tests_codes_erreurs_2026_11_10 {
    use super::*;

    /// La surcharge (`run_bounded` → `Outcome::Overloaded`) répond `-32603`
    /// avec `data` structuré — plus la plage legacy `-32000` (MCP 2026-07-28
    /// déconseille d'introduire de nouveaux codes dedans). Le compteur est
    /// forcé au plafond : le handler n'est jamais exécuté, aucun thread
    /// spawné — le test reste instantané.
    #[test]
    fn surcharge_ressource_repond_32603_avec_data() {
        let core = ServerBuilder::new("sat", "1.0.0")
            .tool_timeout(Duration::from_millis(50))
            .resource("mem://x", "X", "jamais exécuté", || {
                unreachable!("la surcharge doit court-circuiter le handler")
            })
            .core();
        core.inflight_handlers.store(
            core.max_inflight_handlers,
            std::sync::atomic::Ordering::Release,
        );
        let reply = core
            .dispatch_unchecked(
                br#"{"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":"mem://x"}}"#,
            )
            .expect("resources/read doit répondre");
        let v: Value = serde_json::from_slice(&reply).unwrap();
        assert_eq!(v["error"]["code"], -32603, "{v}");
        assert_eq!(v["error"]["data"]["reason"], "overloaded", "{v}");
        assert_eq!(
            v["error"]["data"]["max_in_flight"], MAX_INFLIGHT_HANDLERS as u64,
            "{v}"
        );
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("too many concurrent handlers in flight"),
            "{v}"
        );
    }
}
