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
    let params = match collect_params(&function.sig, macro_name) {
        Ok(params) => params,
        Err(error) => return error.to_compile_error().into(),
    };
    let fn_name = function.sig.ident.clone();
    let name = attrs.name.unwrap_or_else(|| fn_name.to_string());
    let register = format_ident!("register_{}", fn_name);

    // Chaîne `.field(…)`/`.opt(…)` sur l'objet racine, puis arguments
    // extraits du JSON validé — la validation garantit présence et type.
    let schema = params.iter().map(|param| {
        let key = param.ident.to_string();
        let (inner, _) = strip_option(&param.ty);
        let inner_schema = schema_for(inner).expect("type déjà vérifié");
        if param.optional {
            quote! { .opt(#key, #inner_schema) }
        } else {
            quote! { .field(#key, #inner_schema) }
        }
    });
    let args = params.iter().map(|param| {
        let key = param.ident.to_string();
        extract_from(&param.ty, &quote! { __litemcp_args[#key] })
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
        } else {
            Err(meta.error("attribut inconnu — attendus : name, description, uri"))
        }
    });
    syn::parse::Parser::parse2(parser, attr)?;
    Ok(out)
}

/// Un paramètre de fonction annotée = un champ de schéma.
struct Param {
    ident: syn::Ident,
    ty: syn::Type,
    optional: bool,
}

fn collect_params(sig: &Signature, macro_name: &str) -> syn::Result<Vec<Param>> {
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
        params.push(Param {
            ident: pat_ident.ident.clone(),
            ty: ty.clone(),
            optional,
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
fn extract_from(ty: &syn::Type, value: &TokenStream2) -> TokenStream2 {
    let (inner, optional) = strip_option(ty);
    let extraction = extract_required(inner, value);
    if optional {
        quote! {
            if #value.is_null() { ::core::option::Option::None }
            else { ::core::option::Option::Some(#extraction) }
        }
    } else {
        extraction
    }
}

fn extract_required(ty: &syn::Type, value: &TokenStream2) -> TokenStream2 {
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
            quote! { #value.as_i64().unwrap_or_default() as #ty }
        }
        "f32" | "f64" => quote! { #value.as_f64().unwrap_or_default() as #ty },
        "bool" => quote! { #value.as_bool().unwrap_or_default() },
        "String" => quote! { #value.as_str().unwrap_or("").to_owned() },
        "Vec" => {
            let inner = match &path.path.segments.last().unwrap().arguments {
                syn::PathArguments::AngleBracketed(args) => match args.args.first() {
                    Some(syn::GenericArgument::Type(inner)) => inner,
                    _ => return quote! { ::core::default::Default::default() },
                },
                _ => return quote! { ::core::default::Default::default() },
            };
            let item = extract_required(inner, &quote! { __litemcp_item });
            quote! {
                #value.as_array().map(|__litemcp_list| __litemcp_list.iter()
                    .map(|__litemcp_item| #item)
                    .collect())
                    .unwrap_or_default()
            }
        }
        _ => quote! { ::core::default::Default::default() },
    }
}

fn err(message: &str) -> TokenStream {
    quote! { compile_error!(#message); }.into()
}
