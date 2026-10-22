//! Schémas à la Zod — dérivation JSON Schema + validation runtime.
//!
//! L'API calque [Zod](https://zod.dev), le standard de l'écosystème MCP
//! TypeScript : on déclare un schéma, la bibliothèque en dérive le
//! `inputSchema` MCP **et** valide chaque `tools/call` avant d'exécuter le
//! handler. Un appel hors schéma ne touche jamais votre code — il reçoit une
//! erreur JSON-RPC `-32602` portant la liste des problèmes
//! (`error.data.issues`), l'équivalent d'une `ZodError`.
//!
//! Les problèmes sont accumulés dans un collecteur borné ([`Issues`]) :
//! au-delà de [`MAX_ISSUES`], ils sont comptés sans être stockés — un
//! tableau géant d'éléments invalides ne peut pas faire grossir la mémoire
//! indéfiniment.
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
//!             let a = args["a"].as_i64().unwrap(); // garanti par le schéma
//!             let b = args["b"].as_i64().unwrap();
//!             Ok(serde_json::json!({ "somme": a + b }))
//!         },
//!     )
//!     .bind("127.0.0.1:8080")?
//!     .serve()?;
//! # Ok::<(), std::io::Error>(())
//! ```

use serde_json::{Value, json};

/// Plafond d'issues stockées par une validation ([`Issues`]) — au-delà, les
/// problèmes sont comptés sans être stockés.
pub const MAX_ISSUES: usize = 64;

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

    /// Enregistre une issue — au-delà de [`MAX_ISSUES`], elle est seulement
    /// comptée.
    pub fn push(&mut self, issue: impl Into<String>) {
        if self.items.len() < MAX_ISSUES {
            self.items.push(issue.into());
        } else {
            self.overflow += 1;
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
                "… et {overflow} problème(s) supplémentaire(s) omis",
                overflow = self.overflow
            ));
        }
        out
    }
}

/// Contrat commun de tous les schémas : rendu JSON Schema + validation.
pub trait Schema: Send + Sync {
    /// Fragment JSON Schema (draft 2020-12) correspondant.
    fn json(&self) -> Value;
    /// Valide `value` (sous le chemin `key`) et accumule les problèmes
    /// lisibles dans un collecteur borné ([`Issues`]).
    fn validate(&self, key: &str, value: &Value, issues: &mut Issues);
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
    Obj::default()
}

impl Obj {
    /// Ajoute un champ **requis**.
    #[must_use]
    pub fn field(mut self, name: &str, schema: impl Schema + 'static) -> Self {
        self.fields.push(Field {
            name: name.to_string(),
            schema: std::sync::Arc::new(schema),
            required: true,
        });
        self
    }

    /// Ajoute un champ **optionnel** (peut être absent).
    #[must_use]
    pub fn opt(mut self, name: &str, schema: impl Schema + 'static) -> Self {
        self.fields.push(Field {
            name: name.to_string(),
            schema: std::sync::Arc::new(schema),
            required: false,
        });
        self
    }

    /// Mode strict : tout champ inconnu devient un problème de validation.
    #[must_use]
    pub fn strict(mut self) -> Self {
        self.strict = true;
        self
    }
}

impl Schema for Obj {
    fn json(&self) -> Value {
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();
        for field in &self.fields {
            properties.insert(field.name.clone(), field.schema.json());
            if field.required {
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

    fn validate(&self, key: &str, value: &Value, issues: &mut Issues) {
        let Some(map) = value.as_object() else {
            issues.push(format!("`{key}` : objet attendu"));
            return;
        };
        if self.strict {
            for name in map.keys() {
                if !self.fields.iter().any(|f| &f.name == name) {
                    issues.push(format!("`{key}.{name}` : champ inconnu (objet strict)"));
                }
            }
        }
        for field in &self.fields {
            let path = format!("{key}.{}", field.name);
            match map.get(&field.name) {
                Some(child) => field.schema.validate(&path, child, issues),
                None if field.required => issues.push(format!("`{path}` requis")),
                None => {}
            }
        }
    }
}

/// `z.string()` — contraintes `.min(n)` / `.max(n)` (longueur en caractères, conforme JSON Schema).
#[derive(Clone, Default)]
pub struct Str {
    min: Option<usize>,
    max: Option<usize>,
}

/// `z.string()`
pub fn string() -> Str {
    Str::default()
}

impl Str {
    /// Longueur minimale.
    #[must_use]
    pub fn min(mut self, n: usize) -> Self {
        self.min = Some(n);
        self
    }

    /// Longueur maximale.
    #[must_use]
    pub fn max(mut self, n: usize) -> Self {
        self.max = Some(n);
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
        schema
    }

    fn validate(&self, key: &str, value: &Value, issues: &mut Issues) {
        match value.as_str() {
            Some(s) => {
                // JSON Schema compte minLength/maxLength en CARACTÈRES
                // (points de code), pas en octets — conforme au schéma publié.
                let chars = s.chars().count();
                if let Some(min) = self.min {
                    if chars < min {
                        issues.push(format!("`{key}` : au moins {min} caractère(s)"));
                    }
                }
                if let Some(max) = self.max {
                    if chars > max {
                        issues.push(format!("`{key}` : au plus {max} caractère(s)"));
                    }
                }
            }
            None => issues.push(format!("`{key}` : chaîne attendue")),
        }
    }
}

/// `z.number().int()` — entier signé 64 bits, contraintes `.min(n)` / `.max(n)`.
#[derive(Clone, Default)]
pub struct Int {
    min: Option<i64>,
    max: Option<i64>,
}

/// `z.number().int()`
pub fn integer() -> Int {
    Int::default()
}

impl Int {
    /// Valeur minimale (incluse).
    #[must_use]
    pub fn min(mut self, n: i64) -> Self {
        self.min = Some(n);
        self
    }

    /// Valeur maximale (incluse).
    #[must_use]
    pub fn max(mut self, n: i64) -> Self {
        self.max = Some(n);
        self
    }
}

impl Schema for Int {
    fn json(&self) -> Value {
        let mut schema = json!({ "type": "integer" });
        if let Some(min) = self.min {
            schema["minimum"] = json!(min);
        }
        if let Some(max) = self.max {
            schema["maximum"] = json!(max);
        }
        schema
    }

    fn validate(&self, key: &str, value: &Value, issues: &mut Issues) {
        // JSON Schema "type":"integer" accepte tout nombre mathématiquement
        // entier (1e2, 3.0) — pas seulement les i64 syntaxiques. Borne
        // stricte : ±2^63 (bornes i64 une fois arrondies en f64) est rejeté,
        // sinon le cast saturerait en i64::MIN/i64::MAX.
        let as_int = value.as_i64().or_else(|| {
            value
                .as_f64()
                .filter(|n| n.fract() == 0.0 && *n > i64::MIN as f64 && *n < i64::MAX as f64)
                .map(|n| n as i64)
        });
        match as_int {
            Some(n) => {
                if let Some(min) = self.min {
                    if n < min {
                        issues.push(format!("`{key}` : doit être ≥ {min}"));
                    }
                }
                if let Some(max) = self.max {
                    if n > max {
                        issues.push(format!("`{key}` : doit être ≤ {max}"));
                    }
                }
            }
            None => issues.push(format!("`{key}` : entier attendu")),
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
    Num::default()
}

impl Num {
    /// Valeur minimale (incluse).
    #[must_use]
    pub fn min(mut self, n: f64) -> Self {
        self.min = Some(n);
        self
    }

    /// Valeur maximale (incluse).
    #[must_use]
    pub fn max(mut self, n: f64) -> Self {
        self.max = Some(n);
        self
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

    fn validate(&self, key: &str, value: &Value, issues: &mut Issues) {
        match value.as_f64() {
            Some(n) => {
                if let Some(min) = self.min {
                    if n < min {
                        issues.push(format!("`{key}` : doit être ≥ {min}"));
                    }
                }
                if let Some(max) = self.max {
                    if n > max {
                        issues.push(format!("`{key}` : doit être ≤ {max}"));
                    }
                }
            }
            None => issues.push(format!("`{key}` : nombre attendu")),
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

impl Schema for Bool {
    fn json(&self) -> Value {
        json!({ "type": "boolean" })
    }

    fn validate(&self, key: &str, value: &Value, issues: &mut Issues) {
        if value.as_bool().is_none() {
            issues.push(format!("`{key}` : booléen attendu"));
        }
    }
}

/// `z.enum([...])` — chaîne parmi un ensemble fermé.
#[derive(Clone)]
pub struct Enum {
    values: Vec<String>,
}

/// `z.enum(["a", "b", ...])`
pub fn enumeration(values: &[&str]) -> Enum {
    Enum {
        values: values.iter().map(|v| (*v).to_string()).collect(),
    }
}

impl Schema for Enum {
    fn json(&self) -> Value {
        json!({ "type": "string", "enum": self.values })
    }

    fn validate(&self, key: &str, value: &Value, issues: &mut Issues) {
        match value.as_str() {
            Some(s) if self.values.iter().any(|v| v == s) => {}
            Some(_) => issues.push(format!(
                "`{key}` : doit être l'un de {}",
                self.values.join(", ")
            )),
            None => issues.push(format!("`{key}` : chaîne attendue")),
        }
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
        self
    }

    /// Nombre maximal d'éléments.
    #[must_use]
    pub fn max(mut self, n: usize) -> Self {
        self.max = Some(n);
        self
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

    fn validate(&self, key: &str, value: &Value, issues: &mut Issues) {
        match value.as_array() {
            Some(items) => {
                if let Some(min) = self.min {
                    if items.len() < min {
                        issues.push(format!("`{key}` : au moins {min} élément(s)"));
                    }
                }
                if let Some(max) = self.max {
                    if items.len() > max {
                        issues.push(format!("`{key}` : au plus {max} élément(s)"));
                    }
                }
                for (i, item) in items.iter().enumerate() {
                    self.inner.validate(&format!("{key}[{i}]"), item, issues);
                }
            }
            None => issues.push(format!("`{key}` : tableau attendu")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issues_of(schema: &dyn Schema, value: Value) -> Vec<String> {
        let mut issues = Issues::new();
        schema.validate("arguments", &value, &mut issues);
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
                    "a": { "type": "integer" },
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
        assert!(issues[1].contains("requis"));
    }

    #[test]
    fn strict_mode_rejects_unknown_fields() {
        let schema = object().field("a", integer()).strict();
        let issues = issues_of(&schema, json!({ "a": 1, "zzz": true }));
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("champ inconnu"));
    }

    #[test]
    fn non_object_arguments_are_rejected() {
        let schema = object().field("a", integer());
        let issues = issues_of(&schema, json!("not an object"));
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("objet attendu"));
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
        schema.validate("arguments", &value, &mut issues);
        assert_eq!(issues.render().len(), MAX_ISSUES + 1);
        assert!(issues.render().last().unwrap().contains("omis"));
        assert!(!issues.is_empty());
    }

    #[test]
    fn integer_rejects_values_outside_i64_range() {
        let schema = object().field("n", integer());
        // 2^63 n'est pas représentable en i64 : rejeté, jamais saturé en i64::MAX.
        let issues = issues_of(&schema, json!({ "n": 9_223_372_036_854_775_808u64 }));
        assert_eq!(issues.len(), 1, "{issues:?}");
        // ±2^63 en f64 : rejetés aussi (le cast saturerait).
        let two_pow_63 = 2f64.powi(63);
        assert_eq!(issues_of(&schema, json!({ "n": two_pow_63 })).len(), 1);
        assert_eq!(issues_of(&schema, json!({ "n": -two_pow_63 })).len(), 1);
        // Les bornes exactes i64, elles, passent.
        assert!(issues_of(&schema, json!({ "n": i64::MAX })).is_empty());
        assert!(issues_of(&schema, json!({ "n": i64::MIN })).is_empty());
    }
}
