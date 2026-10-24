//! Macros de litemcp — `#[tool]`, `#[resource]` et `#[prompt]` : déclarer
//! une surface MCP à partir de fonctions Rust ordinaires.
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
//!     .bind("127.0.0.1:8080")?
//!     .serve()?;
//! ```
//!
//! Types de paramètres supportés : entiers (`i8`…`i64`, `u8`…`u64`,
//! `isize`, `usize`), flottants (`f32`, `f64`), `bool`, `String`,
//! `Option<T>` (champ optionnel) et `Vec<T>`. La validation du schéma
//! garantit que l'extraction dans le handler ne peut pas échouer : le
//! schéma publié est exécuteur, ici aussi.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::parse::Parse;
use syn::{ItemFn, Signature};

/// Déclare un **outil** : schéma dérivé des paramètres, enregistré via
/// `register_<fonction>` → `ServerBuilder::tool_typed`.
///
/// Attributs : `name = "…"` (défaut : nom de la fonction) et
/// `description = "…"` (**requis**).
#[proc_macro_attribute]
pub fn tool(attr: TokenStream, item: TokenStream) -> TokenStream {
    generate_parametrized(attr, item, Target::Tool)
}

/// Déclare un **prompt** : schéma dérivé des paramètres (validé avant
/// l'exécution du handler), enregistré via `register_<fonction>` →
/// `ServerBuilder::prompt_typed`.
///
/// Attributs : `name = "…"` (défaut : nom de la fonction) et
/// `description = "…"` (**requis**).
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
    let attrs = match parse_attrs(attr.into()) {
        Ok(attrs) => attrs,
        Err(error) => return error.to_compile_error().into(),
    };
    let function = match syn::parse::<ItemFn>(item) {
        Ok(function) => function,
        Err(error) => return error.to_compile_error().into(),
    };
    if attrs.uri.is_none() || attrs.description.is_none() {
        return err("#[resource] exige uri = \"…\" et description = \"…\"");
    }
    if !function.sig.inputs.is_empty() {
        return err("#[resource] : la fonction ne prend pas de paramètres — \
             le handler produit le contenu à chaque lecture");
    }
    let uri = attrs.uri.expect("vérifié ci-dessus");
    let description = attrs.description.expect("vérifié ci-dessus");
    let fn_name = function.sig.ident.clone();
    let name = attrs.name.unwrap_or_else(|| fn_name.to_string());
    let register = format_ident!("register_{}", fn_name);
    quote! {
        #function

        #[doc = concat!("Enregistre la ressource `", #uri, "` (généré par #[resource]).")]
        #[allow(non_snake_case)]
        pub fn #register(
            __litemcp_server: ::litemcp::ServerBuilder,
        ) -> ::litemcp::ServerBuilder {
            __litemcp_server.resource(#uri, #name, #description, #fn_name)
        }
    }
    .into()
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
    let attrs = match parse_attrs(attr.into()) {
        Ok(attrs) => attrs,
        Err(error) => return error.to_compile_error().into(),
    };
    let function = match syn::parse::<ItemFn>(item) {
        Ok(function) => function,
        Err(error) => return error.to_compile_error().into(),
    };
    let Some(description) = attrs.description else {
        return err(&format!("{macro_name} exige description = \"…\""));
    };
    let params = match collect_params(&function.sig, macro_name, &attrs.defaults) {
        Ok(params) => params,
        Err(error) => return error.to_compile_error().into(),
    };
    // Un `default(nom = …)` doit désigner un paramètre **existant**.
    for (name, _) in &attrs.defaults {
        if !params.iter().any(|param| &param.ident == name) {
            return err(&format!(
                "{macro_name} : `default({name} = …)` ne correspond à aucun paramètre"
            ));
        }
    }
    let fn_name = function.sig.ident.clone();
    let name = attrs.name.unwrap_or_else(|| fn_name.to_string());
    let register = format_ident!("register_{}", fn_name);

    // Chaîne `.field(…)`/`.opt(…)` sur l'objet racine, puis arguments
    // extraits du JSON validé — la validation garantit présence et type.
    //
    // Un paramètre **avec valeur par défaut** (`#[tool(default = 5)]`)
    // devient un champ optionnel portant `default` : la valeur est
    // publiée dans le JSON Schema **et** injectée dans les arguments
    // quand elle est absente (comme Zod).
    let schema = params.iter().map(|param| {
        let key = param.ident.to_string();
        let (inner, _) = strip_option(&param.ty);
        let inner_schema = schema_for(inner).expect("type déjà vérifié");
        match &param.default {
            Some(default) => {
                let literal = json_literal(default);
                quote! { .opt(#key, #inner_schema.default(#literal)) }
            }
            None if param.optional => quote! { .opt(#key, #inner_schema) },
            None => quote! { .field(#key, #inner_schema) },
        }
    });
    // Un parametre **avec valeur par defaut** : le schema le rend
    // optionnel et injecte la valeur si elle est absente ; l'extraction
    // retombe sur le defaut (au cas ou le schema n'aurait pas injecte —
    // appel direct de la fonction en Rust).
    let args = params.iter().map(|param| {
        let key = param.ident.to_string();
        let fallback = param.default.as_ref().map(|default| {
            // Le littéral est converti en **expression JSON** possédée
            // (`serde_json::Value::from(...)`) : aucune référence à un
            // temporaire ne survit à la génération.
            let literal = json_literal_owned(default);
            quote! { #literal }
        });
        // Le repli est aussi disponible sous forme de `String` (chemin
        // d'extraction des chaînes).
        let fallback_string = param.default.as_ref().and_then(|default| {
            let syn::Expr::Lit(literal) = default else {
                return None;
            };
            let syn::Lit::Str(text) = &literal.lit else {
                return None;
            };
            let value = text.value();
            Some(quote! { ::std::string::String::from(#value) })
        });
        extract_from(
            &param.ty,
            &quote! { __litemcp_args[#key] },
            fallback,
            fallback_string,
        )
    });

    let registration = match target {
        Target::Tool => quote! {
            #[doc = concat!("Enregistre l'outil `", #name, "` (généré par #[tool]).")]
            #[allow(non_snake_case)]
            pub fn #register(
                __litemcp_server: ::litemcp::ServerBuilder,
            ) -> ::litemcp::ServerBuilder {
                __litemcp_server.tool_typed(
                    #name,
                    #description,
                    ::litemcp::zod::object() #(#schema)*,
                    |__litemcp_args| #fn_name(#(#args),*),
                )
            }
        },
        Target::Prompt => quote! {
            #[doc = concat!("Enregistre le prompt `", #name, "` (généré par #[prompt]).")]
            #[allow(non_snake_case)]
            pub fn #register(
                __litemcp_server: ::litemcp::ServerBuilder,
            ) -> ::litemcp::ServerBuilder {
                __litemcp_server.prompt_typed(
                    #name,
                    #description,
                    ::litemcp::zod::object() #(#schema)*,
                    |__litemcp_args| #fn_name(#(#args),*),
                )
            }
        },
    };
    quote! {
        #function
        #registration
    }
    .into()
}

/// Attributs acceptés par les trois macros.
#[derive(Default)]
struct Attrs {
    name: Option<String>,
    description: Option<String>,
    uri: Option<String>,
    /// Valeurs par défaut des paramètres : `default(limit = 10, mode = "fast")`.
    ///
    /// Rust interdit les *attribute macros* sur les paramètres d'une
    /// fonction : les défauts sont donc déclarés **au niveau de la macro**
    /// (sur la fonction), par nom de paramètre.
    defaults: Vec<(syn::Ident, syn::Expr)>,
}

fn parse_attrs(attr: TokenStream2) -> syn::Result<Attrs> {
    let mut out = Attrs::default();
    let parser = syn::meta::parser(|meta| {
        if meta.path.is_ident("name") {
            out.name = Some(meta.value()?.parse::<syn::LitStr>()?.value());
            Ok(())
        } else if meta.path.is_ident("description") {
            out.description = Some(meta.value()?.parse::<syn::LitStr>()?.value());
            Ok(())
        } else if meta.path.is_ident("uri") {
            out.uri = Some(meta.value()?.parse::<syn::LitStr>()?.value());
            Ok(())
        } else if meta.path.is_ident("default") {
            // `default(limit = 10, mode = "fast")`
            let content;
            syn::parenthesized!(content in meta.input);
            let entries = content.parse_terminated(
                |input: syn::parse::ParseStream<'_>| NamedValue::parse(input),
                syn::Token![,],
            )?;
            for entry in entries {
                out.defaults.push((entry.ident, entry.value));
            }
            Ok(())
        } else {
            Err(meta.error(
                "attribut inconnu — attendus : name, description, uri, default(nom = valeur)",
            ))
        }
    });
    syn::parse::Parser::parse2(parser, attr)?;
    Ok(out)
}

/// Une paire `nom = valeur` parsée dans `default(…)`.
struct NamedValue {
    ident: syn::Ident,
    value: syn::Expr,
}

impl syn::parse::Parse for NamedValue {
    fn parse(input: syn::parse::ParseStream<'_>) -> syn::Result<Self> {
        let ident: syn::Ident = input.parse()?;
        input.parse::<syn::Token![=]>()?;
        let value: syn::Expr = input.parse()?;
        Ok(Self { ident, value })
    }
}

/// Un paramètre de fonction annotée = un champ de schéma.
struct Param {
    ident: syn::Ident,
    ty: syn::Type,
    optional: bool,
    /// Expression de la **valeur par défaut** du paramètre
    /// (`#[tool(default = 5)]`).
    ///
    /// Présente → le champ est publié avec `default` **et** la valeur est
    /// injectée dans les arguments si elle est absente (comme Zod).
    default: Option<syn::Expr>,
}

fn collect_params(
    sig: &Signature,
    macro_name: &str,
    defaults: &[(syn::Ident, syn::Expr)],
) -> syn::Result<Vec<Param>> {
    let mut params = Vec::new();
    for arg in &sig.inputs {
        let syn::FnArg::Typed(pat_type) = arg else {
            return Err(syn::Error::new_spanned(
                sig.fn_token,
                format!("{macro_name} : `self` et les variadiques ne sont pas supportés"),
            ));
        };
        let syn::Pat::Ident(pat_ident) = &*pat_type.pat else {
            return Err(syn::Error::new_spanned(
                &pat_type.pat,
                format!("{macro_name} : chaque paramètre doit être un identifiant simple"),
            ));
        };
        let ty = &*pat_type.ty;
        // Valeur par défaut : déclarée sur la macro
        // (`#[tool(default(limit = 10))]`) — Rust interdit les attribute
        // macros sur les paramètres.
        let default = defaults
            .iter()
            .find(|(name, _)| name == &pat_ident.ident)
            .map(|(_, expr)| expr.clone());
        let (inner, optional) = strip_option(ty);
        if schema_for(inner).is_none() {
            return Err(syn::Error::new_spanned(
                ty,
                format!(
                    "{macro_name} : type non supporté pour le paramètre `{}` — \
                     entiers, f32/f64, bool, String, Option<T>, Vec<T>",
                    pat_ident.ident
                ),
            ));
        }
        if let Some(expr) = &default {
            if optional {
                return Err(syn::Error::new_spanned(
                    ty,
                    format!(
                        "{macro_name} : le paramètre `{}` ne peut pas avoir à la fois \
                         `Option<T>` et une valeur par défaut",
                        pat_ident.ident
                    ),
                ));
            }
            // Le défaut doit être un littéral : il est publié en JSON.
            if !is_json_literal(expr) {
                return Err(syn::Error::new_spanned(
                    expr,
                    format!(
                        "{macro_name} : la valeur par défaut de `{}` doit être un \
                         littéral (publiée dans le JSON Schema)",
                        pat_ident.ident
                    ),
                ));
            }
        }
        params.push(Param {
            ident: pat_ident.ident.clone(),
            ty: ty.clone(),
            optional,
            default,
        });
    }
    Ok(params)
}

/// Vrai couple (type utile, optionnel ?) pour `Option<T>` ; sinon (T, false).
fn strip_option(ty: &syn::Type) -> (&syn::Type, bool) {
    if let syn::Type::Path(path) = ty {
        if let Some(segment) = path.path.segments.last() {
            if segment.ident == "Option" {
                if let syn::PathArguments::AngleBracketed(args) = &segment.arguments {
                    if let Some(syn::GenericArgument::Type(inner)) = args.args.first() {
                        return (inner, true);
                    }
                }
            }
        }
    }
    (ty, false)
}

/// Expression `zod` pour un type supporté — `None` si non supporté.
fn schema_for(ty: &syn::Type) -> Option<TokenStream2> {
    let syn::Type::Path(path) = ty else {
        return None;
    };
    let segment = path.path.segments.last()?;
    match segment.ident.to_string().as_str() {
        "i8" | "i16" | "i32" | "i64" | "isize" | "u8" | "u16" | "u32" | "u64" | "usize" => {
            Some(quote! { ::litemcp::zod::integer() })
        }
        "f32" | "f64" => Some(quote! { ::litemcp::zod::number() }),
        "bool" => Some(quote! { ::litemcp::zod::boolean() }),
        "String" => Some(quote! { ::litemcp::zod::string() }),
        "Vec" => {
            if let syn::PathArguments::AngleBracketed(args) = &segment.arguments {
                if let Some(syn::GenericArgument::Type(inner)) = args.args.first() {
                    let inner_schema = schema_for(inner)?;
                    return Some(quote! { ::litemcp::zod::array(#inner_schema) });
                }
            }
            None
        }
        _ => None,
    }
}

/// Expression qui extrait la valeur du JSON vers le type `ty` — le schéma a
/// déjà validé présence et type à l'exécution.
///
/// `fallback` : expression de repli quand la valeur est absente (paramètre
/// avec valeur par défaut). Absente → repli sur le défaut de la valeur
/// extraite (`0`, `""`, `false`…).
fn extract_from(
    ty: &syn::Type,
    value: &TokenStream2,
    fallback: Option<TokenStream2>,
    fallback_string: Option<TokenStream2>,
) -> TokenStream2 {
    let (inner, optional) = strip_option(ty);
    let extraction = extract_required(inner, value, fallback.clone(), fallback_string);
    if optional {
        quote! {
            if #value.is_null() { ::core::option::Option::None }
            else { ::core::option::Option::Some(#extraction) }
        }
    } else {
        extraction
    }
}

fn extract_required(
    ty: &syn::Type,
    value: &TokenStream2,
    fallback: Option<TokenStream2>,
    fallback_string: Option<TokenStream2>,
) -> TokenStream2 {
    let syn::Type::Path(path) = ty else {
        return quote! { ::core::default::Default::default() };
    };
    let ident = path
        .path
        .segments
        .last()
        .map_or_else(String::new, |s| s.ident.to_string());
    match ident.as_str() {
        "i8" | "i16" | "i32" | "i64" | "isize" | "u8" | "u16" | "u32" | "u64" | "usize" => {
            // Le repli (valeur par défaut) est une valeur JSON : on la
            // convertit via `as_i64` comme une valeur normale.
            match &fallback {
                Some(fallback) => quote! {
                    #value.as_i64().or_else(|| #fallback.as_i64()).unwrap_or_default() as #ty
                },
                None => quote! { #value.as_i64().unwrap_or_default() as #ty },
            }
        }
        "f32" | "f64" => match &fallback {
            Some(fallback) => quote! {
                #value.as_f64().or_else(|| #fallback.as_f64()).unwrap_or_default() as #ty
            },
            None => quote! { #value.as_f64().unwrap_or_default() as #ty },
        },
        "bool" => match &fallback {
            Some(fallback) => quote! {
                #value.as_bool().or_else(|| #fallback.as_bool()).unwrap_or_default()
            },
            None => quote! { #value.as_bool().unwrap_or_default() },
        },
        "String" => match &fallback_string {
            // Le repli est une `String` **possédée** : on ne renvoie
            // jamais une référence sur un `Value` temporaire.
            Some(fallback) => quote! {
                match #value.as_str() {
                    Some(__litemcp_s) => ::std::string::String::from(__litemcp_s),
                    None => #fallback,
                }
            },
            None => quote! { #value.as_str().unwrap_or("").to_owned() },
        },
        "Vec" => {
            let inner = match &path.path.segments.last().unwrap().arguments {
                syn::PathArguments::AngleBracketed(args) => match args.args.first() {
                    Some(syn::GenericArgument::Type(inner)) => inner,
                    _ => return quote! { ::core::default::Default::default() },
                },
                _ => return quote! { ::core::default::Default::default() },
            };
            let item = extract_required(inner, &quote! { __litemcp_item }, None, None);
            match &fallback {
                Some(fallback) => quote! {
                    #value.as_array()
                        .or_else(|| #fallback.as_array())
                        .map(|__litemcp_list| __litemcp_list.iter()
                            .map(|__litemcp_item| #item)
                            .collect())
                        .unwrap_or_default()
                },
                None => quote! {
                    #value.as_array().map(|__litemcp_list| __litemcp_list.iter()
                        .map(|__litemcp_item| #item)
                        .collect())
                    .unwrap_or_default()
                },
            }
        }
        _ => quote! { ::core::default::Default::default() },
    }
}

fn err(message: &str) -> TokenStream {
    quote! { compile_error!(#message); }.into()
}

/// L'expression est-elle un **littéral JSON** publiable ?
///
/// Seuls les littéraux (entier, flottant, chaîne, booléen, `null`) et les
/// tableaux d'entre eux sont acceptés : une valeur par défaut est
/// publiée dans le JSON Schema, donc elle doit être sérialisable.
fn is_json_literal(expr: &syn::Expr) -> bool {
    match expr {
        syn::Expr::Lit(_) => true,
        syn::Expr::Array(array) => array.elems.iter().all(is_json_literal),
        syn::Expr::Group(group) => is_json_literal(&group.expr),
        syn::Expr::Paren(paren) => is_json_literal(&paren.expr),
        syn::Expr::Unary(unary) => is_json_literal(&unary.expr),
        _ => false,
    }
}

/// Idem [`json_literal`] mais le littéral est **construit sur place** (un
/// `serde_json::Value` temporaire possédé) — aucune référence à un
/// temporaire ne subsiste dans le code généré.
fn json_literal_owned(expr: &syn::Expr) -> TokenStream2 {
    match expr {
        syn::Expr::Lit(literal) => match &literal.lit {
            syn::Lit::Int(int) => {
                let n = int.base10_digits().parse::<i64>().unwrap_or(0);
                quote! { ::serde_json::Value::from(#n) }
            }
            syn::Lit::Float(float) => {
                let n = float.base10_digits().parse::<f64>().unwrap_or(0.0);
                quote! { ::serde_json::Value::from(#n) }
            }
            syn::Lit::Str(text) => {
                // `String::from(...)` : la valeur est **possédée**, jamais
                // une référence à un temporaire.
                let value = text.value();
                quote! { ::serde_json::Value::from(::std::string::String::from(#value)) }
            }
            syn::Lit::Bool(flag) => {
                let value = flag.value();
                quote! { ::serde_json::Value::from(#value) }
            }
            _ => quote! { ::serde_json::Value::Null },
        },
        syn::Expr::Array(array) => {
            let items: Vec<TokenStream2> = array.elems.iter().map(json_literal_owned).collect();
            quote! { ::serde_json::Value::from(::serde_json::json!([ #( #items ),* ])) }
        }
        syn::Expr::Group(group) => json_literal_owned(&group.expr),
        syn::Expr::Paren(paren) => json_literal_owned(&paren.expr),
        syn::Expr::Unary(unary) => {
            let inner = json_literal_owned(&unary.expr);
            match unary.op {
                syn::UnOp::Neg(_) => quote! { -#inner },
                _ => quote! { #inner },
            }
        }
        _ => quote! { ::serde_json::Value::Null },
    }
}

/// Convertit un littéral Rust en expression JSON **possédée**
/// (`serde_json::Value`).
///
/// La valeur est construite sur place : aucune référence à un temporaire
/// ne subsiste dans le code généré.
fn json_literal(expr: &syn::Expr) -> TokenStream2 {
    json_literal_owned(expr)
}
