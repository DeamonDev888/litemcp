//! Adaptateur [`schemars`](https://crates.io/crates/schemars) — feature
//! opt-in `schemars`.
//!
//! N'importe quel type `#[derive(JsonSchema)]` devient un schéma
//! **exécuteur** LiteMCP :
//! [`from_schemars::<T>()`](crate::schemars::from_schemars) s'utilise partout où
//! l'API attend `impl zod::Schema` (`tool_typed`, `tool_parse`,
//! `prompt_typed`, `output_schema`).
//!
//! - **Publication** : le JSON Schema est celui que `schemars` dérive de
//!   votre type (`tools/list` le montre tel quel) ;
//! - **Validation** : le **même document** est compilé par le validateur
//!   [`jsonschema`](https://crates.io/crates/jsonschema) et appliqué avant
//!   l'exécution du handler — hors schéma → `-32602` avec les problèmes
//!   dans `error.data.issues`, bornés par [`crate::zod::MAX_ISSUES`].
//!   Les `format` (`email`, `uuid`, `date-time`, `uri`…) sont **appliqués**
//!   (le validateur est compilé avec `should_validate_formats(true)` — par
//!   défaut `format` n'est qu'une annotation JSON Schema 2020-12 et
//!   serait ignoré).
//!
//! La promesse tient donc aussi ici : ce qui est publié est exactement ce
//! qui est validé. Notez que les **sémantiques** sont celles de JSON Schema
//! (le validateur officiel), pas celles du module [`crate::zod`] : par
//! exemple `type: "integer"` y accepte `3.0` — si vous voulez la garantie
//! `as_i64()` stricte de `zod::integer()`, restez sur le module maison.
//!
//! ```ignore
//! use litemcp::schemars::from_schemars;
//! use schemars::JsonSchema;
//!
//! #[derive(JsonSchema)]
//! struct SearchArgs {
//!     query: String,
//!     limit: Option<u8>,
//! }
//!
//! ServerBuilder::new("recherche", "1.0.0")
//!     .tool_typed("search", "Recherche", from_schemars::<SearchArgs>(), |args| {
//!         // arguments validés par le schéma dérivé du struct
//! #         let _ = args; Ok(serde_json::json!({}))
//!     })
//!     .bind("127.0.0.1:8080")?
//!     .serve()?;
//! ```
//!
//! Coût : deux dépendances de plus (`schemars` pour la dérivation,
//! `jsonschema` pour l'exécution) — **uniquement** si vous activez la
//! feature ; le cœur du crate garde exactement `serde` + `serde_json`.

use crate::zod::{Issues, Path, Schema};
use serde_json::Value;
use std::marker::PhantomData;
use std::sync::OnceLock;

/// Un schéma construit depuis un **document JSON Schema brut**.
///
/// Pour les schémas **exotiques** que le DSL [`crate::zod`] n'exprime pas
/// (`oneOf`, `anyOf`, `not`, `patternProperties`, `if`/`then`/`else`,
/// `$ref` internes…) : on écrit le document à la main et on le branche
/// comme n'importe quel autre schéma.
///
/// ```ignore
/// use litemcp::schemars::from_json_schema;
/// use serde_json::json;
///
/// let schema = from_json_schema(json!({
///     "type": "object",
///     "properties": { "x": { "type": "string" } },
///     "required": ["x"]
/// }));
///
/// ServerBuilder::new("app", "1.0.0")
///     .tool_typed("t", "t", schema, |args| Ok(args.clone()))
///     .bind("127.0.0.1:8080")?
///     .serve()?;
/// ```
///
/// Le document est **publié tel quel** et **appliqué** par le validateur
/// [`jsonschema`] — la promesse « le schéma publié est exécuté » tient.
///
/// Le document doit être un **objet** (ou un booléen JSON Schema) : toute
/// autre forme est refusée à la construction (erreur de programmation).
pub struct FromJsonSchema {
    document: Value,
    validator: OnceLock<Validator>,
}

/// Validateur compilé — ou le diagnostic d'échec de compilation.
enum Validator {
    Compiled(::jsonschema::Validator),
    Broken(String),
}

/// Construit un schéma exécuteur depuis un **document JSON Schema brut**.
///
/// Le document est **publié tel quel** dans l'`inputSchema` et **appliqué**
/// par le validateur [`jsonschema`].
///
/// # Panics
///
/// Si le document n'est ni un objet ni un booléen JSON Schema — une
/// erreur de programmation (un document invalide ne peut pas être
/// publié).
#[must_use]
pub fn from_json_schema(document: Value) -> FromJsonSchema {
    assert!(
        document.is_object() || document.is_boolean(),
        "litemcp/schemars : document JSON Schema invalide — un objet ou un \
         booléen est attendu (reçu : {})",
        if document.is_null() {
            "null".to_string()
        } else {
            document.to_string()
        }
    );
    FromJsonSchema {
        document,
        validator: OnceLock::new(),
    }
}

impl FromJsonSchema {
    /// Le document JSON Schema publié.
    #[must_use]
    pub fn document(&self) -> &Value {
        &self.document
    }

    fn validator(&self) -> &Validator {
        self.validator.get_or_init(|| {
            match ::jsonschema::options()
                .should_validate_formats(true)
                .build(&self.document)
            {
                Ok(validator) => Validator::Compiled(validator),
                Err(error) => Validator::Broken(error.to_string()),
            }
        })
    }
}

impl Schema for FromJsonSchema {
    fn json(&self) -> Value {
        self.document.clone()
    }

    fn validate(&self, path: &mut Path, value: &Value, issues: &mut Issues) {
        match self.validator() {
            Validator::Broken(message) => issues.push(format!(
                "`{}`: invalid internal schema ({message})",
                path.as_str()
            )),
            Validator::Compiled(validator) => {
                for error in validator.iter_errors(value) {
                    if issues.is_full() {
                        issues.record_overflow();
                        continue;
                    }
                    let pointer = error.instance_path().to_string();
                    let location = if pointer.is_empty() {
                        path.as_str().to_string()
                    } else {
                        format!("{}{}", path.as_str(), pointer_to_suffix(&pointer))
                    };
                    issues.push(format!("`{location}`: {error}"));
                }
            }
        }
    }
}

/// Adaptateur « schéma exécuteur » pour un type `schemars`.
///
/// Construit-le avec [`from_schemars::<T>()`] — le schéma JSON et le
/// validateur sont calculés **une fois** au premier usage puis cachés.
pub struct FromSchemars<T: ?Sized> {
    /// `fn() -> T` : rend l'adaptateur `Send + Sync + 'static` sans exiger
    /// `T: Send + Sync`.
    _type: PhantomData<fn() -> T>,
    /// Schéma JSON dérivé par schemars — calculé une fois.
    document: OnceLock<Value>,
    /// Validateur compilé **avec les `format` activés** — calculé une fois.
    /// `format` est une annotation en JSON Schema 2020-12 : sans
    /// activation explicite, le validateur l'ignore et le schéma
    /// publié ne serait pas appliqué (promesse « exécuteur » rompue).
    validator_with_formats: OnceLock<Validator>,
}

/// Adapte un type `#[derive(JsonSchema)]` en schéma exécuteur LiteMCP.
///
/// ```ignore
/// use litemcp::schemars::from_schemars;
/// builder.tool_typed("search", "…", from_schemars::<SearchArgs>(), handler)
/// ```
#[must_use]
pub fn from_schemars<T: ::schemars::JsonSchema + 'static>() -> FromSchemars<T> {
    FromSchemars::new()
}

impl<T: ::schemars::JsonSchema + 'static> FromSchemars<T> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            _type: PhantomData,
            document: OnceLock::new(),
            validator_with_formats: OnceLock::new(),
        }
    }

    fn document(&self) -> &Value {
        self.document.get_or_init(|| {
            serde_json::to_value(::schemars::schema_for!(T))
                .unwrap_or_else(|_| serde_json::json!({ "type": "object" }))
        })
    }

    /// Recompile le validateur avec l'**activation des `format`**.
    ///
    /// Par défaut, `jsonschema` **ignore** les `format` (annotation
    /// JSON Schema 2020-12) : un `format: "email"` publié par `schemars`
    /// serait donc publié mais **jamais appliqué** — le schéma cesserait
    /// d'être exécuteur, contrairement à la promesse de LiteMCP et du
    /// module [`crate::zod`] (où `.email()` publie **et** applique).
    ///
    /// `should_validate_formats(true)` rend le validateur conforme au
    /// mot-clé : `email`, `uuid`, `date-time`, `uri`… sont alors vérifiés
    /// comme ils le sont dans [`crate::zod`].
    ///
    /// Les `format` **inconnus** restent ignorés (`should_ignore_unknown_formats`
    /// par défaut à `true`) : un format exotique ne rend pas un schéma
    /// inutilisable, il est simplement non vérifié.
    fn validator_with_formats(&self) -> &Validator {
        self.validator_with_formats.get_or_init(|| {
            let document = self.document();
            match ::jsonschema::options()
                .should_validate_formats(true)
                .build(document)
            {
                Ok(validator) => Validator::Compiled(validator),
                Err(error) => Validator::Broken(error.to_string()),
            }
        })
    }
}

impl<T: ::schemars::JsonSchema + 'static> Default for FromSchemars<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: ::schemars::JsonSchema + 'static> Schema for FromSchemars<T> {
    fn json(&self) -> Value {
        self.document().clone()
    }

    // **Le défaut publié est APPLIÉ** (règle « publié = appliqué ») :
    // `schemars` publie `"default"` pour un champ `#[serde(default)]` et le
    // retire de `required` — sans injection, le handler recevrait un objet
    // privé du champ que le schéma annonce couvrir. La valeur vit dans
    // `properties.<nom>.default` du document ; l'injection est récursive
    // (objets imbriqués, tableaux d'objets), guidée par le même document que
    // celui publié dans `tools/list`.
    fn default_value(&self) -> Option<&Value> {
        self.document().get("default")
    }

    fn apply_defaults(&self, value: &mut Value) {
        let document = self.document();
        let Some(properties) = document.get("properties").and_then(Value::as_object) else {
            return;
        };
        apply_defaults_into(document, properties, value, 0);
    }

    fn has_defaults(&self) -> bool {
        // Échappatoire zéro-allocation : parcourir le document publié (une
        // fois caché par `document()`), sans toucher aux arguments. Les
        // `$ref` locaux sont résolus (un sous-objet peut porter ses propres
        // défauts derrière un `#/$defs/…`).
        let document = self.document();
        let Some(properties) = document.get("properties").and_then(Value::as_object) else {
            return document_has_default(document);
        };
        properties.values().any(|property| {
            let resolved = resolve_local_ref(document, property);
            document_has_default(resolved)
        }) || document_has_default(document)
    }

    fn validate(&self, path: &mut Path, value: &Value, issues: &mut Issues) {
        // Le validateur **avec `format` activé** : la promesse de litemcp
        // est que le schéma publié est exécuté — or `format` est une
        // simple annotation en JSON Schema 2020-12, ignorée par défaut
        // par le validateur. Sans cette activation, un `format: "email"`
        // serait publié mais jamais appliqué (le schéma mentirait).
        match self.validator_with_formats() {
            Validator::Broken(message) => issues.push(format!(
                "`{}`: invalid internal schema ({message})",
                path.as_str()
            )),
            Validator::Compiled(validator) => {
                for error in validator.iter_errors(value) {
                    if issues.is_full() {
                        issues.record_overflow();
                        continue;
                    }
                    // Pointeur d'instance ("/a/0/b") → suffixe lisible
                    // (".a[0].b"), greffé sur le chemin litemcp courant.
                    let pointer = error.instance_path().to_string();
                    let location = if pointer.is_empty() {
                        path.as_str().to_string()
                    } else {
                        format!("{}{}", path.as_str(), pointer_to_suffix(&pointer))
                    };
                    issues.push(format!("`{location}`: {error}"));
                }
            }
        }
    }
}

/// Profondeur maximale de résolution des `$ref` locaux — borne la
/// récursion (un schéma auto-référent ne doit pas boucler).
const MAX_REF_DEPTH: usize = 16;

/// Résout un `$ref` **local** (`#/$defs/…`, `#/definitions/…`) vers le
/// sous-document qu'il désigne.
///
/// Seules les références internes au document publié sont résolues : aucune
/// résolution réseau, aucune référence externe (SEP-2106 — un `$ref` distant
/// ne doit jamais déclencher de requête).
fn resolve_local_ref<'a>(document: &'a Value, node: &'a Value) -> &'a Value {
    let Some(reference) = node.get("$ref").and_then(Value::as_str) else {
        return node;
    };
    let Some(pointer) = reference.strip_prefix('#') else {
        // Référence externe : laissée telle quelle (jamais résolue).
        return node;
    };
    // Pointeur JSON : `#/$defs/Nested` → `$defs` → `Nested`.
    let mut current = document;
    for segment in pointer.split('/').filter(|s| !s.is_empty()) {
        let segment = segment.replace("~1", "/").replace("~0", "~");
        match current.get(&segment) {
            Some(next) => current = next,
            None => return node,
        }
    }
    current
}

/// Injecte les défauts publiés dans `value` (objet racine d'arguments),
/// d'après les `properties` du document.
fn apply_defaults_into(
    document: &Value,
    properties: &serde_json::Map<String, Value>,
    value: &mut Value,
    depth: usize,
) {
    if depth > MAX_REF_DEPTH {
        return;
    }
    let Some(object) = value.as_object_mut() else {
        return;
    };
    for (name, raw_property) in properties {
        let property = resolve_local_ref(document, raw_property);
        match object.get_mut(name) {
            // Champ présent : descendre (l'objet imbriqué porte ses propres
            // défauts).
            Some(child) => apply_defaults_document(document, property, child, depth + 1),
            // Champ absent : injecter le défaut publié, puis le valider
            // contre sa sous-structure (un défaut objet peut porter ses
            // propres défauts imbriqués).
            None => {
                if let Some(default) = property.get("default") {
                    let mut injected = default.clone();
                    apply_defaults_document(document, property, &mut injected, depth + 1);
                    object.insert(name.clone(), injected);
                }
            }
        }
    }
}

/// Injecte récursivement les défauts d'un **sous-document** dans une valeur
/// déjà présente ou fraîchement injectée — objets (`properties`) et tableaux
/// (`items`). Guidée par le même document que celui publié.
fn apply_defaults_document(document: &Value, property: &Value, value: &mut Value, depth: usize) {
    if depth > MAX_REF_DEPTH {
        return;
    }
    // Objet : chaque propriété absente portant un `default` est injectée.
    if let Some(object) = value.as_object_mut() {
        let resolved = resolve_local_ref(document, property);
        if let Some(properties) = resolved.get("properties").and_then(Value::as_object) {
            for (name, raw_sub) in properties {
                let sub = resolve_local_ref(document, raw_sub);
                match object.get_mut(name) {
                    Some(child) => apply_defaults_document(document, sub, child, depth + 1),
                    None => {
                        if let Some(default) = sub.get("default") {
                            let mut injected = default.clone();
                            apply_defaults_document(document, sub, &mut injected, depth + 1);
                            object.insert(name.clone(), injected);
                        }
                    }
                }
            }
        }
        return;
    }
    // Tableau : le schéma `items` s'applique à chaque élément.
    if let Some(array) = value.as_array_mut() {
        let resolved = resolve_local_ref(document, property);
        if let Some(items) = resolved.get("items") {
            for element in array {
                apply_defaults_document(document, items, element, depth + 1);
            }
        }
    }
}

/// Le document (ou l'un de ses sous-schémas) porte-t-il un mot-clé `default` ?
/// Recherche récursive bornée aux mots-clés de structure (`properties`,
/// `items`, `additionalProperties`, les combinateurs), jamais sur les valeurs
/// utilisateur : un `default: {"properties": ...}` littéral n'est pas suivi.
fn document_has_default(document: &Value) -> bool {
    document.as_object().is_some_and(object_has_default)
}

/// Variante sur la carte (évite tout clone dans la récursion).
///
/// Les mots-clés de structure n'ont pas tous la même forme :
/// - `properties` et `patternProperties` sont des **cartes nom → schéma**
///   (chaque VALEUR est un schéma) ;
/// - `items` et `additionalProperties` sont un schéma (ou un tableau de
///   schémas pour `items`) ;
/// - `allOf`/`anyOf`/`oneOf` sont des **tableaux de schémas** ;
/// - `not` est un schéma.
fn object_has_default(object: &serde_json::Map<String, Value>) -> bool {
    if object.contains_key("default") {
        return true;
    }
    // `properties` / `patternProperties` : chaque valeur est un schéma.
    for key in ["properties", "patternProperties"] {
        if let Some(map) = object.get(key).and_then(Value::as_object) {
            if map.values().any(document_has_default) {
                return true;
            }
        }
    }
    // `items` (schéma ou tableau de schémas) et `additionalProperties`.
    for key in ["items", "additionalProperties"] {
        if let Some(sub) = object.get(key) {
            if document_has_default(sub) {
                return true;
            }
        }
    }
    // Combinateurs : tableaux de schémas.
    for key in ["allOf", "anyOf", "oneOf"] {
        if let Some(items) = object.get(key).and_then(Value::as_array) {
            if items.iter().any(document_has_default) {
                return true;
            }
        }
    }
    // `not` : un schéma.
    if let Some(sub) = object.get("not") {
        if document_has_default(sub) {
            return true;
        }
    }
    false
}

/// `/a/0/b` → `.a[0].b` (segments numériques en index).
fn pointer_to_suffix(pointer: &str) -> String {
    let mut suffix = String::new();
    for segment in pointer.split('/').skip(1) {
        if segment.parse::<usize>().is_ok() && !segment.is_empty() {
            suffix.push('[');
            suffix.push_str(segment);
            suffix.push(']');
        } else {
            suffix.push('.');
            // ~1 / ~0 : échappement JSON Pointer pour / et ~.
            suffix.push_str(&segment.replace("~1", "/").replace("~0", "~"));
        }
    }
    suffix
}
