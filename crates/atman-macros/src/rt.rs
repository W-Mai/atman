use proc_macro2::TokenStream;
use quote::quote;
use syn::{Ident, Item, Path, parse_quote};

use crate::common::{Mode, ParsedModule, ToolFn, ValueType, parse_module};

pub(crate) fn expand(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    match expand_inner(attr.into(), item.into()) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.into_compile_error().into(),
    }
}

fn expand_inner(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let ParsedModule {
        mut item,
        tools,
        crate_path,
    } = parse_module(attr, item, Mode::Rt)?;
    let registrations = tools.iter().map(|tool| register_tool(tool, &crate_path));
    let error_bounds = tools.iter().filter_map(|tool| {
        tool.result
            .error
            .as_ref()
            .map(|error| quote!(E: ::core::convert::From<#error>,))
    });
    let router: Item = parse_quote! {
        pub fn router<P, E>() -> ::core::result::Result<#crate_path::ToolRouter<P, E>, #crate_path::ToolRegisterError>
        where
            P: #crate_path::HostPayload + ::core::marker::Send + ::core::marker::Sync + 'static,
            E: #crate_path::ValueError + ::core::marker::Send + ::core::marker::Sync + 'static,
            #(#error_bounds)*
        {
            let mut __atman_router = #crate_path::ToolRouter::<P, E>::new();
            #(#registrations)*
            ::core::result::Result::Ok(__atman_router)
        }
    };
    item.content
        .as_mut()
        .expect("validated inline module")
        .1
        .push(router);
    Ok(quote!(#item))
}

fn register_tool(tool: &ToolFn, root: &Path) -> TokenStream {
    let name = &tool.name;
    let function = &tool.ident;
    let mut bindings = Vec::new();
    let mut arguments: Vec<&Ident> = Vec::new();
    for (index, param) in tool.params.iter().enumerate() {
        let ident = &param.ident;
        let argument_name = ident.to_string().trim_start_matches("r#").to_owned();
        let raw =
            quote!(__atman_args.named(#argument_name).or_else(|| __atman_args.positional(#index)));
        let binding = if matches!(param.ty, ValueType::Option(_)) {
            let value = decode(&param.ty, quote!(__atman_value), root);
            quote! {
                let #ident = match #raw {
                    ::core::option::Option::Some(__atman_value) => (#value)?,
                    ::core::option::Option::None => ::core::option::Option::None,
                };
            }
        } else {
            let value = decode(&param.ty, quote!(__atman_value), root);
            quote! {
                let __atman_value = __atman_args.get(#argument_name, #index)?;
                let #ident = (#value)?;
            }
        };
        bindings.push(binding);
        arguments.push(ident);
    }
    let call = if tool.is_async {
        quote!(#function(#(#arguments),*).await)
    } else {
        quote!(#function(#(#arguments),*))
    };
    let call = if tool.result.error.is_some() {
        quote!((#call).map_err(::core::convert::Into::<E>::into)?)
    } else {
        call
    };
    let encoded = encode(&tool.result.value, quote!(__atman_result), root);
    quote! {
        __atman_router.register(#name, |__atman_args: #root::ToolArgs<P, E>| async move {
            #(#bindings)*
            let __atman_result = #call;
            ::core::result::Result::Ok(#encoded)
        })?;
    }
}

fn decode(ty: &ValueType, value: TokenStream, root: &Path) -> TokenStream {
    let mismatch = |expected: &str| {
        quote! {
            ::core::result::Result::Err(<E as #root::ValueError>::type_mismatch(
                #expected,
                __atman_other.kind_name().into(),
            ))
        }
    };
    match ty {
        ValueType::Unit => {
            let error = mismatch("unit");
            quote! {
                match #value {
                    #root::Value::Unit => ::core::result::Result::Ok(()),
                    __atman_other => #error,
                }
            }
        }
        ValueType::Int => {
            let error = mismatch("int");
            quote! {
                match #value {
                    #root::Value::Int(__atman_number) => ::core::result::Result::Ok(*__atman_number),
                    __atman_other => #error,
                }
            }
        }
        ValueType::Float => {
            let error = mismatch("float");
            quote! {
                match #value {
                    #root::Value::Float(__atman_number) => ::core::result::Result::Ok(*__atman_number),
                    __atman_other => #error,
                }
            }
        }
        ValueType::Bool => {
            let error = mismatch("bool");
            quote! {
                match #value {
                    #root::Value::Bool(__atman_boolean) => ::core::result::Result::Ok(*__atman_boolean),
                    __atman_other => #error,
                }
            }
        }
        ValueType::String => {
            let error = mismatch("string");
            quote! {
                match #value {
                    #root::Value::Str(__atman_text) => ::core::result::Result::Ok(__atman_text.clone()),
                    __atman_other => #error,
                }
            }
        }
        ValueType::Option(inner) => {
            let inner = decode(inner, quote!(__atman_present), root);
            quote! {
                match #value {
                    #root::Value::Unit => ::core::result::Result::Ok(::core::option::Option::None),
                    __atman_present => ::core::result::Result::Ok(::core::option::Option::Some((#inner)?)),
                }
            }
        }
        ValueType::Vec(inner) => {
            let inner = decode(inner, quote!(__atman_item), root);
            let error = mismatch("list");
            quote! {
                match #value {
                    #root::Value::List(__atman_items) => __atman_items
                        .iter()
                        .map(|__atman_item| #inner)
                        .collect::<::core::result::Result<_, E>>(),
                    __atman_other => #error,
                }
            }
        }
        ValueType::ToolCtx => unreachable!("ToolCtx is forbidden in RT mode"),
    }
}

fn encode(ty: &ValueType, value: TokenStream, root: &Path) -> TokenStream {
    match ty {
        ValueType::Unit => quote!({ let _ = #value; #root::Value::Unit }),
        ValueType::Int => quote!(#root::Value::Int(#value)),
        ValueType::Float => quote!(#root::Value::Float(#value)),
        ValueType::Bool => quote!(#root::Value::Bool(#value)),
        ValueType::String => quote!(#root::Value::Str(#value)),
        ValueType::Option(inner) => {
            let inner = encode(inner, quote!(__atman_present), root);
            quote! {
                match #value {
                    ::core::option::Option::Some(__atman_present) => #inner,
                    ::core::option::Option::None => #root::Value::Unit,
                }
            }
        }
        ValueType::Vec(inner) => {
            let inner = encode(inner, quote!(__atman_item), root);
            quote! {
                #root::Value::List(#value.into_iter().map(|__atman_item| #inner).collect())
            }
        }
        ValueType::ToolCtx => unreachable!("ToolCtx is forbidden as a return type"),
    }
}
