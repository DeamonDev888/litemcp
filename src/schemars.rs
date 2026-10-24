//! Adaptateur [`schemars`](https://crates.io/crates/schemars) — feature
//! opt-in `schemars`.
//!
//! N'importe quel type `#[derive(JsonSchema)]` devient un schéma
//! **exécuteur** litemcp :
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
    /// Validateur compilé depuis le même document — calculé une fois.
    validator: OnceLock<Validator>,
}

/// Validateur compilé — ou le diagnostic d'échec de compilation (un schéma
/// généré par schemars est valide en pratique ; si le document ne compile
/// pas, chaque validation rend UNE issue le disant au lieu d'accepter en
/// silence).
enum Validator {
    Compiled(::jsonschema::Validator),
    Broken(String),
}

/// Adapte un type `#[derive(JsonSchema)]` en schéma exécuteur litemcp.
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
            validator: OnceLock::new(),
        }
    }

    fn document(&self) -> &Value {
        self.document.get_or_init(|| {
            serde_json::to_value(::schemars::schema_for!(T))
                .unwrap_or_else(|_| serde_json::json!({ "type": "object" }))
        })
    }

    fn validator(&self) -> &Validator {
        self.validator
            .get_or_init(|| match ::jsonschema::validator_for(self.document()) {
                Ok(validator) => Validator::Compiled(validator),
                Err(error) => Validator::Broken(error.to_string()),
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

    fn validate(&self, path: &mut Path, value: &Value, issues: &mut Issues) {
        match self.validator() {
            Validator::Broken(message) => issues.push(format!(
                "`{}` : schéma interne incompilable ({message})",
                path.as_str()
            )),
            Validator::Compiled(validator) => {
                for error in validator.iter_errors(value) {
                    // Pointeur d'instance ("/a/0/b") → suffixe lisible
                    // (".a[0].b"), greffé sur le chemin litemcp courant.
                    let pointer = error.instance_path().to_string();
                    let location = if pointer.is_empty() {
                        path.as_str().to_string()
                    } else {
                        format!("{}{}", path.as_str(), pointer_to_suffix(&pointer))
                    };
                    issues.push(format!("`{location}` : {error}"));
                }
            }
        }
    }
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
