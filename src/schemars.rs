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
//!   votre type (`tools/list` le montre tel quel, à une exception près :
//!   `schemars` omet certaines bornes d'entier — `maximum` de `uint32`,
//!   `uint64`, `uint`, et les deux bornes de `int32`, `int64`, `int` —
//!   l'adaptateur les complète, sans écraser une borne déjà publiée, pour
//!   que le schéma publié soit celui appliqué) ;
//! - **Défauts** : un `default` publié est **appliqué** — mais jamais dans
//!   une branche `anyOf`/`oneOf` que l'instance ne sélectionne pas : le
//!   choix de branche précède l'injection, une instance valide avant
//!   injection le reste après ;
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
//!     .bind("127.0.0.1:23080")?
//!     .serve()?;
//! ```
//!
//! Coût : deux dépendances de plus (`schemars` pour la dérivation,
//! `jsonschema` pour l'exécution) — **uniquement** si vous activez la
//! feature ; le cœur du crate garde exactement `serde` + `serde_json`.

use crate::zod::{Issues, Path, Schema};
use serde_json::Value;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::{Mutex, OnceLock};

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
///     .bind("127.0.0.1:23080")?
///     .serve()?;
/// ```
///
/// Le document est **publié tel quel** et **appliqué** par le validateur
/// [`jsonschema`] — la promesse « le schéma publié est exécuté » tient.
///
/// Le document doit être un **objet** (ou un booléen JSON Schema) : toute
/// autre forme — et tout document qui ne **compile pas** — est refusé à la
/// construction (erreur de programmation).
pub struct FromJsonSchema {
    document: Value,
    /// Validateur compilé **une fois, à la construction** : un document
    /// invalide ne peut plus être enregistré puis transformé en erreur
    /// `-32602` à chaque appel.
    validator: Validator,
    /// Branches `anyOf`/`oneOf` déjà compilées (sélection avant injection,
    /// voir [`branch_accepts`]) — clé = adresse du nœud publié.
    branch_validators: OnceLock<BranchCache>,
}

/// Validateur compilé — ou le diagnostic d'échec de compilation.
///
/// Le diagnostic n'est **pas conservé** : le message rendu au client est
/// neutre par construction (aucune URL de schéma, aucun nom de feature ne
/// peut fuir).
enum Validator {
    Compiled(::jsonschema::Validator),
    Broken,
}

/// Cache des validateurs de branches `anyOf`/`oneOf` — clé = adresse du
/// nœud dans le document publié (stable : le document est immuable).
type BranchCache = Mutex<HashMap<usize, Option<::jsonschema::Validator>>>;

/// Construit un schéma exécuteur depuis un **document JSON Schema brut**.
///
/// Le document est **publié tel quel** dans l'`inputSchema` et **appliqué**
/// par le validateur [`jsonschema`]. Le validateur est compilé **ici** : un
/// document mal formé (type inconnu, `$ref` local absent, `$ref` distant)
/// est refusé à la construction, au lieu de rendre chaque appel fautif.
///
/// # Panics
///
/// Si le document n'est ni un objet ni un booléen JSON Schema, ou s'il ne
/// **compile pas** — une erreur de programmation : un document invalide ne
/// peut pas être publié, et l'annoncer après coup en `-32602` imputerait au
/// client une faute du serveur.
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
    match ::jsonschema::options()
        .should_validate_formats(true)
        .build(&document)
    {
        Ok(validator) => FromJsonSchema {
            document,
            validator: Validator::Compiled(validator),
            branch_validators: OnceLock::new(),
        },
        Err(error) => panic!(
            "litemcp/schemars : document JSON Schema invalide — il doit compiler pour être \
             publié et appliqué (erreur : {error} ; document : {document})"
        ),
    }
}

impl FromJsonSchema {
    /// Le document JSON Schema publié.
    #[must_use]
    pub fn document(&self) -> &Value {
        &self.document
    }
}

impl Schema for FromJsonSchema {
    fn json(&self) -> Value {
        self.document.clone()
    }

    // **Le défaut publié est APPLIÉ** — exactement la même politique que
    // [`FromSchemars`] : les deux adaptateurs délèguent au même moteur
    // ([`schema_has_default`] pour la détection, [`apply_node_defaults`]
    // pour l'injection). Un `default` publié dans un document écrit à la
    // main est annoncé dans `tools/list` **et** injecté dans les arguments
    // absents, récursivement (objets imbriqués, tableaux, branches
    // `allOf`, et branche réellement sélectionnée d'un `anyOf`/`oneOf`),
    // avec les `$ref` locaux chaînés résolus (détection de cycles + borne
    // [`MAX_REF_DEPTH`]). Avant cette mutualisation, le défaut brut était
    // publié mais jamais appliqué — la promesse « publié = appliqué » était
    // rompue sur cette voie.
    fn default_value(&self) -> Option<&Value> {
        effective_root(&self.document).get("default")
    }

    fn apply_defaults(&self, value: &mut Value) {
        let branch_validators = self.branch_validators.get_or_init(BranchCache::default);
        apply_node_defaults(
            &self.document,
            &self.document,
            value,
            0,
            &|branch, instance| branch_accepts(branch_validators, &self.document, branch, instance),
        );
    }

    fn has_defaults(&self) -> bool {
        schema_has_default(&self.document, &self.document, 0, &mut Vec::new())
    }

    fn has_integers(&self) -> bool {
        schema_has_integer(&self.document, &self.document, 0, &mut Vec::new())
    }

    fn normalize_integers(&self, value: &mut Value) {
        normalize_node_integers(&self.document, &self.document, value, 0, &mut Vec::new());
    }

    fn validate(&self, path: &mut Path, value: &Value, issues: &mut Issues) {
        match &self.validator {
            // Erreur **interne** : jamais imputée au client, jamais de détail
            // (URL de schéma, nom de feature) dans la réponse — le noyau
            // rend `-32603` avec un message neutre.
            Validator::Broken => issues.mark_internal(),
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
    /// Branches `anyOf`/`oneOf` déjà compilées (sélection avant injection,
    /// voir [`branch_accepts`]) — clé = adresse du nœud publié.
    branch_validators: OnceLock<BranchCache>,
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
            branch_validators: OnceLock::new(),
        }
    }

    fn document(&self) -> &Value {
        self.document.get_or_init(|| {
            let mut document = serde_json::to_value(::schemars::schema_for!(T))
                .unwrap_or_else(|_| serde_json::json!({ "type": "object" }));
            // `schemars` publie les `format` entiers en omettant certaines
            // bornes (`uint32`, `uint64`, `uint`, `int32`, `int64`, `int`) :
            // elles sont complétées pour que le schéma publié soit celui
            // réellement appliqué (sinon une valeur hors plage Rust passait
            // puis cassait à la désérialisation).
            publish_integer_bounds(&mut document);
            document
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
    /// par les validateurs de format **de la référence** (`jsonschema`).
    /// Le profil exact appliqué par le DSL [`crate::zod`] — parfois plus
    /// strict que la référence — est documenté sur [`crate::zod::Format`]
    /// et vérifié en différentiel (`tests/zod_diff_jsonschema.rs`).
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
                Err(_) => Validator::Broken,
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
    // `properties.<nom>.default` du document ; détection et injection
    // délèguent au **même moteur** que [`FromJsonSchema`] (même parcours,
    // même résolution des `$ref` locaux chaînés, même borne) — la détection
    // ne peut donc jamais désactiver une injection que l'injection aurait
    // faite.
    fn default_value(&self) -> Option<&Value> {
        effective_root(self.document()).get("default")
    }

    fn apply_defaults(&self, value: &mut Value) {
        let branch_validators = self.branch_validators.get_or_init(BranchCache::default);
        apply_node_defaults(
            self.document(),
            self.document(),
            value,
            0,
            &|branch, instance| {
                branch_accepts(branch_validators, self.document(), branch, instance)
            },
        );
    }

    fn has_defaults(&self) -> bool {
        schema_has_default(self.document(), self.document(), 0, &mut Vec::new())
    }

    fn has_integers(&self) -> bool {
        schema_has_integer(self.document(), self.document(), 0, &mut Vec::new())
    }

    fn normalize_integers(&self, value: &mut Value) {
        normalize_node_integers(self.document(), self.document(), value, 0, &mut Vec::new());
    }

    fn validate(&self, path: &mut Path, value: &Value, issues: &mut Issues) {
        // Le validateur **avec `format` activé** : la promesse de litemcp
        // est que le schéma publié est exécuté — or `format` est une
        // simple annotation en JSON Schema 2020-12, ignorée par défaut
        // par le validateur. Sans cette activation, un `format: "email"`
        // serait publié mais jamais appliqué (le schéma mentirait).
        match self.validator_with_formats() {
            // Erreur **interne** (schéma dérivé incompilable) : `-32603`
            // neutre côté noyau, jamais `-32602` ni de détail interne.
            Validator::Broken => issues.mark_internal(),
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

/// Profondeur maximale des parcours de schéma — borne la récursion de
/// détection/injection (un schéma auto-référent ne doit ni boucler ni
/// déborder) et, via le budget interne du résolveur, le nombre de `$ref`
/// suivis **par chaîne de résolution** (comportement défini au-delà : la
/// résolution s'arrête, le nœud courant est rendu tel quel — jamais une
/// panique ni un hang ; le travail total reste borné par les caps de
/// profondeur et la détection de cycles des parcours).
const MAX_REF_DEPTH: usize = 16;

/// Adresse mémoire d'un nœud — identité structurelle pour la détection de
/// cycles (le même sous-document revisité sur un chemin est un cycle).
fn node_address(node: &Value) -> usize {
    node as *const Value as usize
}

/// Résout une **chaîne** de `$ref` locaux (`#/$defs/…`, `#/definitions/…`)
/// vers le sous-document final : chaque cible atteinte qui porte elle-même
/// un `$ref` est résolue à son tour, jusqu'au premier nœud sans référence.
///
/// Le budget de sauts est **interne** : chaque chaîne résolue suit au plus
/// [`MAX_REF_DEPTH`] références, puis rend le nœud courant tel quel — ses
/// autres mots-clés (s'il en porte à côté du `$ref`) restent visibles aux
/// parcours. Le travail total d'un parcours reste borné par les caps de
/// profondeur et la détection de cycles des marcheurs appelants.
///
/// Garanties :
/// - **cycle** : un nœud déjà visité sur la chaîne courante (comparaison
///   d'adresses) arrête la résolution — `A → B → A` rend `A` sans boucler ;
/// - **borne** : budget épuisé, la résolution s'arrête (comportement défini
///   et testé, jamais une panique ni un hang) ;
/// - **localité** : seules les références internes au document publié sont
///   résolues — aucune résolution réseau, aucune référence externe
///   (SEP-2106 : un `$ref` distant ne doit jamais déclencher de requête) ;
///   une référence inconnue ou mal formée laisse le nœud tel quel.
fn resolve_local_ref<'a>(document: &'a Value, node: &'a Value) -> &'a Value {
    // Budget **interne** : chaque chaîne de résolution suit au plus
    // [`MAX_REF_DEPTH`] références (le nœud courant est rendu tel quel
    // au-delà — comportement défini, jamais un hang).
    let mut budget = MAX_REF_DEPTH;
    let mut current = node;
    let mut seen: Vec<usize> = Vec::new();
    loop {
        let Some(reference) = current.get("$ref").and_then(Value::as_str) else {
            return current; // nœud final : plus aucun `$ref` sur la chaîne
        };
        let address = node_address(current);
        if seen.contains(&address) || budget == 0 {
            // Cycle dans la chaîne ou budget épuisé : s'arrêter ici. Le
            // nœud courant reste tel quel — ses autres mots-clés (s'il en
            // porte à côté du `$ref`) restent visibles aux parcours.
            return current;
        }
        seen.push(address);
        budget -= 1;
        let Some(pointer) = reference.strip_prefix('#') else {
            // Référence externe : laissée telle quelle (jamais résolue).
            return current;
        };
        // Pointeur JSON : `#/$defs/Nested` → `$defs` → `Nested`, avec
        // l'échappement `~1` (/) et `~0` (~).
        let mut target = document;
        let mut resolved = true;
        for segment in pointer.split('/').filter(|s| !s.is_empty()) {
            let segment = segment.replace("~1", "/").replace("~0", "~");
            match target.get(&segment) {
                Some(next) => target = next,
                None => {
                    resolved = false;
                    break;
                }
            }
        }
        if !resolved {
            return current;
        }
        current = target;
    }
}

/// Racine **effective** d'un document : la racine elle-même si elle ne porte
/// pas de `$ref`, sinon le sous-document qu'elle désigne (schemars publie
/// une racine `$ref` pour les types récursifs — détection, injection et
/// défaut racine doivent la résoudre).
fn effective_root(document: &Value) -> &Value {
    resolve_local_ref(document, document)
}

/// L'instance satisfait-elle déjà la **branche** `anyOf`/`oneOf` ?
///
/// L'injection de défauts ne doit toucher que les branches que l'instance
/// sélectionne : c'est le seul moyen de garantir qu'une instance **valide
/// avant** injection le reste **après** (le `default` d'une autre branche
/// est refusé par `additionalProperties: false`, ou par le `const` du
/// discriminant d'une énumération taguée).
///
/// Les validateurs de branches sont compilés **une fois** puis cachés (clé =
/// adresse du nœud publié, stable : le document est immuable) : le coût ne
/// pèse que sur les schémas qui portent réellement des `anyOf`/`oneOf` avec
/// des défauts. Une branche non compilable est traitée comme non
/// sélectionnée — on ne peut pas prouver que l'instance la satisfait, et
/// injecter à l'aveugle est exactement le défaut corrigé ici.
fn branch_accepts(cache: &BranchCache, document: &Value, branch: &Value, instance: &Value) -> bool {
    let key = node_address(branch);
    let mut guard = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let validator = guard
        .entry(key)
        .or_insert_with(|| compile_branch(document, branch));
    validator
        .as_ref()
        .is_some_and(|validator| validator.is_valid(instance))
}

/// Compile une branche **telle qu'elle est publiée**.
///
/// Les `$ref` locaux (`#/$defs/…`, `#/definitions/…`) pointent dans le
/// document complet : ses définitions sont recopiées à la racine de
/// compilation pour rester résolubles — aucune résolution réseau, aucun
/// accès au document original modifié.
fn compile_branch(document: &Value, branch: &Value) -> Option<::jsonschema::Validator> {
    let mut wrapper = branch.clone();
    if let Some(object) = wrapper.as_object_mut() {
        for key in ["$defs", "definitions"] {
            if let Some(definitions) = document.get(key) {
                object.insert(key.to_string(), definitions.clone());
            }
        }
    }
    ::jsonschema::options()
        .should_validate_formats(true)
        .build(&wrapper)
        .ok()
}

/// Bornes exactes du type Rust que `schemars` désigne par son `format`.
///
/// `schemars` 1.2.3 publie `format: "int8"` … `"uint64"`, plus `"int"` /
/// `"uint"` pour `isize`/`usize` (constaté par sonde : `uint8`, `uint16`,
/// `int8`, `int16` portent déjà leurs bornes ; `uint32`, `uint64`, `uint`,
/// `int32`, `int64` et `int` n'en publient aucune ou seulement `minimum: 0`).
/// Rend `(minimum, maximum)` en `i128` — `u64::MAX` y tient sans troncature.
/// `"uint"` et `"int"` suivent la **cible** (`usize`/`isize`), comme le
/// contrat de `zod::uint()`.
fn integer_format_bounds(format: &str) -> Option<(i128, i128)> {
    Some(match format {
        "int8" => (i128::from(i8::MIN), i128::from(i8::MAX)),
        "int16" => (i128::from(i16::MIN), i128::from(i16::MAX)),
        "int32" => (i128::from(i32::MIN), i128::from(i32::MAX)),
        "int64" => (i128::from(i64::MIN), i128::from(i64::MAX)),
        "int" => (isize::MIN as i128, isize::MAX as i128),
        "uint8" => (0, i128::from(u8::MAX)),
        "uint16" => (0, i128::from(u16::MAX)),
        "uint32" => (0, i128::from(u32::MAX)),
        "uint64" => (0, i128::from(u64::MAX)),
        "uint" => (0, usize::MAX as i128),
        _ => return None,
    })
}

/// Complète les bornes d'entier que `schemars` omet, pour **tous** les
/// `format` qu'il publie — pas seulement `uint64`.
///
/// Le schéma publié doit être celui réellement appliqué (même contrat que
/// `zod::uint()`/`zod::integer()`) : sans `maximum`, une valeur hors plage
/// Rust passait le schéma et cassait à la désérialisation au lieu de rendre
/// `-32602`. Une borne **déjà publiée** n'est jamais écrasée (un
/// `#[schemars(range(min = 1, max = 100))]` reste tel quel) : seule une
/// borne absente est ajoutée, et la borne ajoutée est toujours celle du
/// type Rust — le schéma utilisateur n'est ni élargi ni rétréci par
/// l'adaptateur (une borne publiée qui déborderait la plage du type est
/// laissée telle quelle : c'est le document de l'utilisateur).
fn publish_integer_bounds(node: &mut Value) {
    match node {
        Value::Object(object) => {
            if node_declares_integer(object) {
                if let Some((min, max)) = object
                    .get("format")
                    .and_then(Value::as_str)
                    .and_then(integer_format_bounds)
                {
                    insert_bound(object, "minimum", min);
                    insert_bound(object, "maximum", max);
                }
            }
            for child in object.values_mut() {
                publish_integer_bounds(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                publish_integer_bounds(item);
            }
        }
        _ => {}
    }
}

/// Pose `bound` sous `key` **si** la clé est absente. `u64::MAX` dépasse
/// `i64::MAX` : la valeur est construite en `u64` quand elle est positive,
/// en `i64` sinon — dans les deux cas un entier JSON exact (jamais un `f64`).
fn insert_bound(object: &mut serde_json::Map<String, Value>, key: &str, bound: i128) {
    object.entry(key.to_string()).or_insert_with(|| {
        u64::try_from(bound).map_or_else(|_| Value::from(bound as i64), Value::from)
    });
}

/// Applique les défauts publiés de `node` (résolu) dans `value` — objet
/// racine d'arguments, valeur déjà présente ou fraîchement injectée.
///
/// **Même parcours que [`schema_has_default`]** — mêmes mots-clés visités,
/// même résolveur (budget frais par résolution : la portée d'une chaîne est
/// déterministe, indépendante de l'ordre de visite) — et la détection
/// explore **tous** les nœuds atteignables : toute injection possible est
/// donc détectée, jamais désactivée :
/// - `properties` : chaque propriété absente portant un `default` est
///   injectée, chaque propriété présente est descendue ;
/// - `items` (forme objet) : le schéma s'applique à chaque élément ;
/// - `allOf` : toutes les branches s'appliquent — leurs défauts sont
///   injectés dans la même valeur ;
/// - `anyOf`/`oneOf` : **seule la branche que l'instance satisfait déjà**
///   reçoit ses défauts (`select`). Injecter dans toutes les branches
///   ajoutait le `default` d'une branche que l'instance ne sélectionne pas :
///   une instance **valide** devenait invalide (`additionalProperties:
///   false`) et le handler recevait un champ qu'il n'avait pas demandé.
///   Pour un `oneOf`, la branche est retenue seulement si elle est l'**unique**
///   à satisfaire l'instance (sinon l'instance est déjà invalide à ce nœud :
///   injecter ne peut rien réparer) ; pour un `anyOf`, la **première**
///   branche satisfaite (les injecter toutes produirait la même corruption).
///   Une branche non compilable n'est jamais retenue.
fn apply_node_defaults(
    document: &Value,
    node: &Value,
    value: &mut Value,
    depth: usize,
    select: &dyn Fn(&Value, &Value) -> bool,
) {
    if depth > MAX_REF_DEPTH {
        return;
    }
    let node = resolve_local_ref(document, node);
    // La résolution peut être coupée en pleine chaîne (budget par appel) :
    // re-prendre la suite avec un budget frais — la profondeur borne le
    // nombre de reprises. **Sans court-circuiter les frères** du nœud
    // courant (le `return` historique sautait `properties`/`items`/
    // combinateurs portés À CÔTÉ du `$ref`) : la détection les explore
    // toujours après sa propre reprise — un défaut publié dans ces frères
    // doit être injecté aussi (mêmes sous-schémas vus des deux côtés).
    if node.get("$ref").is_some() {
        apply_node_defaults(document, node, value, depth + 1, select);
    }
    let Some(object) = node.as_object() else {
        return; // schéma booléen / scalaire : rien à visiter
    };

    // Objet : champ présent → descente (il porte ses propres défauts) ;
    // champ absent + défaut publié → injection, puis re-descente sur la
    // valeur injectée (un défaut objet peut porter ses propres défauts).
    // Les enfants sont passés **bruts** (exactement comme la détection) : la
    // résolution se fait à l'entrée du parcours — pré-résoudre ici avancerait
    // la chaîne une première fois hors parcours (budget consommé avant
    // examen) et ferait perdre les frères du point de coupe, que la
    // détection, elle, voit.
    if let Some(map) = object.get("properties").and_then(Value::as_object) {
        if let Some(instance) = value.as_object_mut() {
            for (name, raw_child) in map {
                match instance.get_mut(name) {
                    Some(present) => {
                        apply_node_defaults(document, raw_child, present, depth + 1, select);
                    }
                    None => {
                        if let Some(default) = effective_default(document, raw_child, depth + 1) {
                            let mut injected = default.clone();
                            apply_node_defaults(
                                document,
                                raw_child,
                                &mut injected,
                                depth + 1,
                                select,
                            );
                            instance.insert(name.clone(), injected);
                        }
                    }
                }
            }
        }
    }

    // Tableau : le schéma `items` s'applique à chaque élément.
    if let Some(array) = value.as_array_mut() {
        if let Some(items) = object.get("items").filter(|items| items.is_object()) {
            for element in array {
                apply_node_defaults(document, items, element, depth + 1, select);
            }
        }
    }

    // `allOf` : toutes les branches s'appliquent à la même valeur — leurs
    // défauts sont tous injectés (aucune sélection à faire).
    if let Some(branches) = object.get("allOf").and_then(Value::as_array) {
        for branch in branches {
            apply_node_defaults(document, branch, value, depth + 1, select);
        }
    }

    // `anyOf`/`oneOf` : **choisir la branche avant d'injecter**. Une
    // instance valide reste valide (aucun défaut d'une branche qu'elle ne
    // sélectionne pas) et le handler ne reçoit que les défauts de la
    // branche réellement applicable.
    for key in ["anyOf", "oneOf"] {
        if let Some(branches) = object.get(key).and_then(Value::as_array) {
            let selected = if key == "oneOf" {
                // `oneOf` : la branche unique qui satisfait l'instance.
                let accepting: Vec<&Value> = branches
                    .iter()
                    .filter(|branch| select(branch, value))
                    .collect();
                (accepting.len() == 1).then_some(accepting[0])
            } else {
                // `anyOf` : la première branche satisfaite (les injecter
                // toutes reproduirait la corruption d'origine).
                branches.iter().find(|branch| select(branch, value))
            };
            if let Some(branch) = selected {
                apply_node_defaults(document, branch, value, depth + 1, select);
            }
        }
    }
}

/// Le défaut **effectif** d'un nœud : son propre `default`, ou — si la
/// résolution a été coupée en pleine chaîne de `$ref` — celui du premier
/// nœud atteint en reprenant la chaîne avec un budget frais (borné par la
/// profondeur : un cycle rend `None` de façon déterministe).
fn effective_default<'a>(document: &'a Value, node: &'a Value, depth: usize) -> Option<&'a Value> {
    if depth > MAX_REF_DEPTH {
        return None;
    }
    let node = resolve_local_ref(document, node);
    if let Some(default) = node.get("default") {
        return Some(default);
    }
    if node.get("$ref").is_some() {
        return effective_default(document, node, depth + 1);
    }
    None
}

/// Le schéma `node` (ou l'un de ses sous-schémas **visitables par
/// l'injection**) porte-t-il un mot-clé `default` ?
///
/// **Même parcours que [`apply_node_defaults`]** — mêmes mots-clés
/// (`properties`, `items` objet, `allOf`/`anyOf`/`oneOf`), même résolveur
/// chaîné, même borne, même détection de cycles : une injection possible est
/// toujours détectée (jamais de `has_defaults() == false` là où l'injection
/// agirait), et rien d'ininjectable n'est annoncé. Jamais sur les valeurs
/// utilisateur : un `default: {"properties": …}` littéral n'est pas suivi
/// (le mot-clé est testé par présence, jamais traversé).
fn schema_has_default(document: &Value, node: &Value, depth: usize, path: &mut Vec<usize>) -> bool {
    if depth > MAX_REF_DEPTH {
        return false;
    }
    let node = resolve_local_ref(document, node);
    if path.contains(&node_address(node)) {
        return false; // cycle du schéma : déjà en cours d'inspection
    }
    let Some(object) = node.as_object() else {
        return false;
    };
    if object.contains_key("default") {
        return true;
    }
    // La résolution peut être coupée en pleine chaîne (budget par appel) :
    // le nœud courant porte encore un `$ref` — le revisiter avec un budget
    // frais (la profondeur borne le nombre de reprises ; un cycle rend
    // `false` de façon déterministe).
    if object.contains_key("$ref") && schema_has_default(document, node, depth + 1, path) {
        return true;
    }
    path.push(node_address(node));
    let mut found = false;

    // `properties` : chaque valeur est un schéma.
    if let Some(map) = object.get("properties").and_then(Value::as_object) {
        for child in map.values() {
            if schema_has_default(document, child, depth + 1, path) {
                found = true;
                break;
            }
        }
    }
    // `items` (forme objet — même filtre que l'injection).
    if !found {
        if let Some(items) = object.get("items").filter(|items| items.is_object()) {
            found = schema_has_default(document, items, depth + 1, path);
        }
    }
    // Combinateurs : tableaux de schémas. La détection explore **toutes**
    // les branches — sur-ensemble de ce que l'injection retient (elle ne
    // choisit que la branche que l'instance satisfait), donc une injection
    // possible n'est jamais désactivée.
    if !found {
        for key in ["allOf", "anyOf", "oneOf"] {
            if let Some(branches) = object.get(key).and_then(Value::as_array) {
                for branch in branches {
                    if schema_has_default(document, branch, depth + 1, path) {
                        found = true;
                        break;
                    }
                }
                if found {
                    break;
                }
            }
        }
    }

    path.pop();
    found
}

/// Le nœud de schéma (ou l'un de ses sous-schémas) déclare-t-il un type
/// **entier** ? **Même parcours que [`schema_has_default`]** — mêmes
/// mots-clés, même résolveur, même borne, même détection de cycles — de
/// sorte que la détection ne puisse jamais désactiver une normalisation
/// que [`normalize_node_integers`] aurait faite. Les `$ref` sont suivis.
fn schema_has_integer(document: &Value, node: &Value, depth: usize, path: &mut Vec<usize>) -> bool {
    if depth > MAX_REF_DEPTH {
        return false;
    }
    let node = resolve_local_ref(document, node);
    if path.contains(&node_address(node)) {
        return false; // cycle du schéma : déjà en cours d'inspection
    }
    let Some(object) = node.as_object() else {
        return false;
    };
    if node_declares_integer(object) {
        return true;
    }
    path.push(node_address(node));
    let mut found = false;

    // `properties` : chaque valeur est un schéma.
    if let Some(map) = object.get("properties").and_then(Value::as_object) {
        for child in map.values() {
            if schema_has_integer(document, child, depth + 1, path) {
                found = true;
                break;
            }
        }
    }
    // `items` (forme objet — même filtre que la normalisation).
    if !found {
        if let Some(items) = object.get("items").filter(|items| items.is_object()) {
            found = schema_has_integer(document, items, depth + 1, path);
        }
    }
    // Combinateurs : tableaux de schémas (mêmes branches que la normalisation).
    if !found {
        for key in ["allOf", "anyOf", "oneOf"] {
            if let Some(branches) = object.get(key).and_then(Value::as_array) {
                for branch in branches {
                    if schema_has_integer(document, branch, depth + 1, path) {
                        found = true;
                        break;
                    }
                }
                if found {
                    break;
                }
            }
        }
    }

    path.pop();
    found
}

/// Le nœud déclare-t-il `"type": "integer"` (chaîne ou membre d'un tableau) ?
fn node_declares_integer(object: &serde_json::Map<String, Value>) -> bool {
    match object.get("type") {
        Some(Value::String(kind)) => kind == "integer",
        Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind.as_str() == Some("integer")),
        _ => false,
    }
}

/// Le nœud publie-t-il une position **non signée** (`format: "uint8"` …
/// `"uint64"`, ou `"uint"` pour `usize`) ? Dérivé de la **même table** que
/// les bornes publiées ([`integer_format_bounds`]) : les deux ne peuvent pas
/// diverger (une position non signée est celle dont la borne basse est 0).
fn node_declares_unsigned(object: &serde_json::Map<String, Value>) -> bool {
    object
        .get("format")
        .and_then(Value::as_str)
        .and_then(integer_format_bounds)
        .is_some_and(|(min, _)| min == 0)
}

/// Normalise les entiers intégraux aux positions entières déclarées par
/// `node` — **même parcours que [`apply_node_defaults`]** : `properties`
/// (valeurs présentes seulement), `items` (chaque élément), combinateurs ;
/// `$ref` locaux résolus, cycles détectés, profondeur bornée. Un nœud qui
/// déclare `"type": "integer"` convertit la valeur (validée au préalable :
/// tout intégral présent est donc dans la plage exacte) et ne descend pas
/// davantage.
fn normalize_node_integers(
    document: &Value,
    node: &Value,
    value: &mut Value,
    depth: usize,
    path: &mut Vec<usize>,
) {
    if depth > MAX_REF_DEPTH {
        return;
    }
    let node = resolve_local_ref(document, node);
    if path.contains(&node_address(node)) {
        return; // cycle du schéma : ce sous-document est déjà en cours
    }
    let Some(object) = node.as_object() else {
        return; // schéma booléen / scalaire : rien à visiter
    };
    // Type entier déclaré : conversion en entier exact, sans descente.
    if node_declares_integer(object) {
        if let Some(number) = value.as_number() {
            // Position **non signée** (`format: "uint8"` … `"uint64"`,
            // `"uint"` pour `usize`) : conversion u64 exacte — c'est ce que
            // fait `zod::uint()`. `integral_i64` rendrait `None` pour `1e19`
            // (au-delà de i64::MAX) et le handler recevrait un f64. Hors
            // plage, la validation a déjà refusé (bornes publiées), donc la
            // conversion ne peut pas échouer ici.
            if node_declares_unsigned(object) {
                if number.as_u64().is_none() {
                    if let Some(n) = crate::zod::integral_u64(number) {
                        *value = Value::Number(serde_json::Number::from(n));
                    }
                }
            } else if number.as_i64().is_none() {
                if let Some(n) = crate::zod::integral_i64(number) {
                    *value = Value::Number(serde_json::Number::from(n));
                }
            }
        }
        return;
    }
    path.push(node_address(node));

    // Objet : chaque champ présent est descendu (les champs absents ne
    // portent pas de valeur à normaliser).
    if let Some(map) = object.get("properties").and_then(Value::as_object) {
        if let Some(instance) = value.as_object_mut() {
            for (name, raw_child) in map {
                let child = resolve_local_ref(document, raw_child);
                if let Some(present) = instance.get_mut(name) {
                    normalize_node_integers(document, child, present, depth + 1, path);
                }
            }
        }
    }

    // Tableau : le schéma `items` s'applique à chaque élément.
    if let Some(array) = value.as_array_mut() {
        if let Some(items) = object.get("items").filter(|items| items.is_object()) {
            for element in array {
                normalize_node_integers(document, items, element, depth + 1, path);
            }
        }
    }

    // Combinateurs : mêmes branches visitées que la détection.
    for key in ["allOf", "anyOf", "oneOf"] {
        if let Some(branches) = object.get(key).and_then(Value::as_array) {
            for branch in branches {
                normalize_node_integers(document, branch, value, depth + 1, path);
            }
        }
    }

    path.pop();
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
