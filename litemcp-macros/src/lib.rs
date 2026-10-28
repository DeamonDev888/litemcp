//! Macros de litemcp — `#[tool]`, `#[resource]` et `#[prompt]` : déclarer
//! une surface MCP à partir de fonctions Rust ordinaires.
//!
//! Implémentation **zéro dépendance externe** (uniquement `proc_macro` fourni
//! par `rustc` — ni `syn`, ni `quote`, ni `proc-macro2`).
//!
//! Le schéma d'entrée est **dérivé des types des paramètres** (module
//! `litemcp::zod`), et une fonction `register_<nom>` est générée à côté de
//! chaque fonction annotée, à enchaîner via `.with_tool(…)`.
//!
//! ```ignore
//! use litemcp::macros::{prompt, resource, tool};
//!
//! #[tool(name = "add", description = "Additionne deux entiers")]
//! fn add(a: i64, b: i64) -> litemcp::ToolResult {
//!     Ok(serde_json::json!({ "somme": a + b }))
//! }
//!
//! #[resource(uri = "mem://doc", name = "Doc", description = "La doc")]
//! fn doc() -> Result<litemcp::protocol::ResourceContents, String> {
//!     Ok(litemcp::protocol::ResourceContents {
//!         uri: "mem://doc".into(), mime_type: None, text: "Bonjour".into(),
//!     })
//! }
//!
//! #[prompt(description = "Demande une revue de code")]
//! fn revue(fichier: String, severite: Option<String>)
//!     -> Result<Vec<litemcp::protocol::PromptMessage>, String>
//! {
//!     Ok(vec![litemcp::protocol::PromptMessage::user(
//!         format!("Relis {fichier}"),
//!     )])
//! }
//!
//! // génère register_add, register_doc, register_revue
//! ServerBuilder::new("app", "1.0.0")
//!     .with_tool(register_add)
//!     .with_tool(register_doc)
//!     .with_tool(register_revue)
//!     .bind("127.0.0.1:23080")?
//!     .serve()?;
//! ```
//!
//! Types de paramètres supportés : entiers (`i8`…`i64`, `u8`…`u64`,
//! `isize`, `usize`), flottants (`f32`, `f64`), `bool`, `String`,
//! `Option<T>` (champ optionnel) et `Vec<T>`. La validation du schéma
//! garantit que l'extraction dans le handler ne peut pas échouer : le
//! schéma publié est exécuteur, ici aussi.

#![forbid(unsafe_code)]

use proc_macro::{Delimiter, TokenStream, TokenTree};
use std::str::FromStr;

/// Déclare un **outil** : schéma dérivé des paramètres, enregistré via
/// `register_<fonction>` → `ServerBuilder::tool_typed`.
///
/// Attributs : `name = "…"` (défaut : nom de la fonction),
/// `description = "…"` (**requis**) et `default(param = littéral, …)`.
#[proc_macro_attribute]
pub fn tool(attr: TokenStream, item: TokenStream) -> TokenStream {
    generate_parametrized(attr, item, Target::Tool)
}

/// Déclare un **prompt** : schéma dérivé des paramètres (validé avant
/// l'exécution du handler), enregistré via `register_<fonction>` →
/// `ServerBuilder::prompt_typed`.
///
/// Attributs : `name = "…"` (défaut : nom de la fonction),
/// `description = "…"` (**requis**) et `default(param = littéral, …)`.
#[proc_macro_attribute]
pub fn prompt(attr: TokenStream, item: TokenStream) -> TokenStream {
    generate_parametrized(attr, item, Target::Prompt)
}

/// Déclare une **ressource** : fonction **sans paramètre** rendant le
/// contenu à chaque lecture, enregistrée via `register_<fonction>` →
/// `ServerBuilder::resource`.
///
/// Attributs : `uri = "…"` (**requis**), `name = "…"` (défaut : nom de la
/// fonction) et `description = "…"` (**requis**).
#[proc_macro_attribute]
pub fn resource(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attrs = match parse_attrs(attr) {
        Ok(attrs) => attrs,
        Err(message) => return err(&message),
    };
    let sig = match parse_fn_sig(&item, "#[resource]") {
        Ok(sig) => sig,
        Err(message) => return err(&message),
    };
    if attrs.uri.is_none() || attrs.description.is_none() {
        return err("#[resource] exige uri = \"…\" et description = \"…\"");
    }
    if !sig.raw_params.is_empty() {
        return err("#[resource] : la fonction ne prend pas de paramètres — \
             le handler produit le contenu à chaque lecture");
    }
    let uri = attrs.uri.expect("vérifié ci-dessus");
    let description = attrs.description.expect("vérifié ci-dessus");
    let fn_name = &sig.name;
    let fn_ident = strip_raw_ident(fn_name);
    let name = attrs.name.unwrap_or_else(|| fn_ident.to_string());
    let register = format!("register_{fn_ident}");
    let doc = format!("Enregistre la ressource `{uri}` (généré par #[resource]).");

    let registration = format!(
        r#"
        #[doc = {doc:?}]
        #[allow(non_snake_case)]
        pub fn {register}(
            __litemcp_server: ::litemcp::ServerBuilder,
        ) -> ::litemcp::ServerBuilder {{
            __litemcp_server.resource({uri:?}, {name:?}, {description:?}, {fn_name})
        }}
        "#
    );

    let mut out = item;
    out.extend(TokenStream::from_str(&registration).expect("code généré valide"));
    out
}

/// Cible paramétrée de `#[tool]` / `#[prompt]`.
enum Target {
    Tool,
    Prompt,
}

/// Chemin commun de `#[tool]` et `#[prompt]` : parse attributs et
/// paramètres, dérive le schéma et l'extraction, génère `register_<fn>`.
fn generate_parametrized(attr: TokenStream, item: TokenStream, target: Target) -> TokenStream {
    let macro_name = match target {
        Target::Tool => "#[tool]",
        Target::Prompt => "#[prompt]",
    };
    let attrs = match parse_attrs(attr) {
        Ok(attrs) => attrs,
        Err(message) => return err(&message),
    };
    let sig = match parse_fn_sig(&item, macro_name) {
        Ok(sig) => sig,
        Err(message) => return err(&message),
    };
    let Some(description) = attrs.description else {
        return err(&format!("{macro_name} exige description = \"…\""));
    };
    let params = match collect_params(&sig.raw_params, macro_name, &attrs.defaults) {
        Ok(params) => params,
        Err(message) => return err(&message),
    };
    // Un `default(nom = …)` doit désigner un paramètre **existant**.
    for (name, _) in &attrs.defaults {
        if !params.iter().any(|param| &param.ident == name) {
            return err(&format!(
                "{macro_name} : `default({name} = …)` ne correspond à aucun paramètre"
            ));
        }
    }
    let fn_name = &sig.name;
    let fn_ident = strip_raw_ident(fn_name);
    let name = attrs.name.unwrap_or_else(|| fn_ident.to_string());
    let register = format!("register_{fn_ident}");

    let mut schema_chain = String::from("::litemcp::zod::object()");
    for param in &params {
        let key = strip_raw_ident(&param.ident);
        let inner_schema = schema_for(&param.ty).expect("type déjà vérifié");
        match &param.default {
            Some(default) => {
                let literal = default.to_json_value_expr();
                schema_chain.push_str(&format!(".opt({key:?}, {inner_schema}.default({literal}))"));
            }
            None if param.optional => {
                schema_chain.push_str(&format!(".opt({key:?}, {inner_schema})"));
            }
            None => {
                schema_chain.push_str(&format!(".field({key:?}, {inner_schema})"));
            }
        }
    }

    let extracted_args: Vec<String> = params
        .iter()
        .map(|param| {
            let key = strip_raw_ident(&param.ident);
            let value_expr = format!("__litemcp_args[{key:?}]");
            extract_from(
                &param.ty,
                param.optional,
                &value_expr,
                param.default.as_ref(),
            )
        })
        .collect();
    let args_joined = extracted_args.join(", ");

    let (doc, builder_method) = match target {
        Target::Tool => (
            format!("Enregistre l'outil `{name}` (généré par #[tool])."),
            "tool_typed",
        ),
        Target::Prompt => (
            format!("Enregistre le prompt `{name}` (généré par #[prompt])."),
            "prompt_typed",
        ),
    };

    let registration = format!(
        r#"
        #[doc = {doc:?}]
        #[allow(non_snake_case)]
        pub fn {register}(
            __litemcp_server: ::litemcp::ServerBuilder,
        ) -> ::litemcp::ServerBuilder {{
            __litemcp_server.{builder_method}(
                {name:?},
                {description:?},
                {schema_chain},
                |__litemcp_args| {fn_name}({args_joined}),
            )
        }}
        "#
    );

    let mut out = item;
    out.extend(TokenStream::from_str(&registration).expect("code généré valide"));
    out
}

fn strip_raw_ident(ident: &str) -> &str {
    ident.strip_prefix("r#").unwrap_or(ident)
}

/// Attributs acceptés par les trois macros.
#[derive(Default)]
struct Attrs {
    name: Option<String>,
    description: Option<String>,
    uri: Option<String>,
    /// Valeurs par défaut des paramètres : `default(limit = 10, mode = "fast")`.
    defaults: Vec<(String, Vec<TokenTree>)>,
}

fn parse_attrs(attr: TokenStream) -> Result<Attrs, String> {
    let mut out = Attrs::default();
    let tokens: Vec<TokenTree> = attr.into_iter().collect();
    if tokens.is_empty() {
        return Ok(out);
    }
    for entry in split_comma_separated(&tokens) {
        if entry.is_empty() {
            continue;
        }
        let Some(TokenTree::Ident(ident)) = entry.first() else {
            return Err(
                "attribut invalide — attendus : name, description, uri, default(nom = valeur)"
                    .to_string(),
            );
        };
        let key = ident.to_string();
        match key.as_str() {
            "name" | "description" | "uri" => {
                if entry.len() != 3 || !is_punct(&entry[1], '=') {
                    return Err(format!("syntaxe attendue : `{key} = \"…\"`"));
                }
                let Some(value) = parse_token_string_lit(&entry[2]) else {
                    return Err(format!("`{key}` attend une chaîne littérale (`\"…\"`)"));
                };
                match key.as_str() {
                    "name" => out.name = Some(value),
                    "description" => out.description = Some(value),
                    "uri" => out.uri = Some(value),
                    _ => unreachable!(),
                }
            }
            "default" => {
                if entry.len() != 2 {
                    return Err("syntaxe attendue : `default(nom = valeur, …)`".to_string());
                }
                let TokenTree::Group(group) = &entry[1] else {
                    return Err("syntaxe attendue : `default(nom = valeur, …)`".to_string());
                };
                if group.delimiter() != Delimiter::Parenthesis {
                    return Err("syntaxe attendue : `default(nom = valeur, …)`".to_string());
                }
                let inner: Vec<TokenTree> = group.stream().into_iter().collect();
                for pair in split_comma_separated(&inner) {
                    if pair.is_empty() {
                        continue;
                    }
                    let Some(TokenTree::Ident(param_ident)) = pair.first() else {
                        return Err(
                            "syntaxe attendue dans `default(…)` : `nom = valeur`".to_string()
                        );
                    };
                    if pair.len() < 3 || !is_punct(&pair[1], '=') {
                        return Err(format!(
                            "syntaxe attendue dans `default(…)` : `{param_ident} = valeur`"
                        ));
                    }
                    let param_name = strip_raw_ident(&param_ident.to_string()).to_string();
                    if out
                        .defaults
                        .iter()
                        .any(|(existing, _)| existing == &param_name)
                    {
                        return Err(format!("attribut `default` en double pour `{param_name}`"));
                    }
                    out.defaults.push((param_name, pair[2..].to_vec()));
                }
            }
            _ => {
                return Err(
                    "attribut inconnu — attendus : name, description, uri, default(nom = valeur)"
                        .to_string(),
                );
            }
        }
    }
    Ok(out)
}

/// Signature extraite de la fonction annotée.
struct FnSig {
    name: String,
    raw_params: Vec<Vec<TokenTree>>,
}

fn parse_fn_sig(item: &TokenStream, macro_name: &str) -> Result<FnSig, String> {
    let tokens: Vec<TokenTree> = item.clone().into_iter().collect();
    let mut i = 0;
    while i < tokens.len() {
        if let TokenTree::Ident(ident) = &tokens[i] {
            if ident.to_string() == "fn" {
                let Some(TokenTree::Ident(fn_ident)) = tokens.get(i + 1) else {
                    return Err(format!("{macro_name} : nom de fonction attendu après `fn`"));
                };
                let name = fn_ident.to_string();
                let mut j = i + 2;
                while j < tokens.len() {
                    // Génériques (`fn foo<T: Fn(u8)>(…)`) : un `<` avant la
                    // liste de paramètres — non supportés. Erreur explicite
                    // plutôt qu'un mauvais découpage : sans ce garde, la
                    // parenthèse d'une borne `Fn(u8)` serait prise pour la
                    // liste de paramètres et l'erreur de compilation
                    // résultante n'aurait aucun rapport avec la cause.
                    if is_punct(&tokens[j], '<') {
                        return Err(format!(
                            "{macro_name} : les fonctions génériques ne sont pas supportées — \
                             les types de paramètres doivent être concrets"
                        ));
                    }
                    if let TokenTree::Group(group) = &tokens[j] {
                        if group.delimiter() == Delimiter::Parenthesis {
                            let param_tokens: Vec<TokenTree> = group.stream().into_iter().collect();
                            let raw_params = split_comma_separated(&param_tokens)
                                .into_iter()
                                .filter(|slice| !slice.is_empty())
                                .map(<[TokenTree]>::to_vec)
                                .collect();
                            return Ok(FnSig { name, raw_params });
                        }
                    }
                    j += 1;
                }
                return Err(format!(
                    "{macro_name} : liste de paramètres attendue pour `{name}`"
                ));
            }
        }
        i += 1;
    }
    Err(format!(
        "{macro_name} doit être appliqué à une fonction `fn`"
    ))
}

/// Type de paramètre supporté par `#[tool]` et `#[prompt]`.
#[derive(Clone)]
enum ParamType {
    Primitive(&'static str),
    Vec(Box<ParamType>),
}

impl ParamType {
    fn rust_type_str(&self) -> String {
        match self {
            Self::Primitive(name) => (*name).to_string(),
            Self::Vec(inner) => format!("::std::vec::Vec<{}>", inner.rust_type_str()),
        }
    }
}

/// Littéral JSON validé pour `default(param = …)`.
#[derive(Clone)]
enum DefaultLit {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Array(Vec<DefaultLit>),
}

impl DefaultLit {
    /// Expression Rust produisant un `serde_json::Value` possédé.
    fn to_json_value_expr(&self) -> String {
        match self {
            Self::Int(n) => format!("::serde_json::Value::from({n}i64)"),
            Self::Float(n) => format!("::serde_json::Value::from({n:?}f64)"),
            Self::Str(s) => {
                format!("::serde_json::Value::from(::std::string::String::from({s:?}))")
            }
            Self::Bool(b) => format!("::serde_json::Value::from({b})"),
            Self::Array(items) => {
                let rendered: Vec<String> =
                    items.iter().map(DefaultLit::to_json_value_expr).collect();
                format!(
                    "::serde_json::Value::Array(::std::vec![{}])",
                    rendered.join(", ")
                )
            }
        }
    }

    /// Vérifie à la compilation que le littéral par défaut respecte le type et
    /// les bornes du paramètre Rust cible.
    fn is_compatible_with(&self, ty: &ParamType) -> bool {
        match ty {
            ParamType::Primitive("i8") => {
                matches!(self, Self::Int(n) if *n >= i8::MIN as i64 && *n <= i8::MAX as i64)
            }
            ParamType::Primitive("i16") => {
                matches!(self, Self::Int(n) if *n >= i16::MIN as i64 && *n <= i16::MAX as i64)
            }
            ParamType::Primitive("i32") => {
                matches!(self, Self::Int(n) if *n >= i32::MIN as i64 && *n <= i32::MAX as i64)
            }
            ParamType::Primitive("i64" | "isize") => matches!(self, Self::Int(_)),
            ParamType::Primitive("u8") => {
                matches!(self, Self::Int(n) if *n >= 0 && *n <= u8::MAX as i64)
            }
            ParamType::Primitive("u16") => {
                matches!(self, Self::Int(n) if *n >= 0 && *n <= u16::MAX as i64)
            }
            ParamType::Primitive("u32") => {
                matches!(self, Self::Int(n) if *n >= 0 && *n <= u32::MAX as i64)
            }
            ParamType::Primitive("u64" | "usize") => matches!(self, Self::Int(n) if *n >= 0),
            ParamType::Primitive("f32") => match self {
                Self::Float(f) => *f >= f32::MIN as f64 && *f <= f32::MAX as f64,
                Self::Int(_) => true,
                _ => false,
            },
            ParamType::Primitive("f64") => matches!(self, Self::Float(_) | Self::Int(_)),
            ParamType::Primitive("bool") => matches!(self, Self::Bool(_)),
            ParamType::Primitive("String") => matches!(self, Self::Str(_)),
            ParamType::Vec(inner) => match self {
                Self::Array(items) => items.iter().all(|item| item.is_compatible_with(inner)),
                _ => false,
            },
            ParamType::Primitive(_) => false,
        }
    }
}

/// Un paramètre de fonction annotée = un champ de schéma.
struct Param {
    ident: String,
    ty: ParamType,
    optional: bool,
    default: Option<DefaultLit>,
}

fn collect_params(
    raw_params: &[Vec<TokenTree>],
    macro_name: &str,
    defaults: &[(String, Vec<TokenTree>)],
) -> Result<Vec<Param>, String> {
    let mut params = Vec::new();
    for raw in raw_params {
        if raw
            .iter()
            .any(|t| matches!(t, TokenTree::Ident(id) if id.to_string() == "self"))
            || raw.iter().all(|t| is_punct(t, '.'))
        {
            return Err(format!(
                "{macro_name} : `self` et les variadiques ne sont pas supportés"
            ));
        }
        let colon_pos = raw.iter().position(|t| is_punct(t, ':')).ok_or_else(|| {
            format!("{macro_name} : chaque paramètre doit être de la forme `nom: Type`")
        })?;
        let pat_tokens = &raw[..colon_pos];
        let ty_tokens = &raw[colon_pos + 1..];

        let param_ident = match pat_tokens {
            [TokenTree::Ident(id)] => strip_raw_ident(&id.to_string()).to_string(),
            [TokenTree::Ident(kw), TokenTree::Ident(id)] if kw.to_string() == "mut" => {
                strip_raw_ident(&id.to_string()).to_string()
            }
            _ => {
                return Err(format!(
                    "{macro_name} : chaque paramètre doit être un identifiant simple"
                ));
            }
        };

        let (inner_tokens, optional) = strip_option_tokens(ty_tokens);
        let Some(ty) = parse_param_type(&inner_tokens) else {
            return Err(format!(
                "{macro_name} : type non supporté pour le paramètre `{param_ident}` — \
                 entiers, f32/f64, bool, String, Option<T>, Vec<T>"
            ));
        };

        let default_tokens = defaults
            .iter()
            .find(|(name, _)| name == &param_ident)
            .map(|(_, expr)| expr.as_slice());

        let default = match default_tokens {
            None => None,
            Some(_) if optional => {
                return Err(format!(
                    "{macro_name} : le paramètre `{param_ident}` ne peut pas avoir à la fois \
                     `Option<T>` et une valeur par défaut"
                ));
            }
            Some(tokens) => match parse_default_lit(tokens) {
                Some(lit) if lit.is_compatible_with(&ty) => Some(lit),
                Some(_) => {
                    return Err(format!(
                        "{macro_name} : la valeur par défaut de `{param_ident}` est incompatible \
                         avec le type ou les bornes de `{}`",
                        ty.rust_type_str()
                    ));
                }
                None => {
                    return Err(format!(
                        "{macro_name} : la valeur par défaut de `{param_ident}` doit être un \
                         littéral (publiée dans le JSON Schema)"
                    ));
                }
            },
        };

        params.push(Param {
            ident: param_ident,
            ty,
            optional,
            default,
        });
    }
    Ok(params)
}

fn flatten_none_groups(tokens: &[TokenTree]) -> Vec<TokenTree> {
    let mut out = Vec::with_capacity(tokens.len());
    for token in tokens {
        match token {
            TokenTree::Group(group) if group.delimiter() == Delimiter::None => {
                let inner: Vec<TokenTree> = group.stream().into_iter().collect();
                out.extend(flatten_none_groups(&inner));
            }
            other => out.push(other.clone()),
        }
    }
    out
}

/// Extrait `( nom_dernier_segment, Some(tokens_entre_chevrons) )` pour un chemin
/// de type Rust (`Option<T>`, `std::vec::Vec<T>`, `i64`, etc.).
fn split_type_path(tokens: &[TokenTree]) -> Option<(String, Option<Vec<TokenTree>>)> {
    let flat = flatten_none_groups(tokens);
    if flat.is_empty() {
        return None;
    }
    // Recherche d'un éventuel `< ... >` final.
    let angle_start = flat.iter().position(|t| is_punct(t, '<'));
    let (path_part, generic_part) = match angle_start {
        None => (&flat[..], None),
        Some(pos) => {
            if !is_punct(flat.last()?, '>') || pos == 0 {
                return None;
            }
            (&flat[..pos], Some(flat[pos + 1..flat.len() - 1].to_vec()))
        }
    };
    // Le chemin doit se terminer par un `Ident` et ne contenir que des `Ident`
    // et des `::`.
    for token in path_part {
        match token {
            TokenTree::Ident(_) => {}
            TokenTree::Punct(p) if p.as_char() == ':' => {}
            _ => return None,
        }
    }
    let TokenTree::Ident(last_ident) = path_part.last()? else {
        return None;
    };
    Some((
        strip_raw_ident(&last_ident.to_string()).to_string(),
        generic_part,
    ))
}

/// Si `tokens` représente `Option<T>`, renvoie `(T, true)` ; sinon `(tokens, false)`.
fn strip_option_tokens(tokens: &[TokenTree]) -> (Vec<TokenTree>, bool) {
    if let Some((ident, Some(inner))) = split_type_path(tokens) {
        if ident == "Option" && !inner.is_empty() {
            return (inner, true);
        }
    }
    (tokens.to_vec(), false)
}

fn parse_param_type(tokens: &[TokenTree]) -> Option<ParamType> {
    let (ident, generic) = split_type_path(tokens)?;
    match (ident.as_str(), generic.as_deref()) {
        ("i8", None) => Some(ParamType::Primitive("i8")),
        ("i16", None) => Some(ParamType::Primitive("i16")),
        ("i32", None) => Some(ParamType::Primitive("i32")),
        ("i64", None) => Some(ParamType::Primitive("i64")),
        ("isize", None) => Some(ParamType::Primitive("isize")),
        ("u8", None) => Some(ParamType::Primitive("u8")),
        ("u16", None) => Some(ParamType::Primitive("u16")),
        ("u32", None) => Some(ParamType::Primitive("u32")),
        ("u64", None) => Some(ParamType::Primitive("u64")),
        ("usize", None) => Some(ParamType::Primitive("usize")),
        ("f32", None) => Some(ParamType::Primitive("f32")),
        ("f64", None) => Some(ParamType::Primitive("f64")),
        ("bool", None) => Some(ParamType::Primitive("bool")),
        ("String", None) => Some(ParamType::Primitive("String")),
        ("Vec", Some(inner)) => Some(ParamType::Vec(Box::new(parse_param_type(inner)?))),
        _ => None,
    }
}

/// Expression `zod` pour un type supporté — chaque type entier publie et
/// applique ses **bornes exactes**.
fn schema_for(ty: &ParamType) -> Option<String> {
    match ty {
        ParamType::Primitive("i8") => Some(
            "::litemcp::zod::integer()\
                .min(::core::primitive::i8::MIN as i64)\
                .max(::core::primitive::i8::MAX as i64)"
                .to_string(),
        ),
        ParamType::Primitive("i16") => Some(
            "::litemcp::zod::integer()\
                .min(::core::primitive::i16::MIN as i64)\
                .max(::core::primitive::i16::MAX as i64)"
                .to_string(),
        ),
        ParamType::Primitive("i32") => Some(
            "::litemcp::zod::integer()\
                .min(::core::primitive::i32::MIN as i64)\
                .max(::core::primitive::i32::MAX as i64)"
                .to_string(),
        ),
        ParamType::Primitive("i64") => Some("::litemcp::zod::integer()".to_string()),
        ParamType::Primitive("isize") => Some(
            "::litemcp::zod::integer()\
                .min((::core::primitive::isize::MIN as i128).max(::core::primitive::i64::MIN as i128) as i64)\
                .max((::core::primitive::isize::MAX as i128).min(::core::primitive::i64::MAX as i128) as i64)"
                .to_string(),
        ),
        ParamType::Primitive("u8") => Some(
            "::litemcp::zod::integer()\
                .min(0)\
                .max(::core::primitive::u8::MAX as i64)"
                .to_string(),
        ),
        ParamType::Primitive("u16") => Some(
            "::litemcp::zod::integer()\
                .min(0)\
                .max(::core::primitive::u16::MAX as i64)"
                .to_string(),
        ),
        ParamType::Primitive("u32") => Some(
            "::litemcp::zod::integer()\
                .min(0)\
                .max(::core::primitive::u32::MAX as i64)"
                .to_string(),
        ),
        ParamType::Primitive("u64") => Some("::litemcp::zod::integer().min(0)".to_string()),
        ParamType::Primitive("usize") => Some(
            "::litemcp::zod::integer()\
                .min(0)\
                .max((::core::primitive::usize::MAX as u128).min(::core::primitive::i64::MAX as u128) as i64)"
                .to_string(),
        ),
        ParamType::Primitive("f32") => Some(
            "::litemcp::zod::number()\
                .min(::core::primitive::f32::MIN as f64)\
                .max(::core::primitive::f32::MAX as f64)"
                .to_string(),
        ),
        ParamType::Primitive("f64") => Some("::litemcp::zod::number()".to_string()),
        ParamType::Primitive("bool") => Some("::litemcp::zod::boolean()".to_string()),
        ParamType::Primitive("String") => Some("::litemcp::zod::string()".to_string()),
        ParamType::Vec(inner) => {
            let inner_schema = schema_for(inner)?;
            Some(format!("::litemcp::zod::array({inner_schema})"))
        }
        ParamType::Primitive(_) => None,
    }
}

fn extract_from(
    ty: &ParamType,
    optional: bool,
    value_expr: &str,
    default: Option<&DefaultLit>,
) -> String {
    let extraction = extract_required(ty, value_expr, default);
    if optional {
        format!(
            "if {value_expr}.is_null() {{ ::core::option::Option::None }} \
             else {{ ::core::option::Option::Some({extraction}) }}"
        )
    } else {
        extraction
    }
}

fn extract_required(ty: &ParamType, value_expr: &str, default: Option<&DefaultLit>) -> String {
    match ty {
        ParamType::Primitive(
            int_ty @ ("i8" | "i16" | "i32" | "i64" | "isize" | "u8" | "u16" | "u32" | "u64"
            | "usize"),
        ) => match default {
            Some(lit) => {
                let fb = lit.to_json_value_expr();
                format!(
                    "{value_expr}.as_i64().or_else(|| {fb}.as_i64()).unwrap_or_default() as {int_ty}"
                )
            }
            None => format!("{value_expr}.as_i64().unwrap_or_default() as {int_ty}"),
        },
        ParamType::Primitive(float_ty @ ("f32" | "f64")) => match default {
            Some(lit) => {
                let fb = lit.to_json_value_expr();
                format!(
                    "{value_expr}.as_f64().or_else(|| {fb}.as_f64()).unwrap_or_default() as {float_ty}"
                )
            }
            None => format!("{value_expr}.as_f64().unwrap_or_default() as {float_ty}"),
        },
        ParamType::Primitive("bool") => match default {
            Some(lit) => {
                let fb = lit.to_json_value_expr();
                format!("{value_expr}.as_bool().or_else(|| {fb}.as_bool()).unwrap_or_default()")
            }
            None => format!("{value_expr}.as_bool().unwrap_or_default()"),
        },
        ParamType::Primitive("String") => match default {
            Some(DefaultLit::Str(s)) => format!(
                "match {value_expr}.as_str() {{ \
                    Some(__litemcp_s) => ::std::string::String::from(__litemcp_s), \
                    None => ::std::string::String::from({s:?}), \
                }}"
            ),
            _ => format!("{value_expr}.as_str().unwrap_or(\"\").to_owned()"),
        },
        ParamType::Vec(inner) => {
            let item = extract_required(inner, "__litemcp_item", None);
            let rust_ty = ty.rust_type_str();
            match default {
                Some(lit) => {
                    let fb = lit.to_json_value_expr();
                    format!(
                        "{value_expr}.as_array()\
                            .or_else(|| {fb}.as_array())\
                            .map(|__litemcp_list| __litemcp_list.iter()\
                                .map(|__litemcp_item| {item})\
                                .collect::<{rust_ty}>())\
                            .unwrap_or_default()"
                    )
                }
                None => format!(
                    "{value_expr}.as_array()\
                        .map(|__litemcp_list| __litemcp_list.iter()\
                            .map(|__litemcp_item| {item})\
                            .collect::<{rust_ty}>())\
                        .unwrap_or_default()"
                ),
            }
        }
        ParamType::Primitive(_) => "::core::default::Default::default()".to_string(),
    }
}

/// Analyse un littéral JSON (`i64`, `f64` fini, `bool`, `"…"`, `-nombre`, ou
/// `[…]`) depuis une tranche de `TokenTree`.
fn parse_default_lit(tokens: &[TokenTree]) -> Option<DefaultLit> {
    match tokens {
        [TokenTree::Group(group)]
            if matches!(group.delimiter(), Delimiter::Parenthesis | Delimiter::None) =>
        {
            let inner: Vec<TokenTree> = group.stream().into_iter().collect();
            parse_default_lit(&inner)
        }
        [TokenTree::Group(group)] if group.delimiter() == Delimiter::Bracket => {
            let inner: Vec<TokenTree> = group.stream().into_iter().collect();
            let mut items = Vec::new();
            for slice in split_comma_separated(&inner) {
                if slice.is_empty() {
                    continue;
                }
                items.push(parse_default_lit(slice)?);
            }
            Some(DefaultLit::Array(items))
        }
        [TokenTree::Ident(ident)] => match ident.to_string().as_str() {
            "true" => Some(DefaultLit::Bool(true)),
            "false" => Some(DefaultLit::Bool(false)),
            _ => None,
        },
        [TokenTree::Literal(_) | TokenTree::Group(_)] => {
            if let Some(s) = parse_token_string_lit(&tokens[0]) {
                return Some(DefaultLit::Str(s));
            }
            parse_numeric_lit(&tokens[0].to_string(), false)
        }
        [first, rest @ ..] if is_punct(first, '-') => {
            let inner = match rest {
                [TokenTree::Group(group)]
                    if matches!(group.delimiter(), Delimiter::Parenthesis | Delimiter::None) =>
                {
                    let v: Vec<TokenTree> = group.stream().into_iter().collect();
                    if v.len() != 1 {
                        return None;
                    }
                    v[0].to_string()
                }
                [single] => single.to_string(),
                _ => return None,
            };
            parse_numeric_lit(&inner, true)
        }
        _ => None,
    }
}

fn parse_numeric_lit(raw: &str, negative: bool) -> Option<DefaultLit> {
    let clean: String = raw.chars().filter(|&c| c != '_').collect();
    // Entier décimal avec suffixe optionnel (`10`, `10u64`, `10i32`…).
    for suffix in [
        "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize", "",
    ] {
        if let Some(digits) = clean.strip_suffix(suffix) {
            if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                let signed = if negative {
                    format!("-{digits}")
                } else {
                    digits.to_string()
                };
                if let Ok(n) = signed.parse::<i64>() {
                    return Some(DefaultLit::Int(n));
                }
            }
        }
    }
    // Flottant décimal avec suffixe optionnel (`2.5`, `2.5f64`, `1e5`…).
    for suffix in ["f32", "f64", ""] {
        if let Some(num) = clean.strip_suffix(suffix) {
            if num.contains('.') || num.contains('e') || num.contains('E') {
                let signed = if negative {
                    format!("-{num}")
                } else {
                    num.to_string()
                };
                if let Ok(f) = signed.parse::<f64>() {
                    if f.is_finite() {
                        return Some(DefaultLit::Float(f));
                    }
                }
            }
        }
    }
    None
}

/// Découpe une tranche de `TokenTree` sur les virgules de premier niveau.
fn split_comma_separated(tokens: &[TokenTree]) -> Vec<&[TokenTree]> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut angle_depth = 0usize;
    for (idx, token) in tokens.iter().enumerate() {
        if is_punct(token, '<') {
            angle_depth += 1;
        } else if is_punct(token, '>') {
            angle_depth = angle_depth.saturating_sub(1);
        } else if angle_depth == 0 && is_punct(token, ',') {
            parts.push(&tokens[start..idx]);
            start = idx + 1;
        }
    }
    parts.push(&tokens[start..]);
    parts
}

fn is_punct(token: &TokenTree, ch: char) -> bool {
    matches!(token, TokenTree::Punct(p) if p.as_char() == ch)
}

/// Décode un littéral de chaîne Rust (`"…"` ou `r#"…"#`) depuis un `TokenTree`.
fn parse_token_string_lit(token: &TokenTree) -> Option<String> {
    match token {
        TokenTree::Group(group) if group.delimiter() == Delimiter::None => {
            let inner: Vec<TokenTree> = group.stream().into_iter().collect();
            if inner.len() == 1 {
                parse_token_string_lit(&inner[0])
            } else {
                None
            }
        }
        TokenTree::Literal(lit) => parse_str_literal(&lit.to_string()),
        _ => None,
    }
}

fn parse_str_literal(raw: &str) -> Option<String> {
    if let Some(rest) = raw.strip_prefix('r') {
        let hashes = rest.bytes().take_while(|&b| b == b'#').count();
        let after_hashes = &rest[hashes..];
        let content = after_hashes
            .strip_prefix('"')?
            .strip_suffix(&"#".repeat(hashes))?
            .strip_suffix('"')?;
        return Some(content.to_string());
    }
    let body = raw.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next()? {
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            '\\' => out.push('\\'),
            '0' => out.push('\0'),
            '\'' => out.push('\''),
            '"' => out.push('"'),
            'x' => {
                let hi = chars.next()?.to_digit(16)?;
                let lo = chars.next()?.to_digit(16)?;
                out.push(char::from_u32((hi << 4) | lo)?);
            }
            'u' => {
                if chars.next()? != '{' {
                    return None;
                }
                let mut code = 0u32;
                let mut digits = 0;
                for c in chars.by_ref() {
                    if c == '}' {
                        break;
                    }
                    if c == '_' {
                        continue;
                    }
                    code = (code << 4) | c.to_digit(16)?;
                    digits += 1;
                    if digits > 6 {
                        return None;
                    }
                }
                out.push(char::from_u32(code)?);
            }
            '\n' => {
                while chars
                    .peek()
                    .is_some_and(|c| matches!(c, ' ' | '\t' | '\n' | '\r'))
                {
                    chars.next();
                }
            }
            _ => return None,
        }
    }
    Some(out)
}

fn err(message: &str) -> TokenStream {
    TokenStream::from_str(&format!("compile_error!({message:?});")).expect("compile_error valide")
}
