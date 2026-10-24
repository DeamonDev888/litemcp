//! Macro `#[tool]` — déclare un outil MCP à partir d'une fonction Rust
//! ordinaire : le schéma d'entrée (module `litemcp::zod`) est **dérivé des
//! types des paramètres**, et une fonction `register_<nom>` est générée.
//!
//! ```ignore
//! use litemcp::macros::tool;
//!
//! #[tool(name = "add", description = "Additionne deux entiers")]
//! fn add(a: i64, b: i64) -> litemcp::ToolResult {
//!     Ok(serde_json::json!({ "somme": a + b }))
//! }
//!
//! // génère, à côté de `add` :
//! //   pub fn register_add(server: litemcp::ServerBuilder) -> litemcp::ServerBuilder
//! // — utilisable avec `.with_tool(register_add)`.
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
use syn::{ItemFn, Type};

/// Attributs acceptés : `name = "…"` (défaut : nom de la fonction) et
/// `description = "…"` (**requis** — `tools/list` en a besoin).
#[proc_macro_attribute]
pub fn tool(attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let attr_parser = syn::meta::parser(|meta| {
        if meta.path.is_ident("name") {
            name = Some(meta.value()?.parse::<syn::LitStr>()?.value());
            Ok(())
        } else if meta.path.is_ident("description") {
            description = Some(meta.value()?.parse::<syn::LitStr>()?.value());
            Ok(())
        } else {
            Err(meta.error("attribut inconnu — attendus : name, description"))
        }
    });
    if let Err(error) = syn::parse::Parser::parse2(attr_parser, attr.into()) {
        return error_into_tokens(error);
    }
    let Some(description) = description else {
        return error_message("#[tool] exige description = \"…\"");
    };

    let function = syn::parse::<ItemFn>(item);
    let Ok(function) = function else {
        return error_message("#[tool] s'applique à une fonction");
    };
    let fn_name = function.sig.ident.clone();
    let tool_name = name.unwrap_or_else(|| fn_name.to_string());

    // Un paramètre = un champ : (nom du champ, type, optionnel ?).
    let mut fields: Vec<(syn::Ident, Type, bool)> = Vec::new();
    for arg in &function.sig.inputs {
        let syn::FnArg::Typed(pat_type) = arg else {
            return error_message("#[tool] : `self` et les variadiques ne sont pas supportés");
        };
        let syn::Pat::Ident(pat_ident) = &*pat_type.pat else {
            return error_message("#[tool] : chaque paramètre doit être un identifiant simple");
        };
        let ty = &*pat_type.ty;
        let (inner, optional) = strip_option(ty);
        // Vérifie le type immédiatement pour un message d'erreur précis.
        if schema_for(inner).is_none() {
            return error_message(&format!(
                "#[tool] : type non supporté pour le paramètre `{}` — \
                 entiers, f32/f64, bool, String, Option<T>, Vec<T>",
                pat_ident.ident
            ));
        }
        fields.push((pat_ident.ident.clone(), (*pat_type.ty).clone(), optional));
    }

    // Chaîne de construction du schéma : zod::object().field(…).opt(…).
    let schema = fields.iter().map(|(ident, ty, optional)| {
        let key = ident.to_string();
        let (inner, _) = strip_option(ty);
        let inner_schema = schema_for(inner).expect("type déjà vérifié");
        if *optional {
            quote! { .opt(#key, #inner_schema) }
        } else {
            quote! { .field(#key, #inner_schema) }
        }
    });
    // Arguments extraits du JSON validé — la validation du schéma garantit
    // présence et type, `unwrap_or_default` n'est qu'une ceinture.
    let args = fields.iter().map(|(ident, ty, _)| {
        let key = ident.to_string();
        extract_from(ty, &quote! { __litemcp_args[#key] })
    });

    let register = format_ident!("register_{}", fn_name);
    let expanded = quote! {
        #function

        #[doc = concat!("Enregistre l'outil `", #tool_name, "` (généré par #[tool]).")]
        #[allow(non_snake_case)]
        pub fn #register(
            __litemcp_server: ::litemcp::ServerBuilder,
        ) -> ::litemcp::ServerBuilder {
            __litemcp_server.tool_typed(
                #tool_name,
                #description,
                ::litemcp::zod::object() #(#schema)*,
                |__litemcp_args| #fn_name(#(#args),*),
            )
        }
    };
    expanded.into()
}

/// Vrai couple (type utile, optionnel ?) pour `Option<T>` ; sinon (T, false).
fn strip_option(ty: &Type) -> (&Type, bool) {
    if let Type::Path(path) = ty {
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
fn schema_for(ty: &Type) -> Option<TokenStream2> {
    let Type::Path(path) = ty else {
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
fn extract_from(ty: &Type, value: &TokenStream2) -> TokenStream2 {
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

fn extract_required(ty: &Type, value: &TokenStream2) -> TokenStream2 {
    let Type::Path(path) = ty else {
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

fn error_into_tokens(error: syn::Error) -> TokenStream {
    let message = error.to_string();
    error_message(&message)
}

fn error_message(message: &str) -> TokenStream {
    quote! { compile_error!(#message); }.into()
}
