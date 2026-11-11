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
//!     .bind("127.0.0.1:23080")?
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
///     .bind("127.0.0.1:23080")?
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

    // **Le défaut publié est APPLIÉ** — exactement la même politique que
    // [`FromSchemars`] : les deux adaptateurs délèguent au même moteur
    // ([`schema_has_default`] pour la détection, [`apply_node_defaults`]
    // pour l'injection). Un `default` publié dans un document écrit à la
    // main est annoncé dans `tools/list` **et** injecté dans les arguments
    // absents, récursivement (objets imbriqués, tableaux, branches
    // `allOf`/`anyOf`/`oneOf`), avec les `$ref` locaux chaînés résolus
    // (détection de cycles + borne [`MAX_REF_DEPTH`]). Avant cette
    // mutualisation, le défaut brut était publié mais jamais appliqué —
    // la promesse « publié = appliqué » était rompue sur cette voie.
    fn default_value(&self) -> Option<&Value> {
        effective_root(&self.document).get("default")
    }

    fn apply_defaults(&self, value: &mut Value) {
        apply_node_defaults(&self.document, &self.document, value, 0);
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
    // `properties.<nom>.default` du document ; détection et injection
    // délèguent au **même moteur** que [`FromJsonSchema`] (même parcours,
    // même résolution des `$ref` locaux chaînés, même borne) — la détection
    // ne peut donc jamais désactiver une injection que l'injection aurait
    // faite.
    fn default_value(&self) -> Option<&Value> {
        effective_root(self.document()).get("default")
    }

    fn apply_defaults(&self, value: &mut Value) {
        apply_node_defaults(self.document(), self.document(), value, 0);
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
/// - `allOf`/`anyOf`/`oneOf` : les branches partagent la même valeur — un
///   défaut publié dans une branche est un défaut publié du schéma.
///
/// Une chaîne de `$ref` **coupée** (cycle ou budget de sauts épuisé) est
/// reprise avec un budget frais **puis** les mots-clés **frères** du nœud
/// courant sont explorés — exactement comme la détection : un nœud encore
/// référent peut porter des frères publiant un défaut (`{"$ref": …,
/// "properties": {…}}`), et court-circuiter ces frères rendrait l'injection
/// aveugle à un sous-schéma que la détection annonce.
///
/// **Pas de contrôle de cycle de schéma ici** (contrairement à la
/// détection) : un schéma récursif se ré-entend légitimement à chaque
/// niveau de l'**instance** — des sous-arbres d'instance différents — et
/// les sauter priverait les niveaux profonds de leurs défauts. La
/// terminaison vient de la finitude de l'instance (chaque appel descend
/// dans un sous-arbre strictement plus petit ou dans une valeur fraîchement
/// clonée, elle-même finie) et de la borne [`MAX_REF_DEPTH`].
///
/// Sont volontairement **exclus des deux parcours** : `patternProperties`,
/// `additionalProperties` et `not` (leurs défauts s'adressent à des clés
/// inconnues du schéma — rien n'y est injectable, donc rien n'y est annoncé).
/// Un `default` porté par le nœud lui-même relève de
/// [`Schema::default_value`], pas de l'injection dans les champs.
fn apply_node_defaults(document: &Value, node: &Value, value: &mut Value, depth: usize) {
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
        apply_node_defaults(document, node, value, depth + 1);
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
                        apply_node_defaults(document, raw_child, present, depth + 1);
                    }
                    None => {
                        if let Some(default) = effective_default(document, raw_child, depth + 1) {
                            let mut injected = default.clone();
                            apply_node_defaults(document, raw_child, &mut injected, depth + 1);
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
                apply_node_defaults(document, items, element, depth + 1);
            }
        }
    }

    // Combinateurs : mêmes sous-schémas vus par la détection.
    for key in ["allOf", "anyOf", "oneOf"] {
        if let Some(branches) = object.get(key).and_then(Value::as_array) {
            for branch in branches {
                apply_node_defaults(document, branch, value, depth + 1);
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
    // Combinateurs : tableaux de schémas (mêmes branches que l'injection).
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
            if number.as_i64().is_none() {
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
