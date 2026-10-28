//! Schémas à la Zod — dérivation JSON Schema + validation runtime.
//!
//! L'API calque [Zod](https://zod.dev), le standard de l'écosystème MCP
//! TypeScript — **il n'existe pas de Zod officiel en Rust** (Zod est une
//! bibliothèque TypeScript-first) : ce module en est l'implémentation maison
//! pour Rust, zéro dépendance. On déclare un schéma, la bibliothèque en
//! dérive le `inputSchema` MCP **et** valide chaque `tools/call` avant
//! d'exécuter le handler. Un appel hors schéma ne touche jamais votre code —
//! il reçoit une erreur JSON-RPC `-32602` portant la liste des problèmes
//! (`error.data.issues`), l'équivalent d'une `ZodError`.
//!
//! Le schéma publié est **exécuteur** : ce qu'il annonce est exactement ce
//! que la validation applique — y compris les formats (`format` est une
//! simple annotation en JSON Schema 2020-12 ; ici il est **appliqué**).
//!
//! Les **valeurs par défaut** ([`Defaulted`]) sont publiées **et**
//! appliquées : un champ absent dont le schéma porte un défaut est valide,
//! et la valeur est injectée dans les arguments avant l'exécution.
//!
//! [`record`] valide les **valeurs** d'un objet (dictionnaire) et
//! [`integer_enumeration`] publie une énumération d'entiers.
//!
//! Les problèmes sont accumulés dans un collecteur borné ([`Issues`]) :
//! au-delà de [`MAX_ISSUES`], ils sont comptés sans être stockés — un
//! tableau géant d'éléments invalides ne peut pas faire grossir la mémoire
//! indéfiniment. Le chemin chaud n'alloue qu'une fois par requête (la
//! racine du chemin) : le chemin de validation ([`Path`]) est un buffer
//! unique, étendu puis détronqué, et une chaîne n'est construite qu'au
//! moment d'émettre une issue.
//!
//! ```no_run
//! use litemcp::prelude::*;
//!
//! ServerBuilder::new("calc", "1.0.0")
//!     .tool_typed(
//!         "add",
//!         "Additionne deux entiers",
//!         zod::object()
//!             .field("a", zod::integer())
//!             .field("b", zod::integer()),
//!         |args| {
//!             // Le schéma garantit la présence et le type : `as_i64()` est
//!             // toujours `Some`. Le `?` reste recommandé par prudence
//!             // (il n'est jamais déclenché).
//!             let a = args["a"].as_i64().ok_or("a manquant")?;
//!             let b = args["b"].as_i64().ok_or("b manquant")?;
//!             Ok(serde_json::json!({ "somme": a + b }))
//!         },
//!     )
//!     .bind("127.0.0.1:23080")?
//!     .serve()?;
//! # Ok::<(), std::io::Error>(())
//! ```

use serde_json::{Value, json};
use std::fmt::Write as _;
use std::sync::Arc;

/// Plafond d'issues stockées par une validation ([`Issues`]) — au-delà, les
/// problèmes sont comptés sans être stockés.
pub const MAX_ISSUES: usize = 64;

/// Profondeur maximale d'imbrication acceptée lors de la validation ou de
/// l'injection de défauts — protège la pile d'exécution contre un arbre
/// `serde_json::Value` profondément imbriqué construit en mémoire.
pub const MAX_DEPTH: usize = 128;

/// Collecteur d'issues de validation, **borné en mémoire**.
///
/// Un tableau de centaines de milliers d'éléments invalides ne doit pas
/// pouvoir faire grossir la liste indéfiniment : au-delà de [`MAX_ISSUES`],
/// les problèmes sont comptés sans être stockés, et le rendu final
/// ([`Issues::render`]) signale combien ont été omis.
#[derive(Debug, Default, Clone)]
pub struct Issues {
    items: Vec<String>,
    overflow: usize,
}

impl Issues {
    /// Collecteur vide.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Le plafond [`MAX_ISSUES`] est-il atteint ?
    ///
    /// Permet d'éviter de formater et d'allouer le message d'erreur quand il
    /// ne sera plus stocké (seul le compteur d'omissions est incrémenté via
    /// [`Issues::record_overflow`]).
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.items.len() >= MAX_ISSUES
    }

    /// Comptabilise une issue supplémentaire au-delà de [`MAX_ISSUES`] sans
    /// allouer de chaîne.
    pub fn record_overflow(&mut self) {
        self.overflow = self.overflow.saturating_add(1);
    }

    /// Enregistre une issue — au-delà de [`MAX_ISSUES`], elle est seulement
    /// comptée.
    pub fn push(&mut self, issue: impl Into<String>) {
        if self.items.len() < MAX_ISSUES {
            self.items.push(issue.into());
        } else {
            self.overflow = self.overflow.saturating_add(1);
        }
    }

    /// Au moins un problème ?
    pub fn is_empty(&self) -> bool {
        self.items.is_empty() && self.overflow == 0
    }

    /// Issues rendues : les premières stockées, plus — s'il y en a — un
    /// récapitulatif des problèmes omis.
    pub fn render(&self) -> Vec<String> {
        let mut out = self.items.clone();
        if self.overflow > 0 {
            out.push(format!(
                "… and {overflow} more issue(s) omitted",
                overflow = self.overflow
            ));
        }
        out
    }
}

/// Émet une issue dans `$issues` sans évaluer `format!` si le plafond
/// [`MAX_ISSUES`] est déjà atteint (zéro allocation au-delà de 64 erreurs).
macro_rules! push_issue {
    ($issues:expr, $($arg:tt)*) => {
        if $issues.is_full() {
            $issues.record_overflow();
        } else {
            $issues.push(format!($($arg)*));
        }
    };
}

/// Chemin de validation, **réutilisé par un buffer unique**.
///
/// Le chemin chaud (tout est valide) n'alloue qu'une fois — la racine :
/// les segments sont ensuite poussés dans le buffer puis détronqués à la
/// marque précédente, et une chaîne n'est matérialisée qu'au moment
/// d'émettre une issue ([`Path::as_str`]). Avant cette correction, chaque
/// champ et chaque élément de tableau allouait son chemin même quand la
/// validation réussissait (12 allocations pour 2 champs + 10 éléments).
pub struct Path {
    buf: String,
    depth: usize,
}

impl Path {
    /// Chemin racine (ex. `arguments`).
    ///
    /// Pré-alloue une marge de 32 octets pour les segments de champs et
    /// d'indices afin que le premier [`Path::push_field`] ne réalloue pas.
    #[must_use]
    pub fn root(root: &str) -> Self {
        let mut buf = String::with_capacity(root.len() + 32);
        buf.push_str(root);
        Self { buf, depth: 0 }
    }

    /// Marque de longueur courante — à passer à [`Path::truncate`] pour
    /// dépiler le dernier segment.
    #[must_use]
    pub fn mark(&self) -> usize {
        self.buf.len()
    }

    /// Empile un segment de champ : `arguments` → `arguments.nom`.
    pub fn push_field(&mut self, name: &str) {
        self.buf.push('.');
        self.buf.push_str(name);
    }

    /// Empile un index de tableau : `arguments.list` → `arguments.list[3]`.
    pub fn push_index(&mut self, index: usize) {
        let _ = write!(self.buf, "[{index}]");
    }

    /// Dépile jusqu'à la marque ([`Path::mark`]).
    pub fn truncate(&mut self, mark: usize) {
        self.buf.truncate(mark);
    }

    /// Chemin courant — pour construire un message d'issue.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.buf
    }
}

/// Contrat commun de tous les schémas : rendu JSON Schema + validation.
///
/// La validation reçoit un [`Path`] réutilisé (zéro allocation sur le
/// chemin nominal) et accumule les problèmes lisibles dans un collecteur
/// borné ([`Issues`]).
pub trait Schema: Send + Sync {
    /// Fragment JSON Schema (draft 2020-12) correspondant.
    fn json(&self) -> Value;
    /// Valide `value` au chemin `path` et accumule les problèmes dans
    /// `issues`.
    fn validate(&self, path: &mut Path, value: &Value, issues: &mut Issues);

    /// Valeur par défaut de ce schéma, s'il en a une.
    ///
    /// Le défaut est **publié** dans le JSON Schema (mot-clé `default`)
    /// **et appliqué** : un champ absent dont le schéma porte un défaut
    /// est considéré valide, et la valeur par défaut est **injectée** dans
    /// les arguments avant l'exécution du handler (comme le fait Zod).
    fn default_value(&self) -> Option<&Value> {
        None
    }

    /// Injecte les valeurs par défaut manquantes, **récursivement**.
    ///
    /// Appelé sur la racine des arguments : pour chaque champ absent
    /// dont le schéma porte un défaut, la valeur est insérée ; les
    /// valeurs présentes sont parcourues (un objet imbriqué peut porter
    /// ses propres défauts).
    fn apply_defaults(&self, _value: &mut Value) {}

    /// Ce schéma (ou l'un de ses descendants) porte-t-il un défaut ?
    ///
    /// Permet d'éviter toute copie des arguments quand aucune valeur
    /// par défaut n'est à injecter.
    fn has_defaults(&self) -> bool {
        false
    }
}

/// Un schéma encapsulé (`Box`, `Arc`) est un schéma.
///
/// Sans ces implémentations, impossible de stocker des schémas
/// **hétérogènes** (usage dynamique : schémas issus d'une configuration,
/// d'une base, d'un registre…) — `Box<dyn Schema>` ne satisfait pas
/// `S: Schema` et il fallait un wrapper manuel.
impl<S: Schema + ?Sized> Schema for Box<S> {
    fn json(&self) -> Value {
        (**self).json()
    }
    fn validate(&self, path: &mut Path, value: &Value, issues: &mut Issues) {
        (**self).validate(path, value, issues);
    }
    fn default_value(&self) -> Option<&Value> {
        (**self).default_value()
    }
    fn apply_defaults(&self, value: &mut Value) {
        (**self).apply_defaults(value);
    }
    fn has_defaults(&self) -> bool {
        (**self).has_defaults()
    }
}

/// Idem [`Box`] — un schéma partagé (`Arc<dyn Schema>`) reste un schéma.
impl<S: Schema + ?Sized> Schema for Arc<S> {
    fn json(&self) -> Value {
        (**self).json()
    }
    fn validate(&self, path: &mut Path, value: &Value, issues: &mut Issues) {
        (**self).validate(path, value, issues);
    }
    fn default_value(&self) -> Option<&Value> {
        (**self).default_value()
    }
    fn apply_defaults(&self, value: &mut Value) {
        (**self).apply_defaults(value);
    }
    fn has_defaults(&self) -> bool {
        (**self).has_defaults()
    }
}

/// Une **référence** vers un schéma est un schéma — indispensable pour
/// passer `&dyn Schema` aux API qui attendent `impl Schema` sans
/// consommer le schéma.
impl<S: Schema + ?Sized> Schema for &S {
    fn json(&self) -> Value {
        (**self).json()
    }
    fn validate(&self, path: &mut Path, value: &Value, issues: &mut Issues) {
        (**self).validate(path, value, issues);
    }
    fn default_value(&self) -> Option<&Value> {
        (**self).default_value()
    }
    fn apply_defaults(&self, value: &mut Value) {
        (**self).apply_defaults(value);
    }
    fn has_defaults(&self) -> bool {
        (**self).has_defaults()
    }
}

/// Vérifie un couple de bornes cohérent — erreur de programmation, panique
/// au démarrage (même contrat que le doublon de champ) : un schéma
/// insatisfaisable (`min > max`) serait publié en silence et toute valeur
/// échouerait avec deux messages contradictoires.
fn assert_ordered<T: PartialOrd + std::fmt::Debug>(min: &T, max: &T, quoi: &str) {
    assert!(
        min <= max,
        "litemcp/zod : bornes incohérentes ({min:?} > {max:?}) sur {quoi} — \
         le schéma serait insatisfaisable"
    );
}

/// Un schéma porteur d'une **valeur par défaut**.
///
/// Le défaut est **publié** (mot-clé `default` du JSON Schema) **et
/// appliqué** : un champ absent portant un défaut est valide, et la
/// valeur est **injectée** dans les arguments avant l'exécution du
/// handler — exactement comme le fait Zod.
///
/// ```rust
/// # use litemcp::zod;
/// let schema = zod::object()
///     .field("query", zod::string())
///     .opt("limit", zod::integer().default(10));
/// // "limit" est publié avec `default: 10` et injecté si absent.
/// ```
#[derive(Clone)]
pub struct Defaulted<S> {
    inner: S,
    default: Value,
}

impl<S> Defaulted<S> {
    /// Attache une valeur par défaut à un schéma.
    #[must_use]
    pub fn new(inner: S, default: Value) -> Self {
        Self { inner, default }
    }
}

impl<S: Schema> Schema for Defaulted<S> {
    fn json(&self) -> Value {
        let mut schema = self.inner.json();
        if let Some(object) = schema.as_object_mut() {
            object.insert("default".to_string(), self.default.clone());
        }
        schema
    }

    fn validate(&self, path: &mut Path, value: &Value, issues: &mut Issues) {
        self.inner.validate(path, value, issues);
    }

    fn default_value(&self) -> Option<&Value> {
        Some(&self.default)
    }

    fn apply_defaults(&self, value: &mut Value) {
        self.inner.apply_defaults(value);
    }

    fn has_defaults(&self) -> bool {
        // Ce schéma porte **toujours** un défaut : la descente dans `inner`
        // n'ajouterait rien (et coûterait un parcours récursif inutile).
        true
    }
}

/// Champ de [`Obj`] : requis ([`.field`](Obj::field)) ou optionnel ([`.opt`](Obj::opt)).
#[derive(Clone)]
struct Field {
    name: String,
    schema: std::sync::Arc<dyn Schema>,
    required: bool,
}

/// Objet — équivalent de `z.object({ ... })`. Toujours la racine d'un
/// schéma d'arguments d'outil.
///
/// ```rust
/// # use litemcp::zod;
/// let schema = zod::object()
///     .field("query", zod::string().min(1).max(128)) // requis
///     .opt("limit", zod::integer().min(1).max(100))  // optionnel
///     .strict(); // refuse les champs inconnus
/// ```
#[derive(Clone, Default)]
pub struct Obj {
    fields: Vec<Field>,
    strict: bool,
}

/// `z.object({ ... })`
pub fn object() -> Obj {
    Default::default()
}

impl Obj {
    /// Ajoute un champ **requis**.
    ///
    /// **Panie de développement si le nom est déjà présent** : un doublon
    /// produirait un `inputSchema` invalide (`required` avec des éléments
    /// dupliqués — le meta-schema JSON Schema 2020-12 exige des éléments
    /// uniques) et un outil insatisfaisable (toutes les valeurs échouent).
    /// C'est une erreur de programmation, pas une entrée d'utilisateur :
    /// une panique au démarrage est le bon signal.
    #[must_use]
    pub fn field(mut self, name: &str, schema: impl Schema + 'static) -> Self {
        self.push_field(name, schema, true);
        self
    }

    /// Ajoute un champ **optionnel** (peut être absent).
    ///
    /// **Panie de développement si le nom est déjà présent** (voir
    /// [`Obj::field`]).
    #[must_use]
    pub fn opt(mut self, name: &str, schema: impl Schema + 'static) -> Self {
        self.push_field(name, schema, false);
        self
    }

    /// Insertion interne — refuse les doublons de nom.
    fn push_field(&mut self, name: &str, schema: impl Schema + 'static, required: bool) {
        assert!(
            !self.fields.iter().any(|f| f.name == name),
            "litemcp/zod : le champ `{name}` est déjà défini — un doublon produirait \
             un inputSchema invalide (required avec doublons) et un outil \
             insatisfaisable"
        );
        self.fields.push(Field {
            name: name.to_string(),
            schema: std::sync::Arc::new(schema),
            required,
        });
    }

    /// Mode strict : tout champ inconnu devient un problème de validation.
    #[must_use]
    pub fn strict(mut self) -> Self {
        self.strict = true;
        self
    }

    /// Valeur par défaut (publiée **et appliquée** — voir
    /// [`Defaulted`]).
    #[must_use]
    pub fn default(self, value: impl Into<Value>) -> Defaulted<Self> {
        Defaulted::new(self, value.into())
    }
}

impl Schema for Obj {
    fn json(&self) -> Value {
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();
        for field in &self.fields {
            properties.insert(field.name.clone(), field.schema.json());
            // Un champ **avec valeur par défaut** n'est pas `required` :
            // son absence est couverte par le défaut (publié dans le
            // `default` du schéma de la propriété).
            if field.required && field.schema.default_value().is_none() {
                required.push(json!(field.name));
            }
        }
        let mut schema = json!({ "type": "object", "properties": properties });
        if !required.is_empty() {
            schema["required"] = json!(required);
        }
        if self.strict {
            schema["additionalProperties"] = json!(false);
        }
        schema
    }

    fn validate(&self, path: &mut Path, value: &Value, issues: &mut Issues) {
        let Some(map) = value.as_object() else {
            push_issue!(issues, "`{}`: expected an object", path.as_str());
            return;
        };
        if path.depth >= MAX_DEPTH {
            push_issue!(
                issues,
                "`{}`: maximum nesting depth ({MAX_DEPTH}) exceeded",
                path.as_str()
            );
            return;
        }
        if self.strict {
            for name in map.keys() {
                if !self.fields.iter().any(|f| &f.name == name) {
                    if issues.is_full() {
                        issues.record_overflow();
                    } else {
                        let mark = path.mark();
                        path.push_field(name);
                        issues.push(format!(
                            "`{}`: unknown field (strict object)",
                            path.as_str()
                        ));
                        path.truncate(mark);
                    }
                }
            }
        }
        path.depth += 1;
        for field in &self.fields {
            match map.get(&field.name) {
                Some(child) => {
                    if issues.is_full() {
                        field.schema.validate(path, child, issues);
                    } else {
                        let mark = path.mark();
                        path.push_field(&field.name);
                        field.schema.validate(path, child, issues);
                        path.truncate(mark);
                    }
                }
                // Champ absent **avec valeur par défaut** : pas une
                // erreur — le défaut sera injecté (voir
                // [`Schema::apply_defaults`]). Le défaut lui-même est
                // validé : un défaut incohérent avec le schéma est une
                // erreur de programmation.
                None if field.schema.default_value().is_some() => {
                    if let Some(default) = field.schema.default_value() {
                        if issues.is_full() {
                            field.schema.validate(path, default, issues);
                        } else {
                            let mark = path.mark();
                            path.push_field(&field.name);
                            field.schema.validate(path, default, issues);
                            path.truncate(mark);
                        }
                    }
                }
                None if field.required => {
                    if issues.is_full() {
                        issues.record_overflow();
                    } else {
                        let mark = path.mark();
                        path.push_field(&field.name);
                        issues.push(format!("`{}` is required", path.as_str()));
                        path.truncate(mark);
                    }
                }
                None => {}
            }
        }
        path.depth -= 1;
    }

    fn apply_defaults(&self, value: &mut Value) {
        let Some(map) = value.as_object_mut() else {
            return;
        };
        for field in &self.fields {
            match map.get_mut(&field.name) {
                Some(child) => field.schema.apply_defaults(child),
                None => {
                    // Champ absent : on injecte le défaut s'il y en a un,
                    // puis on propage les éventuels défauts internes des
                    // sous-champs.
                    if let Some(default) = field.schema.default_value() {
                        let mut injected = default.clone();
                        field.schema.apply_defaults(&mut injected);
                        map.insert(field.name.clone(), injected);
                    }
                }
            }
        }
    }

    fn has_defaults(&self) -> bool {
        self.fields.iter().any(|field| field.schema.has_defaults())
    }
}

/// Format de chaîne — publié comme `format` (annotation en JSON Schema
/// 2020-12) et **exécuté** par la validation : le schéma reste exécuteur.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Adresse e-mail (forme `local@domaine.tld` — approximation assumée,
    /// sans dépendance regex).
    Email,
    /// UUID canonique 8-4-4-4-12 (forme seulement, comme Zod).
    Uuid,
    /// URL absolue `schéma://hôte…` (approximation assumée).
    Url,
    /// Date-time ISO 8601 (`YYYY-MM-DDTHH:MM:SS[.frac][Z|±HH:MM]`).
    Datetime,
}

impl Format {
    /// Mot-clé `format` JSON Schema (2020-12 : `date-time`, pas `datetime`).
    #[must_use]
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Email => "email",
            Self::Uuid => "uuid",
            Self::Url => "uri",
            Self::Datetime => "date-time",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Email => "a valid e-mail address",
            Self::Uuid => "UUID",
            Self::Url => "an absolute URL",
            Self::Datetime => "an ISO 8601 date-time",
        }
    }

    fn matches(self, text: &str) -> bool {
        match self {
            Self::Email => is_email(text),
            Self::Uuid => is_uuid(text),
            Self::Url => is_url(text),
            Self::Datetime => is_datetime(text),
        }
    }
}

/// `local@domaine` : un seul `@`, pas d'espace ni de caractère de contrôle,
/// bornes RFC 5321 (`<= 254` octets, `local <= 64`, `label <= 63`), `local`
/// sans point initial/final ni `..`, domaine en labels LDH (sans `-`
/// initial/final) contenant au moins un point.
fn is_email(text: &str) -> bool {
    if text.len() > 254 {
        return false;
    }
    let Some((local, domain)) = text.split_once('@') else {
        return false;
    };
    if domain.contains('@')
        || local.is_empty()
        || local.len() > 64
        || domain.is_empty()
        || local.starts_with('.')
        || local.ends_with('.')
        || local.contains("..")
    {
        return false;
    }
    if text.chars().any(|c| c.is_whitespace() || c.is_control()) || !domain.contains('.') {
        return false;
    }
    domain.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    }) && local
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "._%+-".contains(c))
}

/// 8-4-4-4-12 chiffres hexadécimaux, tirets aux positions canoniques.
fn is_uuid(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 36
        && [8usize, 13, 18, 23].iter().all(|&i| bytes[i] == b'-')
        && bytes
            .iter()
            .enumerate()
            .all(|(i, &b)| matches!(i, 8 | 13 | 18 | 23) || b.is_ascii_hexdigit())
}

/// `schéma://hôte/…` : schéma alphanumérique, autorité valide à hôte non vide,
/// aucun espace, caractère de contrôle ou antislash.
fn is_url(text: &str) -> bool {
    if text
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
    {
        return false;
    }
    let Some((scheme, rest)) = text.split_once("://") else {
        return false;
    };
    if !scheme
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic())
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    {
        return false;
    }
    let Some(authority) = rest.split(['/', '?', '#']).next() else {
        return false;
    };
    if authority.is_empty() {
        return false;
    }
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, hp)| hp);
    let (host, port) = if let Some(after_bracket) = host_port.strip_prefix('[') {
        let Some((ipv6, remainder)) = after_bracket.split_once(']') else {
            return false;
        };
        if ipv6.is_empty()
            || !ipv6
                .bytes()
                .all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.')
        {
            return false;
        }
        let port = match remainder.strip_prefix(':') {
            Some(p) => Some(p),
            None if remainder.is_empty() => None,
            None => return false,
        };
        (ipv6, port)
    } else if let Some((h, p)) = host_port.rsplit_once(':') {
        (h, Some(p))
    } else {
        (host_port, None)
    };
    if host.is_empty() || host.starts_with('.') || host.contains("..") {
        return false;
    }
    if let Some(p) = port {
        if p.is_empty() || p.parse::<u16>().ok().filter(|&n| n > 0).is_none() {
            return false;
        }
    }
    true
}

/// Date-time ISO 8601, calendrier vérifié (jours par mois, années
/// bissextiles) — analyse par octets, sans découpage de chaîne (jamais de
/// panique sur du multi-octet hostile).
fn is_datetime(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() < 20 {
        return false;
    }
    let digits = |range: std::ops::Range<usize>| {
        range
            .clone()
            .all(|i| bytes.get(i).is_some_and(|b| b.is_ascii_digit()))
    };
    let two = |i: usize| -> u32 { (bytes[i] - b'0') as u32 * 10 + (bytes[i + 1] - b'0') as u32 };
    // YYYY-MM-DD
    if !(digits(0..4) && bytes[4] == b'-' && digits(5..7) && bytes[7] == b'-' && digits(8..10)) {
        return false;
    }
    let year = two(0) * 100 + two(2);
    let month = two(5);
    let day = two(8);
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return false;
    }
    let separator = bytes[10];
    if !matches!(separator, b'T' | b't' | b' ') {
        return false;
    }
    // HH:MM:SS
    if !(digits(11..13)
        && bytes[13] == b':'
        && digits(14..16)
        && bytes[16] == b':'
        && digits(17..19))
    {
        return false;
    }
    if two(11) > 23 || two(14) > 59 || two(17) > 60 {
        return false; // 60 = seconde intercalaire tolérée
    }
    // Fraction optionnelle.
    let mut i = 19;
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        let fraction_start = i;
        while bytes.get(i).is_some_and(|b| b.is_ascii_digit()) {
            i += 1;
        }
        if i == fraction_start {
            return false;
        }
    }
    // Fuseau obligatoire (RFC 3339) : Z ou ±HH:MM.
    match bytes.get(i) {
        Some(&b'Z') | Some(&b'z') => i + 1 == bytes.len(),
        Some(&b'+') | Some(&b'-') => {
            digits(i + 1..i + 3)
                && bytes.get(i + 3) == Some(&b':')
                && digits(i + 4..i + 6)
                && two(i + 1) <= 23
                && two(i + 4) <= 59
                && i + 6 == bytes.len()
        }
        _ => false,
    }
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// `z.string()` — contraintes `.min(n)` / `.max(n)` (longueur en caractères,
/// conforme JSON Schema) et formats **exécutés** (`.email()`, `.uuid()`,
/// `.url()`, `.datetime()`).
#[derive(Clone, Default)]
pub struct Str {
    min: Option<usize>,
    max: Option<usize>,
    format: Option<Format>,
}

/// `z.string()`
pub fn string() -> Str {
    Default::default()
}

impl Str {
    /// Longueur minimale.
    #[must_use]
    pub fn min(mut self, n: usize) -> Self {
        self.min = Some(n);
        self.check();
        self
    }

    /// Longueur maximale.
    #[must_use]
    pub fn max(mut self, n: usize) -> Self {
        self.max = Some(n);
        self.check();
        self
    }

    fn check(&self) {
        if let (Some(min), Some(max)) = (self.min, self.max) {
            assert_ordered(&min, &max, "string()");
        }
    }

    /// Exige une adresse e-mail (publié `format: "email"`, appliqué).
    #[must_use]
    pub fn email(mut self) -> Self {
        self.format = Some(Format::Email);
        self
    }

    /// Exige un UUID canonique (publié `format: "uuid"`, appliqué).
    #[must_use]
    pub fn uuid(mut self) -> Self {
        self.format = Some(Format::Uuid);
        self
    }

    /// Exige une URL absolue (publié `format: "uri"`, appliqué).
    #[must_use]
    pub fn url(mut self) -> Self {
        self.format = Some(Format::Url);
        self
    }

    /// Valeur par défaut (publiée **et appliquée** — voir
    /// [`Defaulted`]).
    #[must_use]
    pub fn default(self, value: impl Into<Value>) -> Defaulted<Self> {
        Defaulted::new(self, value.into())
    }

    /// Exige un date-time ISO 8601 (publié `format: "date-time"`, appliqué).
    #[must_use]
    pub fn datetime(mut self) -> Self {
        self.format = Some(Format::Datetime);
        self
    }
}

impl Schema for Str {
    fn json(&self) -> Value {
        let mut schema = json!({ "type": "string" });
        if let Some(min) = self.min {
            schema["minLength"] = json!(min);
        }
        if let Some(max) = self.max {
            schema["maxLength"] = json!(max);
        }
        if let Some(format) = self.format {
            schema["format"] = json!(format.keyword());
        }
        schema
    }

    fn validate(&self, path: &mut Path, value: &Value, issues: &mut Issues) {
        let Some(text) = value.as_str() else {
            push_issue!(issues, "`{}`: expected a string", path.as_str());
            return;
        };
        // JSON Schema compte minLength/maxLength en CARACTÈRES
        // (points de code), pas en octets — conforme au schéma publié.
        let chars = text.chars().count();
        if let Some(min) = self.min {
            if chars < min {
                push_issue!(
                    issues,
                    "`{}`: must be at least {min} character(s)",
                    path.as_str()
                );
            }
        }
        if let Some(max) = self.max {
            if chars > max {
                push_issue!(
                    issues,
                    "`{}`: must be at most {max} character(s)",
                    path.as_str()
                );
            }
        }
        if let Some(format) = self.format {
            if !format.matches(text) {
                push_issue!(issues, "`{}`: expected {}", path.as_str(), format.label());
            }
        }
    }
}

/// `z.number().int()` — entier signé 64 bits, contraintes `.min(n)` / `.max(n)`.
///
/// **Validation stricte, alignée sur le schéma publié.** Seuls les entiers
/// JSON *syntaxiques* sont acceptés (`1`, `-1`, `42`) : un flottant à
/// fractule nulle (`3.0`, `1e2`) est **refusé**.
///
/// Pourquoi ce choix : le schéma publié est `{"type": "integer"}`, et le
/// JSON Schema draft 2020-12 y accepte `3.0`. Mais `serde_json::Value::as_i64()`
/// renvoie `None` sur un tel nombre — le motif d'usage documenté par la crate
/// (`args["a"].as_i64().unwrap()`) **paniquerait** sur une valeur que le
/// schéma disait valide. Refuser `3.0` rend la garantie du schéma
/// inconditionnelle : `as_i64()` réussit **toujours** quand le schéma passe.
///
/// Le schéma publié porte donc les bornes exactes (`minimum`/`maximum`) :
/// ce qu'il annonce est **exactement** ce que la validation applique.
#[derive(Clone, Default)]
pub struct Int {
    min: Option<i64>,
    max: Option<i64>,
}

/// `z.number().int()`
pub fn integer() -> Int {
    Default::default()
}

impl Int {
    /// Valeur minimale (incluse).
    #[must_use]
    pub fn min(mut self, n: i64) -> Self {
        self.min = Some(n);
        self.check();
        self
    }

    /// Valeur maximale (incluse).
    #[must_use]
    pub fn max(mut self, n: i64) -> Self {
        self.max = Some(n);
        self.check();
        self
    }

    fn check(&self) {
        if let (Some(min), Some(max)) = (self.min, self.max) {
            assert_ordered(&min, &max, "integer()");
        }
    }

    /// Valeur par défaut (publiée **et appliquée** — voir
    /// [`Defaulted`]).
    #[must_use]
    pub fn default(self, value: impl Into<Value>) -> Defaulted<Self> {
        Defaulted::new(self, value.into())
    }
}

impl Schema for Int {
    fn json(&self) -> Value {
        let mut schema = json!({ "type": "integer" });
        // Les bornes publiées sont **exactement** celles appliquées : le
        // schéma ne peut pas promettre plus que la validation n'accorde.
        schema["minimum"] = json!(self.min.unwrap_or(i64::MIN));
        schema["maximum"] = json!(self.max.unwrap_or(i64::MAX));
        schema
    }

    fn validate(&self, path: &mut Path, value: &Value, issues: &mut Issues) {
        // **Entier JSON strict** : `Value::as_i64()` ne réussit que pour un
        // nombre stocké comme entier (`1`, `-1`). Un flottant à fractule
        // nulle (`3.0`, `1e2`) est refusé : il passerait le schéma JSON
        // (« integer » matche `3.0`) mais casserait `as_i64().unwrap()`,
        // dont la crate **documente** l'usage. Refuser ici rend la promesse
        // du schéma vraie.
        let Some(n) = value.as_i64() else {
            push_issue!(issues, "`{}`: expected an integer", path.as_str());
            return;
        };
        if let Some(min) = self.min {
            if n < min {
                push_issue!(issues, "`{}`: must be ≥ {min}", path.as_str());
            }
        }
        if let Some(max) = self.max {
            if n > max {
                push_issue!(issues, "`{}`: must be ≤ {max}", path.as_str());
            }
        }
    }
}

/// `z.number()` — tout nombre JSON, contraintes `.min(n)` / `.max(n)`.
#[derive(Clone, Default)]
pub struct Num {
    min: Option<f64>,
    max: Option<f64>,
}

/// `z.number()`
pub fn number() -> Num {
    Default::default()
}

impl Num {
    /// Valeur minimale (incluse). **Panie de développement si non finie**
    /// (NaN/±∞) : `serde_json` la dégraderait en `null` dans le schéma
    /// publié et la validation ne l'appliquerait jamais — silencieusement.
    #[must_use]
    pub fn min(mut self, n: f64) -> Self {
        assert!(
            n.is_finite(),
            "litemcp/zod : borne non finie ({n:?}) sur number() — serde_json la \
             publierait en null et la validation ne l'appliquerait jamais"
        );
        self.min = Some(n);
        self.check();
        self
    }

    /// Valeur maximale (incluse). Même règle de finitude que
    /// [`Num::min`].
    #[must_use]
    pub fn max(mut self, n: f64) -> Self {
        assert!(
            n.is_finite(),
            "litemcp/zod : borne non finie ({n:?}) sur number() — serde_json la \
             publierait en null et la validation ne l'appliquerait jamais"
        );
        self.max = Some(n);
        self.check();
        self
    }

    fn check(&self) {
        if let (Some(min), Some(max)) = (self.min, self.max) {
            assert_ordered(&min, &max, "number()");
        }
    }

    /// Valeur par défaut (publiée **et appliquée** — voir
    /// [`Defaulted`]).
    #[must_use]
    pub fn default(self, value: impl Into<Value>) -> Defaulted<Self> {
        Defaulted::new(self, value.into())
    }
}

impl Schema for Num {
    fn json(&self) -> Value {
        let mut schema = json!({ "type": "number" });
        if let Some(min) = self.min {
            schema["minimum"] = json!(min);
        }
        if let Some(max) = self.max {
            schema["maximum"] = json!(max);
        }
        schema
    }

    fn validate(&self, path: &mut Path, value: &Value, issues: &mut Issues) {
        match value.as_f64() {
            Some(n) => {
                if let Some(min) = self.min {
                    if n < min {
                        push_issue!(issues, "`{}`: must be ≥ {min}", path.as_str());
                    }
                }
                if let Some(max) = self.max {
                    if n > max {
                        push_issue!(issues, "`{}`: must be ≤ {max}", path.as_str());
                    }
                }
            }
            None => push_issue!(issues, "`{}`: expected a number", path.as_str()),
        }
    }
}

/// `z.boolean()`.
#[derive(Clone, Default)]
pub struct Bool;

/// `z.boolean()`
pub fn boolean() -> Bool {
    Bool
}

impl Bool {
    /// Valeur par défaut (publiée **et appliquée** — voir
    /// [`Defaulted`]).
    #[must_use]
    pub fn default(self, value: impl Into<Value>) -> Defaulted<Self> {
        Defaulted::new(self, value.into())
    }
}

impl Schema for Bool {
    fn json(&self) -> Value {
        json!({ "type": "boolean" })
    }

    fn validate(&self, path: &mut Path, value: &Value, issues: &mut Issues) {
        if value.as_bool().is_none() {
            push_issue!(issues, "`{}`: expected a boolean", path.as_str());
        }
    }
}

/// `z.enum([...])` — chaîne parmi un ensemble fermé.
#[derive(Clone)]
pub struct Enum {
    /// Les valeurs autorisées — chaînes ou entiers.
    values: Vec<Value>,
    /// Le type JSON publié (`"string"` ou `"integer"`).
    kind: &'static str,
}

/// `z.enum(["a", "b", ...])` — énumération de **chaînes**.
///
/// **Panie de développement** si la liste est vide (schéma insatisfaisable)
/// ou contient un doublon (le meta-schema JSON Schema exige des éléments
/// `enum` uniques).
pub fn enumeration(values: &[&str]) -> Enum {
    assert!(
        !values.is_empty(),
        "litemcp/zod : enumeration vide — schéma insatisfaisable"
    );
    for (index, value) in values.iter().enumerate() {
        assert!(
            !values[index + 1..].contains(value),
            "litemcp/zod : doublon {value:?} dans enumeration — le meta-schema \
             JSON Schema exige des éléments uniques"
        );
    }
    Enum {
        values: values.iter().map(|v| json!(*v)).collect(),
        kind: "string",
    }
}

/// `z.enum([1, 2, 3])` — énumération d'**entiers**.
///
/// Même contrat que [`enumeration`] : liste vide ou doublon → panique de
/// développement. Publié `{"type": "integer", "enum": [1, 2, 3]}`.
pub fn integer_enumeration(values: &[i64]) -> Enum {
    assert!(
        !values.is_empty(),
        "litemcp/zod : enumeration vide — schéma insatisfaisable"
    );
    for (index, value) in values.iter().enumerate() {
        assert!(
            !values[index + 1..].contains(value),
            "litemcp/zod : doublon {value:?} dans enumeration — le meta-schema \
             JSON Schema exige des éléments uniques"
        );
    }
    Enum {
        values: values.iter().map(|v| json!(*v)).collect(),
        kind: "integer",
    }
}

impl Enum {
    /// Valeur par défaut (publiée **et appliquée** — voir
    /// [`Defaulted`]).
    #[must_use]
    pub fn default(self, value: impl Into<Value>) -> Defaulted<Self> {
        Defaulted::new(self, value.into())
    }
}

impl Schema for Enum {
    fn json(&self) -> Value {
        json!({ "type": self.kind, "enum": self.values })
    }

    fn validate(&self, path: &mut Path, value: &Value, issues: &mut Issues) {
        if self.values.iter().any(|v| v == value) {
            return;
        }
        if issues.is_full() {
            issues.record_overflow();
            return;
        }
        // Type incorrect → message de type ; sinon message d'appartenance.
        let type_ok = match self.kind {
            "integer" => value.is_i64() || value.is_u64(),
            _ => value.is_string(),
        };
        if type_ok {
            let rendered = self
                .values
                .iter()
                .map(|v| match v.as_str() {
                    Some(s) => s.to_string(),
                    None => v.to_string(),
                })
                .collect::<Vec<_>>()
                .join(", ");
            issues.push(format!("`{}`: must be one of {rendered}", path.as_str()));
        } else {
            let expected = if self.kind == "integer" {
                "expected an integer"
            } else {
                "expected a string"
            };
            issues.push(format!("`{}`: {expected}", path.as_str()));
        }
    }
}

/// `z.record(inner)` — objet à **valeurs typées** (dictionnaire).
///
/// Chaque valeur de l'objet est validée par le schéma interne ; les clés
/// sont libres (chaînes, comme en JSON).
///
/// Publié `{"type": "object", "additionalProperties": <inner>}` — la
/// forme JSON Schema du dictionnaire.
///
/// ```rust
/// # use litemcp::zod;
/// let schema = zod::record(zod::string());
/// // {"a": "x", "b": "y"} → valide ; {"a": 1} → refusé.
/// ```
#[derive(Clone)]
pub struct Record {
    inner: std::sync::Arc<dyn Schema>,
}

impl Record {
    /// Le schéma interne (type des valeurs).
    #[must_use]
    pub fn inner(&self) -> &dyn Schema {
        self.inner.as_ref()
    }

    /// Valeur par défaut (publiée **et appliquée** — voir
    /// [`Defaulted`]).
    #[must_use]
    pub fn default(self, value: impl Into<Value>) -> Defaulted<Self> {
        Defaulted::new(self, value.into())
    }
}

/// `z.record(inner)`
pub fn record(inner: impl Schema + 'static) -> Record {
    Record {
        inner: std::sync::Arc::new(inner),
    }
}

impl Schema for Record {
    fn json(&self) -> Value {
        json!({
            "type": "object",
            "additionalProperties": self.inner.json(),
        })
    }

    fn validate(&self, path: &mut Path, value: &Value, issues: &mut Issues) {
        let Some(map) = value.as_object() else {
            push_issue!(issues, "`{}`: expected an object", path.as_str());
            return;
        };
        if path.depth >= MAX_DEPTH {
            push_issue!(
                issues,
                "`{}`: maximum nesting depth ({MAX_DEPTH}) exceeded",
                path.as_str()
            );
            return;
        }
        path.depth += 1;
        for (key, child) in map {
            if issues.is_full() {
                self.inner.validate(path, child, issues);
            } else {
                let mark = path.mark();
                path.push_field(key);
                self.inner.validate(path, child, issues);
                path.truncate(mark);
            }
        }
        path.depth -= 1;
    }

    fn apply_defaults(&self, value: &mut Value) {
        // Les valeurs présentes sont parcourues (chacune peut porter
        // ses propres défauts) ; aucune clé n'est inventée.
        if let Some(map) = value.as_object_mut() {
            for child in map.values_mut() {
                self.inner.apply_defaults(child);
            }
        }
    }

    fn has_defaults(&self) -> bool {
        self.inner.has_defaults()
    }
}

/// `z.array(inner)` — tableau homogène, contraintes `.min(n)` / `.max(n)` (taille).
#[derive(Clone)]
pub struct Arr {
    inner: std::sync::Arc<dyn Schema>,
    min: Option<usize>,
    max: Option<usize>,
}

/// `z.array(schema)`
pub fn array(inner: impl Schema + 'static) -> Arr {
    Arr {
        inner: std::sync::Arc::new(inner),
        min: None,
        max: None,
    }
}

impl Arr {
    /// Nombre minimal d'éléments.
    #[must_use]
    pub fn min(mut self, n: usize) -> Self {
        self.min = Some(n);
        self.check();
        self
    }

    /// Nombre maximal d'éléments.
    #[must_use]
    pub fn max(mut self, n: usize) -> Self {
        self.max = Some(n);
        self.check();
        self
    }

    fn check(&self) {
        if let (Some(min), Some(max)) = (self.min, self.max) {
            assert_ordered(&min, &max, "array()");
        }
    }

    /// Valeur par défaut (publiée **et appliquée** — voir
    /// [`Defaulted`]).
    #[must_use]
    pub fn default(self, value: impl Into<Value>) -> Defaulted<Self> {
        Defaulted::new(self, value.into())
    }
}

impl Schema for Arr {
    fn json(&self) -> Value {
        let mut schema = json!({ "type": "array", "items": self.inner.json() });
        if let Some(min) = self.min {
            schema["minItems"] = json!(min);
        }
        if let Some(max) = self.max {
            schema["maxItems"] = json!(max);
        }
        schema
    }

    fn validate(&self, path: &mut Path, value: &Value, issues: &mut Issues) {
        let Some(items) = value.as_array() else {
            push_issue!(issues, "`{}`: expected an array", path.as_str());
            return;
        };
        if path.depth >= MAX_DEPTH {
            push_issue!(
                issues,
                "`{}`: maximum nesting depth ({MAX_DEPTH}) exceeded",
                path.as_str()
            );
            return;
        }
        if let Some(min) = self.min {
            if items.len() < min {
                push_issue!(
                    issues,
                    "`{}`: must contain at least {min} item(s)",
                    path.as_str()
                );
            }
        }
        if let Some(max) = self.max {
            if items.len() > max {
                push_issue!(
                    issues,
                    "`{}`: must contain at most {max} item(s)",
                    path.as_str()
                );
            }
        }
        path.depth += 1;
        for (index, item) in items.iter().enumerate() {
            if issues.is_full() {
                self.inner.validate(path, item, issues);
            } else {
                let mark = path.mark();
                path.push_index(index);
                self.inner.validate(path, item, issues);
                path.truncate(mark);
            }
        }
        path.depth -= 1;
    }

    fn apply_defaults(&self, value: &mut Value) {
        if let Some(items) = value.as_array_mut() {
            for item in items {
                self.inner.apply_defaults(item);
            }
        }
    }

    fn has_defaults(&self) -> bool {
        self.inner.has_defaults()
    }
}

/// `z.nullable()` — accepte `null` **en plus** du schéma interne.
///
/// Publié `{"anyOf": [schéma, {"type": "null"}]}` (la forme JSON Schema
/// 2020-12) ; la validation accepte `null` sans issue et délègue le reste.
/// Compose avec n'importe quel schéma : `zod::nullable(zod::string().max(3))`.
#[derive(Clone)]
pub struct Nullable {
    inner: Arc<dyn Schema>,
}

/// `z.nullable(schema)`
pub fn nullable(inner: impl Schema + 'static) -> Nullable {
    Nullable {
        inner: Arc::new(inner),
    }
}

impl Nullable {
    /// Valeur par défaut (publiée **et appliquée** — voir
    /// [`Defaulted`]).
    #[must_use]
    pub fn default(self, value: impl Into<Value>) -> Defaulted<Self> {
        Defaulted::new(self, value.into())
    }
}

impl Schema for Nullable {
    fn json(&self) -> Value {
        let mut schema = json!({ "anyOf": [self.inner.json(), { "type": "null" }] });
        if let Some(default) = self.inner.default_value() {
            schema["default"] = default.clone();
        }
        schema
    }

    fn validate(&self, path: &mut Path, value: &Value, issues: &mut Issues) {
        if value.is_null() {
            return;
        }
        self.inner.validate(path, value, issues);
    }

    fn default_value(&self) -> Option<&Value> {
        self.inner.default_value()
    }

    fn apply_defaults(&self, value: &mut Value) {
        if !value.is_null() {
            self.inner.apply_defaults(value);
        }
    }

    fn has_defaults(&self) -> bool {
        self.inner.has_defaults()
    }
}

/// Attache une **description** à n'importe quel schéma (`z.describe(…)`).
///
/// Publiée dans le JSON Schema (`description`) — utile pour documenter un
/// champ : `zod::object().field("q", zod::describe("requête", zod::string()))`.
/// La validation délègue intégralement au schéma interne.
#[derive(Clone)]
pub struct Described {
    description: String,
    inner: Arc<dyn Schema>,
}

/// `z.describe("…", schema)`
pub fn describe(description: &str, inner: impl Schema + 'static) -> Described {
    Described {
        description: description.to_string(),
        inner: Arc::new(inner),
    }
}

impl Described {
    /// Valeur par défaut (publiée **et appliquée** — voir
    /// [`Defaulted`]).
    #[must_use]
    pub fn default(self, value: impl Into<Value>) -> Defaulted<Self> {
        Defaulted::new(self, value.into())
    }
}

impl Schema for Described {
    fn json(&self) -> Value {
        let mut schema = self.inner.json();
        if let Some(object) = schema.as_object_mut() {
            object.insert("description".to_string(), json!(self.description));
        }
        schema
    }

    fn validate(&self, path: &mut Path, value: &Value, issues: &mut Issues) {
        self.inner.validate(path, value, issues);
    }

    fn default_value(&self) -> Option<&Value> {
        self.inner.default_value()
    }

    fn apply_defaults(&self, value: &mut Value) {
        self.inner.apply_defaults(value);
    }

    fn has_defaults(&self) -> bool {
        self.inner.has_defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn issues_of(schema: &dyn Schema, value: Value) -> Vec<String> {
        let mut issues = Issues::new();
        let mut path = Path::root("arguments");
        schema.validate(&mut path, &value, &mut issues);
        issues.render()
    }

    #[test]
    fn object_schema_is_derived() {
        let schema = object()
            .field("a", integer())
            .opt("b", string().max(8))
            .strict();
        assert_eq!(
            schema.json(),
            json!({
                "type": "object",
                "properties": {
                    // Les bornes publiées sont celles réellement appliquées.
                    "a": { "type": "integer", "minimum": i64::MIN, "maximum": i64::MAX },
                    "b": { "type": "string", "maxLength": 8 }
                },
                "required": ["a"],
                "additionalProperties": false
            })
        );
    }

    #[test]
    fn valid_object_has_no_issue() {
        let schema = object().field("a", integer()).opt("b", string());
        assert!(issues_of(&schema, json!({ "a": 1 })).is_empty());
        assert!(issues_of(&schema, json!({ "a": 1, "b": "x" })).is_empty());
    }

    #[test]
    fn missing_required_and_wrong_type_are_reported() {
        let schema = object().field("a", integer()).field("b", string());
        let issues = issues_of(&schema, json!({ "a": "oops" }));
        assert_eq!(issues.len(), 2, "{issues:?}");
        assert!(issues[0].contains("arguments.a"));
        assert!(issues[1].contains("required"));
    }

    #[test]
    fn strict_mode_rejects_unknown_fields() {
        let schema = object().field("a", integer()).strict();
        let issues = issues_of(&schema, json!({ "a": 1, "zzz": true }));
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("unknown field"));
    }

    #[test]
    fn non_object_arguments_are_rejected() {
        let schema = object().field("a", integer());
        let issues = issues_of(&schema, json!("not an object"));
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("expected an object"));
    }

    #[test]
    fn nested_paths_are_reported() {
        let schema = object().field("list", array(integer()).min(1));
        let issues = issues_of(&schema, json!({ "list": [1, "x", 3] }));
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("arguments.list[1]"));
    }

    #[test]
    fn integer_bounds_apply() {
        let schema = object().field("n", integer().min(1).max(10));
        assert!(!issues_of(&schema, json!({ "n": 0 })).is_empty());
        assert!(issues_of(&schema, json!({ "n": 10 })).is_empty());
        assert!(!issues_of(&schema, json!({ "n": 1.5 })).is_empty());
    }

    #[test]
    fn integer_rejects_non_integral_floats() {
        // Le schéma doit être EXÉCUTEUR : une valeur acceptée par la
        // validation doit toujours être lisible par `Value::as_i64()`.
        // `3.0` et `1e2` sont des « integer » pour JSON Schema mais
        // `as_i64()` renvoie None → ils sont refusés.
        let schema = object().field("n", integer());
        assert!(!issues_of(&schema, json!({ "n": 3.0 })).is_empty());
        assert!(!issues_of(&schema, json!({ "n": 1e2 })).is_empty());
        assert!(!issues_of(&schema, json!({ "n": 0.0 })).is_empty());
        assert!(!issues_of(&schema, json!({ "n": -0.0 })).is_empty());
        // Les vrais entiers passent.
        assert!(issues_of(&schema, json!({ "n": 3 })).is_empty());
        assert!(issues_of(&schema, json!({ "n": -3 })).is_empty());
        assert!(issues_of(&schema, json!({ "n": 0 })).is_empty());
    }

    #[test]
    fn integer_published_bounds_match_the_validation() {
        // Le schéma publié ne doit pas promettre plus que la validation
        // n'accorde : les bornes exactes sont publiées.
        let schema = object().field("n", integer().min(1).max(10));
        assert_eq!(schema.json()["properties"]["n"]["minimum"], 1);
        assert_eq!(schema.json()["properties"]["n"]["maximum"], 10);

        // Sans bornes explicites : les bornes i64 réelles sont publiées.
        let schema = object().field("n", integer());
        assert_eq!(schema.json()["properties"]["n"]["minimum"], i64::MIN);
        assert_eq!(schema.json()["properties"]["n"]["maximum"], i64::MAX);
    }

    #[test]
    fn duplicate_field_name_panics() {
        // Un doublon produirait un inputSchema invalide (`required` avec
        // doublons) et un outil insatisfaisable : c'est une erreur de
        // programmation → panique au démarrage.
        let result =
            std::panic::catch_unwind(|| object().field("a", integer()).field("a", string()));
        assert!(result.is_err(), "un doublon de nom doit paniquer");
    }

    #[test]
    fn integer_rejects_values_outside_i64_range() {
        let schema = object().field("n", integer());
        // 2^63 n'est pas représentable en i64 : refusé, jamais saturé en i64::MAX.
        let issues = issues_of(&schema, json!({ "n": 9_223_372_036_854_775_808u64 }));
        assert_eq!(issues.len(), 1, "{issues:?}");
        // Les flottants sont refusés (as_i64() strict).
        let two_pow_63 = 2f64.powi(63);
        assert_eq!(issues_of(&schema, json!({ "n": two_pow_63 })).len(), 1);
        assert_eq!(issues_of(&schema, json!({ "n": -two_pow_63 })).len(), 1);
        // Les bornes exactes i64, elles, passent.
        assert!(issues_of(&schema, json!({ "n": i64::MAX })).is_empty());
        assert!(issues_of(&schema, json!({ "n": i64::MIN })).is_empty());
    }

    #[test]
    fn enumeration_limits_values() {
        let schema = object().field("mode", enumeration(&["fast", "safe"]));
        assert!(issues_of(&schema, json!({ "mode": "fast" })).is_empty());
        let issues = issues_of(&schema, json!({ "mode": "wild" }));
        assert!(issues[0].contains("fast, safe"));
    }

    #[test]
    fn issues_are_capped_to_bound_memory() {
        // Amplification mémoire : 200 éléments invalides ne doivent produire
        // que MAX_ISSUES issues stockées + un récapitulatif des omises.
        let schema = array(integer());
        let value = json!(vec!["x"; 200]);
        let mut issues = Issues::new();
        let mut path = Path::root("arguments");
        schema.validate(&mut path, &value, &mut issues);
        assert_eq!(issues.render().len(), MAX_ISSUES + 1);
        assert!(issues.render().last().unwrap().contains("omitted"));
        assert!(!issues.is_empty());
    }

    // --- Audit 2026-10-24 : assertions de construction (C1-C4) ---

    #[test]
    fn inconsistent_bounds_panic_at_construction() {
        // C2 : min > max sur chaque sorte de borne — jamais publié.
        for broken in [
            || {
                let _ = integer().min(10).max(5);
            },
            || {
                let _ = string().min(5).max(3);
            },
            || {
                let _ = number().min(10.0).max(5.0);
            },
            || {
                let _ = array(integer()).min(4).max(2);
            },
        ] {
            let result = std::panic::catch_unwind(broken);
            assert!(result.is_err(), "bornes incohérentes = panique dev");
        }
    }

    #[test]
    fn non_finite_number_bounds_panic_at_construction() {
        // C1 : NaN/±∞ dans une borne f64 — serde_json les publierait en
        // `null` et la validation ne les appliquerait jamais.
        for broken in [
            || {
                let _ = number().min(f64::NAN);
            },
            || {
                let _ = number().max(f64::NAN);
            },
            || {
                let _ = number().min(f64::INFINITY);
            },
            || {
                let _ = number().max(f64::NEG_INFINITY);
            },
        ] {
            let result = std::panic::catch_unwind(broken);
            assert!(result.is_err(), "borne non finie = panique dev");
        }
    }

    #[test]
    fn broken_enumerations_panic_at_construction() {
        // C3/C4 : liste vide (insatisfaisable) ou doublons (meta-schema
        // exige des éléments uniques).
        let result = std::panic::catch_unwind(|| {
            let _ = enumeration(&[]);
        });
        assert!(result.is_err(), "enumeration vide = panique dev");
        let result = std::panic::catch_unwind(|| {
            let _ = enumeration(&["a", "a", "b"]);
        });
        assert!(result.is_err(), "doublon dans enumeration = panique dev");
    }

    // --- A1 : nullable ---

    #[test]
    fn nullable_accepts_null_and_validates_the_inner_schema() {
        // `nullable` ne change PAS la cardinalité : avec `.field` le champ
        // reste requis (absent → issue), avec `.opt` il est optionnel.
        let required = object().field("s", nullable(string().max(3)));
        let optional = object().opt("s", nullable(string().max(3)));
        // null : accepté sans issue par les deux.
        assert!(issues_of(&required, json!({ "s": null })).is_empty());
        assert!(issues_of(&optional, json!({ "s": null })).is_empty());
        // Absent : requis le refuse, optionnel l'accepte.
        assert!(!issues_of(&required, json!({})).is_empty());
        assert!(issues_of(&optional, json!({})).is_empty());
        // Valeur valide : déléguée au schéma interne.
        assert!(issues_of(&required, json!({ "s": "abc" })).is_empty());
        // Valeur invalide : issue du schéma interne.
        assert!(!issues_of(&required, json!({ "s": "abcd" })).is_empty());
        // Publié en anyOf avec le type null.
        assert_eq!(
            required.json()["properties"]["s"]["anyOf"][1],
            json!({ "type": "null" })
        );
    }

    // --- A2 : formats exécutés ---

    fn formatted(format: Format) -> Str {
        match format {
            Format::Email => string().email(),
            Format::Uuid => string().uuid(),
            Format::Url => string().url(),
            Format::Datetime => string().datetime(),
        }
    }

    #[test]
    fn formats_are_published_and_enforced() {
        let cases: [(Format, &str, &str, &str); 4] = [
            (Format::Email, "a@b.cd", "pas-un-email", "email"),
            (
                Format::Uuid,
                "123e4567-e89b-12d3-a456-426614174000",
                "123e4567-e89b-12d3-a456-42661417400",
                "uuid",
            ),
            (Format::Url, "https://a.example/x?y=1", "pas-une-url", "uri"),
            (
                Format::Datetime,
                "2026-10-24T18:30:00Z",
                "2026-13-01T00:00:00Z",
                "date-time",
            ),
        ];
        for (format, valid, invalid, keyword) in cases {
            let schema = object().field("v", formatted(format));
            // Format publié ET appliqué : le schéma est exécuteur.
            assert_eq!(schema.json()["properties"]["v"]["format"], keyword);
            assert!(
                issues_of(&schema, json!({ "v": valid })).is_empty(),
                "{format:?} : {valid} doit passer"
            );
            assert!(
                !issues_of(&schema, json!({ "v": invalid })).is_empty(),
                "{format:?} : {invalid} doit être refusé"
            );
        }
    }

    #[test]
    fn datetime_calendar_is_enforced() {
        let schema = object().field("d", string().datetime());
        // 30 février : refusé (calendrier vérifié).
        assert!(!issues_of(&schema, json!({ "d": "2026-02-30T00:00:00Z" })).is_empty());
        // 29 février non bissextile : refusé ; bissextile : accepté.
        assert!(!issues_of(&schema, json!({ "d": "2027-02-29T00:00:00Z" })).is_empty());
        assert!(issues_of(&schema, json!({ "d": "2028-02-29T00:00:00Z" })).is_empty());
        // Fraction et fuseau ±HH:MM.
        assert!(issues_of(&schema, json!({ "d": "2026-10-24T18:30:00.123+02:00" })).is_empty());
        // Sans fuseau : refusé (RFC 3339).
        assert!(!issues_of(&schema, json!({ "d": "2026-10-24T18:30:00" })).is_empty());
        // Octets non-ASCII : aucune panique, simple refus.
        assert!(!issues_of(&schema, json!({ "d": "éééé-éé-ééTéé:éé:ééZ" })).is_empty());
    }

    // --- A3 : describe ---

    #[test]
    fn describe_publishes_the_description_and_delegates() {
        let schema = object().field("q", describe("termes de la recherche", string().min(2)));
        let published = &schema.json()["properties"]["q"];
        assert_eq!(published["description"], "termes de la recherche");
        assert_eq!(published["type"], "string");
        // Validation déléguée intégralement.
        assert!(issues_of(&schema, json!({ "q": "ok" })).is_empty());
        assert!(!issues_of(&schema, json!({ "q": "x" })).is_empty());
    }

    #[test]
    fn email_and_url_formats_reject_control_chars_and_rfc_violations() {
        let email_schema = object().field("e", string().email());
        for bad_email in [
            ".a@b.cd",
            "a.@b.cd",
            "a..b@b.cd",
            "a@-b.cd",
            "a@b-.cd",
            "a@b.cd\x00",
            "a@b.cd\x1b[31m",
            &format!("{}@b.cd", "a".repeat(65)),
        ] {
            assert!(
                !issues_of(&email_schema, json!({ "e": bad_email })).is_empty(),
                "email invalide accepté : {bad_email:?}"
            );
        }

        let url_schema = object().field("u", string().url());
        for bad_url in [
            "https://a.example/x\x00",
            "https://a.example/\x1b[31m",
            "https://a.example/\x7f",
            "https://a.example\\path",
            "https://@/path",
            "https://:80/path",
            "https://a.example:0/path",
            "https://a.example:99999/path",
            "https://.a.example/path",
            "https://a..example/path",
        ] {
            assert!(
                !issues_of(&url_schema, json!({ "u": bad_url })).is_empty(),
                "URL invalide acceptée : {bad_url:?}"
            );
        }
        assert!(issues_of(&url_schema, json!({ "u": "https://[::1]:8080/mcp" })).is_empty());
    }

    #[test]
    fn max_depth_guard_rejects_excessive_nesting_without_stack_overflow() {
        let mut schema: Box<dyn Schema> = Box::new(integer());
        let mut value = json!(42);
        for _ in 0..(MAX_DEPTH + 4) {
            schema = Box::new(array(schema));
            value = json!([value]);
        }
        let issues = issues_of(&schema, value);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].contains("maximum nesting depth"), "{issues:?}");
    }
}
