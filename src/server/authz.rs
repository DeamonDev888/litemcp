//! Autorisation : droits par élément, gabarits couvrants, refus et
//! journal des décisions.

use super::handlers::sanitize_echo;
use super::*;

/// Message **anglais** rendu au client pour un refus — il ne dit jamais
/// *quel* droit manque ni pourquoi la confirmation a refusé : le motif précis
/// reste côté application (hook d'audit).
fn forbidden_message(wire_reason: &str) -> &'static str {
    match wire_reason {
        "approval_pending" => "Forbidden: approval pending",
        "approval_denied" => "Forbidden: approval denied",
        "unauthenticated" => "Unauthorized: no verified identity",
        _ => "Forbidden: a required right is missing",
    }
}

/// L'appelant détient-il **tous** les droits exigés ?
///
/// **Échec fermé** : un élément sans droit déclaré (`required` vide) est
/// refusé dès que l'autorisation est active — c'est la règle de la
/// spécification (§4), et l'inverse (un `all()` sur un ensemble vide) serait
/// une porte ouverte.
pub(super) fn rights_held(context: &auth::AuthContext, required: &[String]) -> bool {
    !required.is_empty() && required.iter().all(|right| context.has_right(right))
}

/// Le gabarit couvre-t-il cette URI concrète ?
///
/// Deux chemins, choisis par la forme du motif :
///
/// - **motif simple** (`file:///x/{p}`, `file:///pub/~{y}`, ou plusieurs
///   variables séparées par des littéraux non vides) : règle historique,
///   segment par segment — une variable `{nom}` (nom ASCII alphanumérique ou
///   `_`) couvre un segment **non vide** qui ne contient pas `/` (développement
///   simple RFC 6570 §3.2.2) ;
/// - **tout autre motif** (opérateur `{+x}`, `{?x}`, `{/x}`…, liste `{a,b}`,
///   expressions accolées, expression non fermée) : couverture
///   **conservatrice** ([`conservative_template_covers`]) — préfixe littéral,
///   suffixe littéral, littéraux intermédiaires dans l'ordre, chaque expression
///   valant « n'importe quelle chaîne ». Elle ne peut que **sur**-couvrir : une
///   famille protégée par un tel motif reste protégée (échec fermé), au prix
///   d'URI voisines protégées par excès.
fn template_covers(pattern: &str, uri: &str) -> bool {
    if is_simple_template(pattern) {
        simple_template_covers(pattern, uri)
    } else {
        conservative_template_covers(pattern, uri)
    }
}

/// Le motif est-il de forme simple — toutes les expressions sont des variables
/// `{nom}`, séparées par des littéraux **non vides** (préfixe et suffixe
/// pouvant être vides) ? C'est la seule forme que la règle segment par segment
/// sait lire sans ambiguïté.
fn is_simple_template(pattern: &str) -> bool {
    let mut cursor = 0usize;
    let mut first = true;
    loop {
        let Some(open) = pattern[cursor..].find('{') else {
            return true;
        };
        let start = cursor + open;
        // Deux expressions accolées (`{x}{y}`) : la règle simple ne sait pas
        // les développer, le motif passe par la couverture conservatrice.
        if !first && start == cursor {
            return false;
        }
        let Some(close) = pattern[start + 1..].find('}') else {
            return false;
        };
        let name = &pattern[start + 1..start + 1 + close];
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return false;
        }
        first = false;
        cursor = start + 1 + close + 1;
    }
}

/// Motif simple : littéraux comparés octet par octet, une variable `{nom}`
/// couvrant un segment **non vide** sans `/`.
fn simple_template_covers(pattern: &str, uri: &str) -> bool {
    let mut rest = uri;
    let mut cursor = 0usize;
    loop {
        let Some(open) = pattern[cursor..].find('{') else {
            return rest == &pattern[cursor..];
        };
        let literal = &pattern[cursor..cursor + open];
        let Some(stripped) = rest.strip_prefix(literal) else {
            return false;
        };
        rest = stripped;
        let Some(close) = pattern[cursor + open..].find('}') else {
            return false;
        };
        let name = &pattern[cursor + open + 1..cursor + open + close];
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return false;
        }
        let after = cursor + open + close + 1;
        let next_literal = match pattern[after..].find('{') {
            Some(next) => &pattern[after..after + next],
            None => &pattern[after..],
        };
        // La variable s'arrête au prochain `/` ; sans littéral suivant elle
        // doit couvrir tout le reste.
        let span = rest.find('/').unwrap_or(rest.len());
        if next_literal.is_empty() {
            if span == 0 || span != rest.len() {
                return false;
            }
            rest = "";
        } else {
            // Bornes de caractères uniquement : une URI non-ASCII vient du
            // réseau et ne doit jamais faire paniquer une découpe d'octets.
            let mut chosen = None;
            for (index, _) in rest.char_indices().skip(1) {
                if index > span {
                    break;
                }
                if rest[index..].starts_with(next_literal) {
                    chosen = Some(index);
                    break;
                }
            }
            let Some(len) = chosen else {
                return false;
            };
            rest = &rest[len..];
        }
        cursor = after;
    }
}

/// Couverture **conservatrice** d'un motif non simple (`{+x}`, `{?x}`,
/// `{a,b}`, expressions accolées ou non fermées).
///
/// Chaque expression est traitée comme si elle pouvait développer **n'importe
/// quelle chaîne** (y compris vide, `/` et `?` compris), ce qui ne peut que
/// sur-couvrir : préfixe littéral, suffixe littéral, puis littéraux
/// intermédiaires cherchés dans l'ordre. Une famille protégée par un motif
/// exotique reste donc protégée — l'inverse (aucune URI couverte, donc aucun
/// droit hérité) serait une porte ouverte.
///
/// Toutes les découpes passent par `str::find`/`strip_prefix`/`ends_with`,
/// jamais par un index d'octet calculé : une URI vient du réseau et peut être
/// non-ASCII sans faire paniquer la fonction.
fn conservative_template_covers(pattern: &str, uri: &str) -> bool {
    // Littéraux séparés par les expressions ; `last` éléments = préfixe,
    // intermédiaires, suffixe.
    let mut literals: Vec<&str> = Vec::new();
    let mut cursor = 0usize;
    loop {
        let Some(open) = pattern[cursor..].find('{') else {
            literals.push(&pattern[cursor..]);
            break;
        };
        let start = cursor + open;
        literals.push(&pattern[cursor..start]);
        let Some(close) = pattern[start + 1..].find('}') else {
            // Expression non fermée : elle couvre tout le reste, donc le
            // suffixe littéral est vide (sur-couverture assumée).
            literals.push("");
            break;
        };
        cursor = start + 1 + close + 1;
    }
    let last = literals.len().saturating_sub(1);
    if last == 0 {
        // Aucune expression : égalité littérale.
        return uri == pattern;
    }
    let Some(rest) = uri.strip_prefix(literals[0]) else {
        return false;
    };
    let tail = literals[last];
    if !rest.ends_with(tail) {
        return false;
    }
    // `ends_with` garantit une frontière de caractère : la découpe est sûre.
    let mut middle = &rest[..rest.len() - tail.len()];
    for literal in &literals[1..last] {
        let Some(index) = middle.find(literal) else {
            return false;
        };
        middle = &middle[index + literal.len()..];
    }
    true
}

/// Méthode et élément nommé d'un corps JSON-RPC, pour le **journal** d'un
/// refus d'identité (chemin froid : le corps n'est lu que sur un refus).
///
/// Best-effort : un corps illisible ne donne ni méthode ni nom, et les
/// en-têtes `Mcp-Method`/`Mcp-Name` servent alors de repli (ils ne décident
/// de rien, ils nomment l'événement).
pub(crate) fn audit_target(body: &[u8], headers: &McpHeaders) -> (String, Option<String>) {
    let parsed = serde_json::from_slice::<Value>(body).ok();
    let method = parsed
        .as_ref()
        .and_then(|value| value.get("method"))
        .and_then(Value::as_str)
        .or(headers.method.as_deref())
        .unwrap_or_default()
        .to_string();
    let name = parsed
        .as_ref()
        .and_then(|value| value.get("params"))
        .and_then(|params| {
            params
                .get("name")
                .or_else(|| params.get("uri"))
                .and_then(Value::as_str)
        })
        .or(headers.name.as_deref())
        .map(str::to_string);
    (method, name)
}

/// Ajoute `right` à `rights` s'il n'y est pas déjà : les droits d'un élément
/// forment un ensemble, l'ordre d'appel est indifférent.
pub(super) fn push_right(rights: &mut Vec<String>, right: &str) {
    if !rights.iter().any(|existing| existing == right) {
        rights.push(right.to_string());
    }
}

/// Applique les droits **déclarés** aux définitions d'un catalogue : la table
/// déclarée fait foi (`.require_*_right` peut précéder l'enregistrement de
/// l'élément).
pub(super) fn apply_rights<'a, T: 'a>(
    declared: &BTreeMap<String, Vec<String>>,
    items: impl Iterator<Item = (&'a String, &'a mut T)>,
    rights_of: impl Fn(&mut T) -> &mut Vec<String>,
) {
    for (key, def) in items {
        if let Some(rights) = declared.get(key) {
            let slot = rights_of(def);
            slot.clear();
            slot.extend(rights.iter().cloned());
        }
    }
}

/// La méthode désigne-t-elle **un élément nommé** soumis aux droits ?
pub(super) fn is_element_method(method: &str) -> bool {
    matches!(
        method,
        protocol::method::TOOLS_CALL
            | protocol::method::RESOURCES_READ
            | protocol::method::PROMPTS_GET
    )
}

/// Erreur JSON-RPC d'un refus d'autorisation : un seul code (`-32023`),
/// `data.reason` porte la distinction — et rien d'autre (ni droit manquant,
/// ni motif interne).
pub(super) fn forbidden_error(refusal: &AuthRefusal) -> ErrorObject {
    let mut data = serde_json::Map::new();
    data.insert("reason".to_string(), Value::from(refusal.wire_reason));
    // `approval_pending` est le seul motif **retentable** : rien n'est
    // exécuté ni mis en file, le client réessaie (même corps, nouvel `id`).
    if refusal.wire_reason == "approval_pending" {
        data.insert("retryable".to_string(), Value::Bool(true));
        if let Some(delay) = refusal.retry_after {
            data.insert(
                "retryAfterMs".to_string(),
                Value::from(u64::try_from(delay.as_millis()).unwrap_or(u64::MAX)),
            );
        }
    }
    ErrorObject::with_data(
        ErrorObject::FORBIDDEN,
        forbidden_message(refusal.wire_reason),
        Value::Object(data),
    )
}

impl ServerCore {
    /// Droits exigés par une URI de ressource : **déclaration exacte**
    /// d'abord, sinon l'**union** des droits de *tous* les gabarits qui
    /// couvrent l'URI, sinon rien.
    ///
    /// Une URI déclarée exactement (ressource enregistrée ou clé de gabarit)
    /// ne dépend que de ses propres droits : un gabarit plus large ne la
    /// restreint pas (pas de sur-restriction). Pour une URI couverte par
    /// plusieurs gabarits, **tous** doivent être détenus : retenir le premier
    /// couvrant dans l'ordre des clés laissait passer une URI protégée par un
    /// gabarit étroit avec le seul droit du gabarit large.
    fn resource_rights(&self, uri: &str) -> Option<Cow<'_, [String]>> {
        if let Some(def) = self.resources.get(uri) {
            return Some(Cow::Borrowed(def.required_rights.as_slice()));
        }
        if let Some(def) = self.templates.get(uri) {
            return Some(Cow::Borrowed(def.required_rights.as_slice()));
        }
        let mut required: Vec<String> = Vec::new();
        let mut covered = false;
        for def in self.templates.values() {
            if !template_covers(&def.template.uri_template, uri) {
                continue;
            }
            covered = true;
            // Un gabarit couvrant sans droit déclaré laisse l'union vide :
            // l'URI est refusée, comme l'élément lui-même (échec fermé).
            if def.required_rights.is_empty() {
                required.clear();
                break;
            }
            for right in &def.required_rights {
                push_right(&mut required, right);
            }
        }
        covered.then_some(Cow::Owned(required))
    }

    /// Droits exigés par la référence d'une complétion, **si l'élément
    /// existe** — `None` sinon (élément inconnu : le fournisseur tranche,
    /// l'existence des noms n'est pas un secret, `docs/auth.md` §4).
    ///
    /// Un gabarit de ressource est reconnu par sa clé `uriTemplate`
    /// ([`ServerBuilder::require_resource_right`]) comme par son URI exacte,
    /// et couvre les URI concrètes de sa famille.
    pub(super) fn completion_rights(
        &self,
        reference: &CompleteReference,
    ) -> Option<Cow<'_, [String]>> {
        match reference {
            CompleteReference::Prompt { name, .. } => self
                .prompts
                .get(name)
                .map(|def| Cow::Borrowed(def.required_rights.as_slice())),
            CompleteReference::Resource { uri } => self.resource_rights(uri),
        }
    }

    /// Droits exigés par l'élément nommé de cette méthode, s'il **existe**.
    ///
    /// `None` = méthode sans élément nommé **ou** élément inconnu : dans les
    /// deux cas il n'y a pas de décision d'autorisation à rendre ici, et
    /// l'élément inconnu reste `-32602` (l'existence des noms n'est pas un
    /// secret).
    pub(super) fn element_rights(&self, method: &str, params: &Value) -> Option<Cow<'_, [String]>> {
        match method {
            protocol::method::TOOLS_CALL => params
                .get("name")
                .and_then(Value::as_str)
                .and_then(|name| self.tools.get(name))
                .map(|def| Cow::Borrowed(def.required_rights.as_slice())),
            protocol::method::RESOURCES_READ => params
                .get("uri")
                .and_then(Value::as_str)
                .and_then(|uri| self.resource_rights(uri)),
            protocol::method::PROMPTS_GET => params
                .get("name")
                .and_then(Value::as_str)
                .and_then(|name| self.prompts.get(name))
                .map(|def| Cow::Borrowed(def.required_rights.as_slice())),
            _ => None,
        }
    }

    /// Réponse d'un refus d'autorisation : événement d'audit, événement
    /// d'observation, corps `-32023` et statut `403`.
    pub(super) fn auth_failure(
        &self,
        id: &Value,
        incoming: &crate::jsonrpc::Incoming,
        name: Option<&str>,
        started: Instant,
        refusal: AuthRefusal,
        client: Option<&str>,
    ) -> DispatchResponse {
        self.emit_auth_decision(
            &incoming.method,
            name,
            client,
            refusal.verdict,
            Some(refusal.reason),
        );
        self.protocol_failure(
            id,
            incoming,
            name,
            started,
            forbidden_error(&refusal),
            HttpStatus::Forbidden,
        )
    }

    /// Publie une décision d'autorisation au journal de l'application —
    /// no-op sans autorisation configurée ni hook enregistré.
    ///
    /// Une panique de hook est attrapée et ignorée : l'audit ne doit jamais
    /// interrompre le service (même contrat que [`Self::emit_call`]).
    pub(super) fn emit_auth_decision(
        &self,
        method: &str,
        name: Option<&str>,
        client: Option<&str>,
        verdict: auth::AuthVerdict,
        reason: Option<&str>,
    ) {
        let Some(gate) = self.auth_gate.as_ref() else {
            return;
        };
        if gate.decision_hooks.is_empty() {
            return;
        }
        let event = auth::AuthDecisionEvent {
            method,
            name,
            client,
            verdict,
            reason,
        };
        for hook in &gate.decision_hooks {
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| hook(&event)));
        }
    }

    /// Notifie les hooks d'observation — no-op sans hook enregistré.
    ///
    /// Une panique de hook est attrapée et ignorée : l'observation ne doit
    /// jamais interrompre le service (le hook défaillant est silencieusement
    /// passé pour CET événement, il sera rappelé au suivant).
    pub(super) fn emit_call(
        &self,
        method: &str,
        name: Option<&str>,
        duration: Duration,
        status: CallStatus,
    ) {
        if self.hooks.is_empty() {
            return;
        }
        let event = CallEvent {
            method,
            name,
            duration,
            status,
        };
        for hook in self.hooks.iter() {
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| hook(&event)));
        }
    }

    /// Vérifie la version de protocole annoncée dans `_meta` (SEP-2575).
    ///
    /// Appelée en profil legacy (le profil strict exige `_meta` et valide la
    /// version avant d'arriver ici) : l'absence de version y est tolérée
    /// (client legacy). Une version explicite non supportée est refusée en
    /// `-32022` (`UnsupportedProtocolVersionError`) avec la liste des
    /// versions supportées.
    pub(super) fn check_protocol_version(&self, params: &Value) -> Option<ErrorObject> {
        let requested = params
            .get("_meta")
            .and_then(|meta| meta.get(protocol::META_PROTOCOL_VERSION))
            .and_then(Value::as_str)?;
        if protocol::SUPPORTED_VERSIONS.contains(&requested) {
            return None;
        }
        let safe = sanitize_echo(requested);
        Some(ErrorObject::with_data(
            ErrorObject::UNSUPPORTED_PROTOCOL_VERSION,
            format!("Unsupported protocol version: {safe}"),
            json!({
                "supported": protocol::SUPPORTED_VERSIONS,
                "requested": safe,
            }),
        ))
    }
}
