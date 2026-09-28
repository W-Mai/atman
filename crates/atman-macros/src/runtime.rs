//! Product runtime bindings generated from annotated host functions.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{Item, LitStr, Path};

use crate::common::{CancelKind, Mode, ParsedModule, ToolFn, ValueType, parse_module};

pub(crate) fn expand(attr: TokenStream, item: TokenStream) -> TokenStream {
    match expand_inner(attr.into(), item.into()) {
        Ok(expanded) => expanded.into(),
        Err(error) => error.into_compile_error().into(),
    }
}

fn expand_inner(attr: TokenStream2, item: TokenStream2) -> syn::Result<TokenStream2> {
    let ParsedModule {
        mut item,
        tools,
        crate_path,
    } = parse_module(attr, item, Mode::Runtime)?;

    let registrations = tools
        .iter()
        .map(|tool| registration(tool, &crate_path))
        .collect::<syn::Result<Vec<_>>>()?;

    let register: Item = syn::parse2(quote! {
        /// Registers every annotated function in this module.
        pub fn register(registry: &#crate_path::ToolRegistry) -> ::core::result::Result<(), #crate_path::RegisterError> {
            #(#registrations)*
            ::core::result::Result::Ok(())
        }
    })?;
    item.content
        .as_mut()
        .expect("parse_module accepts only inline modules")
        .1
        .push(register);
    Ok(quote!(#item))
}

fn registration(tool: &ToolFn, path: &Path) -> syn::Result<TokenStream2> {
    let function = &tool.ident;
    let name = LitStr::new(&tool.name, function.span());
    let tier = match tool.tier.expect("runtime parser validates tier") {
        0 => quote!(#path::Tier::Zero),
        1 => quote!(#path::Tier::One),
        2 => quote!(#path::Tier::Two),
        3 => quote!(#path::Tier::Three),
        4 => quote!(#path::Tier::Four),
        _ => unreachable!("runtime parser validates tier range"),
    };
    let cancel = match tool.cancel.unwrap_or(CancelKind::AbortSafe) {
        CancelKind::AbortSafe => quote!(#path::CancelBehavior::AbortSafe),
        CancelKind::Revertible => quote!(#path::CancelBehavior::Revertible),
        CancelKind::Atomic => quote!(#path::CancelBehavior::Atomic),
        CancelKind::Irreversible => quote!(#path::CancelBehavior::Irreversible),
    };
    let call_mode = if tool.is_async {
        quote!(.call_mode(#path::ToolCallMode::Deferred))
    } else {
        quote!()
    };
    let description = if tool.docs.is_empty() {
        quote!()
    } else {
        let docs = LitStr::new(&tool.docs, function.span());
        quote!(.description(#docs))
    };

    let mut properties = Vec::new();
    let mut required = Vec::new();
    let mut decoders = Vec::new();
    let mut call_args = Vec::new();

    for (index, param) in tool
        .params
        .iter()
        .filter(|param| !matches!(param.ty, ValueType::ToolCtx))
        .enumerate()
    {
        let param_name = LitStr::new(
            param.ident.to_string().trim_start_matches("r#"),
            param.ident.span(),
        );
        let schema = schema(&param.ty, path)?;
        properties.push(quote!(#param_name: #schema));
        if !matches!(param.ty, ValueType::Option(_)) {
            required.push(quote!(#param_name));
        }

        let local = format_ident!("__atman_arg_{index}");
        let index = syn::Index::from(index);
        let decode = decode(&param.ty, quote!(__atman_value), path)?;
        let source = quote! {
            __atman_args.named(#param_name)
                .or_else(|| __atman_args.positional.get(#index))
        };
        let decoder = if matches!(param.ty, ValueType::Option(_)) {
            quote! {
                let #local = match #source {
                    ::core::option::Option::None => ::core::option::Option::None,
                    ::core::option::Option::Some(__atman_value) => #decode?,
                };
            }
        } else {
            quote! {
                let __atman_value = #source.ok_or_else(|| #path::RuntimeError::MissingArg(#param_name.into()))?;
                let #local = #decode?;
            }
        };
        decoders.push(decoder);
        call_args.push(quote!(#local));
    }
    if tool
        .params
        .last()
        .is_some_and(|param| matches!(param.ty, ValueType::ToolCtx))
    {
        call_args.push(quote!(__atman_ctx));
    }

    let output = encode(&tool.result.value, quote!(__atman_result), path)?;
    let call = if tool.is_async {
        quote!(#function(#(#call_args),*).await)
    } else {
        quote!(#function(#(#call_args),*))
    };
    let invoke = if tool.result.error.is_some() {
        quote! {
            let __atman_result = #call.map_err(|error| -> #path::RuntimeError { error.into() })?;
        }
    } else {
        quote! { let __atman_result = #call; }
    };

    Ok(quote! {
        registry.register_fn(
            #path::ToolDefinition::new(#name, #tier)
                #description
                .cancel_behavior(#cancel)
                #call_mode
                .input_schema(#path::__private::serde_json::json!({
                    "type": "object",
                    "properties": { #(#properties),* },
                    "required": [ #(#required),* ]
                })),
            |__atman_args: #path::ToolArgs, __atman_ctx: #path::ToolCtx| async move {
                #(#decoders)*
                #invoke
                ::core::result::Result::Ok(#output)
            },
        )?;
    })
}

fn schema(ty: &ValueType, path: &Path) -> syn::Result<TokenStream2> {
    let json = quote!(#path::__private::serde_json::json!);
    Ok(match ty {
        ValueType::Unit => quote!(#json({ "type": "null" })),
        ValueType::Int => quote!(#json({ "type": "integer" })),
        ValueType::Float => quote!(#json({ "type": "number" })),
        ValueType::Bool => quote!(#json({ "type": "boolean" })),
        ValueType::String => quote!(#json({ "type": "string" })),
        ValueType::Option(inner) => {
            let inner = schema(inner, path)?;
            quote!(#json({ "anyOf": [#inner, { "type": "null" }] }))
        }
        ValueType::Vec(inner) => {
            let inner = schema(inner, path)?;
            quote!(#json({ "type": "array", "items": #inner }))
        }
        ValueType::ToolCtx => {
            return Err(syn::Error::new_spanned(
                path,
                "ToolCtx is injected and cannot appear in a tool schema",
            ));
        }
    })
}

/// Decodes a borrowed Atman value into an owned Rust value.
fn decode(ty: &ValueType, value: TokenStream2, path: &Path) -> syn::Result<TokenStream2> {
    let mismatch = |expected: &str| {
        quote! {
            ::core::result::Result::Err(#path::RuntimeError::TypeMismatch {
                expected: #expected.into(),
                actual: __atman_wrong.kind_name().into(),
            })
        }
    };
    Ok(match ty {
        ValueType::Unit => {
            let mismatch = mismatch("unit");
            quote! { match #value {
                #path::Value::Unit => ::core::result::Result::Ok(()),
                __atman_wrong => #mismatch,
            } }
        }
        ValueType::Int => {
            let mismatch = mismatch("int");
            quote! { match #value {
                #path::Value::Int(__atman_inner) => ::core::result::Result::Ok(*__atman_inner),
                __atman_wrong => #mismatch,
            } }
        }
        ValueType::Float => {
            let mismatch = mismatch("float");
            quote! { match #value {
                #path::Value::Float(__atman_inner) => ::core::result::Result::Ok(*__atman_inner),
                __atman_wrong => #mismatch,
            } }
        }
        ValueType::Bool => {
            let mismatch = mismatch("bool");
            quote! { match #value {
                #path::Value::Bool(__atman_inner) => ::core::result::Result::Ok(*__atman_inner),
                __atman_wrong => #mismatch,
            } }
        }
        ValueType::String => {
            let mismatch = mismatch("string");
            quote! { match #value {
                #path::Value::Str(__atman_inner) => ::core::result::Result::Ok(__atman_inner.clone()),
                __atman_wrong => #mismatch,
            } }
        }
        ValueType::Option(inner) => {
            let inner = decode(inner, quote!(__atman_some), path)?;
            quote! { match #value {
                #path::Value::Unit => ::core::result::Result::Ok(::core::option::Option::None),
                __atman_some => #inner.map(::core::option::Option::Some),
            } }
        }
        ValueType::Vec(inner) => {
            let inner = decode(inner, quote!(__atman_item), path)?;
            let mismatch = mismatch("list");
            quote! { match #value {
                #path::Value::List(__atman_items) => __atman_items
                    .iter()
                    .map(|__atman_item| #inner)
                    .collect::<::core::result::Result<::std::vec::Vec<_>, #path::RuntimeError>>(),
                __atman_wrong => #mismatch,
            } }
        }
        ValueType::ToolCtx => {
            return Err(syn::Error::new_spanned(
                path,
                "ToolCtx is injected and cannot be decoded from tool arguments",
            ));
        }
    })
}

fn encode(ty: &ValueType, value: TokenStream2, path: &Path) -> syn::Result<TokenStream2> {
    Ok(match ty {
        ValueType::Unit => quote!(#path::Value::Unit),
        ValueType::Int => quote!(#path::Value::Int(#value)),
        ValueType::Float => quote!(#path::Value::Float(#value)),
        ValueType::Bool => quote!(#path::Value::Bool(#value)),
        ValueType::String => quote!(#path::Value::Str(#value)),
        ValueType::Option(inner) => {
            let inner = encode(inner, quote!(__atman_some), path)?;
            quote! { match #value {
                ::core::option::Option::Some(__atman_some) => #inner,
                ::core::option::Option::None => #path::Value::Unit,
            } }
        }
        ValueType::Vec(inner) => {
            let inner = encode(inner, quote!(__atman_item), path)?;
            quote! { #path::Value::List(#value.into_iter().map(|__atman_item| #inner).collect()) }
        }
        ValueType::ToolCtx => {
            return Err(syn::Error::new_spanned(
                path,
                "ToolCtx cannot be returned from a tool",
            ));
        }
    })
}
