use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{ImplItem, Item, ItemImpl, ItemMod, Path, parse_quote};

use crate::rt_parse::{
    ImplParamKind, ImplTool, ModuleTool, ParsedImpl, ParsedModule, parse_impl, parse_module,
};

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
    match syn::parse2::<Item>(item.clone())? {
        Item::Mod(item_mod) => expand_module(attr, item_mod),
        Item::Impl(item_impl) => expand_stateful_impl(attr, item_impl),
        other => Err(syn::Error::new_spanned(
            other,
            "tools requires an inline module or a concrete inherent impl",
        )),
    }
}

fn expand_module(attr: TokenStream, item: ItemMod) -> syn::Result<TokenStream> {
    let ParsedModule {
        mut item,
        tools,
        root,
    } = parse_module(attr, item)?;
    let registrations = tools.iter().map(|tool| register_tool(tool, &root));
    let error_bounds = tools.iter().filter_map(|tool| {
        tool.result
            .error
            .as_ref()
            .map(|error| quote!(E: ::core::convert::From<#error>,))
    });
    let router: Item = parse_quote! {
        pub fn router<P, E>() -> ::core::result::Result<#root::ToolRouter<P, E>, #root::ToolRegisterError>
        where
            P: #root::HostPayload + ::core::marker::Send + ::core::marker::Sync + 'static,
            E: #root::ValueError + ::core::marker::Send + ::core::marker::Sync + 'static,
            #(#error_bounds)*
        {
            let mut __atman_router = #root::ToolRouter::<P, E>::new();
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

fn expand_stateful_impl(attr: TokenStream, item: ItemImpl) -> syn::Result<TokenStream> {
    let ParsedImpl {
        mut item,
        tools,
        root,
        namespace,
        self_ty,
        factory_ident,
    } = parse_impl(attr, item)?;

    let into_binding: ImplItem = parse_quote! {
        #[doc(hidden)]
        pub fn into_atman_binding(self) -> #factory_ident {
            #factory_ident { __atman_host: self }
        }
    };
    item.items.push(into_binding);

    let input_bounds = tools.iter().flat_map(|tool| {
        tool.params.iter().map(|param| match &param.kind {
            ImplParamKind::Owned(ty) => {
                quote!(#ty: #root::binding::Input<__AtmanBindingP, __AtmanBindingE>,)
            }
            ImplParamKind::SharedResource(ty) => quote!(
                #ty: #root::resource::ResourceType
                    + ::core::marker::Send
                    + ::core::marker::Sync
                    + 'static,
            ),
        })
    });
    let output_bounds = tools.iter().map(|tool| {
        let ty = &tool.result.value;
        quote!(#ty: #root::binding::Output<__AtmanBindingP, __AtmanBindingE>,)
    });
    let error_bounds = tools.iter().filter_map(|tool| {
        tool.result
            .error
            .as_ref()
            .map(|error| quote!(__AtmanBindingE: ::core::convert::From<#error>,))
    });
    let registrations = tools
        .iter()
        .map(|tool| register_stateful_tool(tool, &namespace, &root));
    let release = register_release_tool(&namespace, &root);

    Ok(quote! {
        #item

        #[doc(hidden)]
        pub struct #factory_ident {
            __atman_host: #self_ty,
        }

        impl<__AtmanBindingP, __AtmanBindingE>
            #root::binding::Factory<__AtmanBindingP, __AtmanBindingE> for #factory_ident
        where
            #self_ty: ::core::marker::Send + ::core::marker::Sync + 'static,
            __AtmanBindingP: #root::resource::ResourcePayload
                + ::core::clone::Clone
                + ::core::marker::Send
                + ::core::marker::Sync
                + 'static,
            __AtmanBindingE: #root::ValueError
                + ::core::clone::Clone
                + ::core::marker::Send
                + ::core::marker::Sync
                + 'static,
            #(#input_bounds)*
            #(#output_bounds)*
            #(#error_bounds)*
        {
            fn build(
                self,
            ) -> ::core::result::Result<
                #root::ToolRouter<__AtmanBindingP, __AtmanBindingE>,
                #root::ToolRegisterError,
            > {
                let mut __atman_router =
                    #root::ToolRouter::<__AtmanBindingP, __AtmanBindingE>::new();
                let __atman_host = #root::__private::Arc::new(self.__atman_host);
                let __atman_resources = #root::__private::Arc::new(
                    #root::resource::ResourceRegistry::new()?,
                );
                let __atman_context = #root::__private::Arc::new(
                    #root::binding::Context::with_resources(__atman_resources),
                );

                #(#registrations)*
                #release

                ::core::result::Result::Ok(__atman_router)
            }
        }
    })
}

fn register_stateful_tool(tool: &ImplTool, namespace: &str, root: &Path) -> TokenStream {
    let method = &tool.ident;
    let full_name = format!("{namespace}.{}", tool.leaf_name);
    let description = &tool.docs;
    let result_ty = &tool.result.value;
    let mode = if tool.is_async {
        quote!(#root::ToolCallMode::Deferred)
    } else {
        quote!(#root::ToolCallMode::Immediate)
    };
    let register = if tool.is_async {
        quote!(register_with_spec)
    } else {
        quote!(register_sync_with_spec)
    };

    let param_specs = tool.params.iter().enumerate().map(|(position, param)| {
        let name = &param.name;
        match &param.kind {
            ImplParamKind::Owned(ty) => quote! {
                __atman_params.push(#root::catalog::ToolParamSpec {
                    name: #root::__private::String::from(#name),
                    position: #position,
                    required: <#ty as #root::binding::Input<
                        __AtmanBindingP,
                        __AtmanBindingE,
                    >>::REQUIRED,
                    ty: <#ty as #root::binding::Input<
                        __AtmanBindingP,
                        __AtmanBindingE,
                    >>::input_type(),
                });
            },
            ImplParamKind::SharedResource(ty) => quote! {
                __atman_params.push(#root::catalog::ToolParamSpec {
                    name: #root::__private::String::from(#name),
                    position: #position,
                    required: true,
                    ty: #root::catalog::TypeSpec::Resource(#root::catalog::ResourceSpec {
                        name: ::core::option::Option::Some(#root::__private::String::from(
                            <#ty as #root::resource::ResourceType>::TYPE_NAME,
                        )),
                    }),
                });
            },
        }
    });

    let slots = tool
        .params
        .iter()
        .enumerate()
        .map(|(position, _)| format_ident!("__atman_argument_{position}"))
        .collect::<Vec<_>>();
    let bindings = tool
        .params
        .iter()
        .enumerate()
        .zip(&slots)
        .map(|((position, param), slot)| {
            let name = &param.name;
            match &param.kind {
                ImplParamKind::Owned(ty) => quote! {
                    let #slot = <#ty as #root::binding::Input<
                        __AtmanBindingP,
                        __AtmanBindingE,
                    >>::decode_input(
                        __atman_args.take(#name, #position),
                        __atman_call_context.as_ref(),
                    )
                    .map_err(|__atman_error| {
                        <__AtmanBindingE as #root::ValueError>::binding_error(
                            __atman_error.at_argument(#name),
                        )
                    })?;
                },
                ImplParamKind::SharedResource(ty) => quote! {
                    let #slot = #root::binding::borrow_resource::<
                        #ty,
                        __AtmanBindingP,
                        __AtmanBindingE,
                    >(
                        __atman_args.take(#name, #position),
                        __atman_call_context.as_ref(),
                    )
                    .map_err(|__atman_error| {
                        <__AtmanBindingE as #root::ValueError>::binding_error(
                            __atman_error.at_argument(#name),
                        )
                    })?;
                },
            }
        });
    let call_args = tool.params.iter().zip(&slots).map(|(param, slot)| {
        if matches!(param.kind, ImplParamKind::SharedResource(_)) {
            quote!(&*#slot)
        } else {
            quote!(#slot)
        }
    });
    let call = if tool.is_async {
        quote!(__atman_call_host.#method(#(#call_args),*).await)
    } else {
        quote!(__atman_call_host.#method(#(#call_args),*))
    };
    let call = if tool.result.error.is_some() {
        quote!((#call).map_err(::core::convert::Into::<__AtmanBindingE>::into)?)
    } else {
        call
    };
    let handler_body = quote! {
        #(#bindings)*
        let __atman_result = #call;
        let __atman_output_context = __atman_call_context.for_call();
        __atman_output_context.output_transaction(|| <#result_ty as #root::binding::Output<
                __AtmanBindingP,
                __AtmanBindingE,
            >>::encode_output(__atman_result, &__atman_output_context))
        .map_err(|__atman_error| {
            <__AtmanBindingE as #root::ValueError>::binding_error(
                __atman_error.at_argument("return"),
            )
        })
    };
    let handler = if tool.is_async {
        quote! {
            move |mut __atman_args: #root::ToolArgs<
                __AtmanBindingP,
                __AtmanBindingE,
            >| {
                let __atman_call_host =
                    #root::__private::Arc::clone(&__atman_tool_host);
                let __atman_call_context =
                    #root::__private::Arc::clone(&__atman_tool_context);
                async move { #handler_body }
            }
        }
    } else {
        quote! {
            move |mut __atman_args: #root::ToolArgs<
                __AtmanBindingP,
                __AtmanBindingE,
            >| {
                let __atman_call_host = &__atman_tool_host;
                let __atman_call_context = &__atman_tool_context;
                #handler_body
            }
        }
    };

    quote! {
        {
            let mut __atman_params: #root::__private::Vec<#root::catalog::ToolParamSpec> =
                #root::__private::Vec::new();
            #(#param_specs)*
            let __atman_spec = #root::catalog::ToolSpec {
                name: #root::__private::String::from(#full_name),
                namespace: #root::__private::String::from(#namespace),
                description: #root::__private::String::from(#description),
                mode: #mode,
                params: __atman_params,
                result: <#result_ty as #root::binding::Output<
                    __AtmanBindingP,
                    __AtmanBindingE,
                >>::output_type(),
            };
            let __atman_tool_host = #root::__private::Arc::clone(&__atman_host);
            let __atman_tool_context = #root::__private::Arc::clone(&__atman_context);
            __atman_router.#register(__atman_spec, #handler)?;
        }
    }
}

fn register_release_tool(namespace: &str, root: &Path) -> TokenStream {
    let full_name = format!("{namespace}.release");
    quote! {
        {
            let mut __atman_params: #root::__private::Vec<#root::catalog::ToolParamSpec> =
                #root::__private::Vec::new();
            __atman_params.push(#root::catalog::ToolParamSpec {
                name: #root::__private::String::from("resource"),
                position: 0,
                required: true,
                ty: #root::catalog::TypeSpec::Resource(#root::catalog::ResourceSpec {
                    name: ::core::option::Option::None,
                }),
            });
            let __atman_spec = #root::catalog::ToolSpec {
                name: #root::__private::String::from(#full_name),
                namespace: #root::__private::String::from(#namespace),
                description: #root::__private::String::from("Release a host resource."),
                mode: #root::ToolCallMode::Immediate,
                params: __atman_params,
                result: #root::catalog::TypeSpec::Unit,
            };
            let __atman_release_context =
                #root::__private::Arc::clone(&__atman_context);
            __atman_router.register_sync_with_spec(
                __atman_spec,
                move |mut __atman_args: #root::ToolArgs<
                    __AtmanBindingP,
                    __AtmanBindingE,
                >| {
                    let __atman_handle = #root::binding::decode_resource_handle(
                        __atman_args.take("resource", 0),
                        __atman_release_context.as_ref(),
                    )
                    .map_err(|__atman_error| {
                        <__AtmanBindingE as #root::ValueError>::binding_error(
                            __atman_error.at_argument("resource"),
                        )
                    })?;
                    __atman_release_context
                        .resources()
                        .and_then(|__atman_resources| {
                            __atman_resources
                                .release(__atman_handle)
                                .map_err(::core::convert::Into::into)
                        })
                        .map_err(|__atman_error: #root::binding::BindingError| {
                            <__AtmanBindingE as #root::ValueError>::binding_error(
                                __atman_error.at_argument("resource"),
                            )
                        })?;
                    ::core::result::Result::Ok(#root::Value::Unit)
                },
            )?;
        }
    }
}

fn register_tool(tool: &ModuleTool, root: &Path) -> TokenStream {
    let name = &tool.name;
    let namespace = name.rsplit_once('.').map_or("", |(namespace, _)| namespace);
    let description = &tool.docs;
    let function = &tool.ident;
    let result_ty = &tool.result.value;
    let mode = if tool.is_async {
        quote!(#root::ToolCallMode::Deferred)
    } else {
        quote!(#root::ToolCallMode::Immediate)
    };
    let register = if tool.is_async {
        quote!(register_with_spec)
    } else {
        quote!(register_sync_with_spec)
    };
    let param_specs = tool.params.iter().enumerate().map(|(position, param)| {
        let parameter_name = param.ident.to_string();
        let parameter_name = parameter_name.trim_start_matches("r#");
        let ty = &param.ty;
        quote! {
            __atman_params.push(#root::catalog::ToolParamSpec {
                name: #root::__private::String::from(#parameter_name),
                position: #position,
                required: <#ty as #root::binding::Input<P, E>>::REQUIRED,
                ty: <#ty as #root::binding::Input<P, E>>::input_type(),
            });
        }
    });
    let slots = tool
        .params
        .iter()
        .enumerate()
        .map(|(position, _)| format_ident!("__atman_argument_{position}"))
        .collect::<Vec<_>>();
    let bindings = tool
        .params
        .iter()
        .enumerate()
        .zip(&slots)
        .map(|((position, param), slot)| {
            let parameter_name = param.ident.to_string();
            let parameter_name = parameter_name.trim_start_matches("r#");
            let ty = &param.ty;
            quote! {
                let #slot = <#ty as #root::binding::Input<P, E>>::decode_input(
                    __atman_args.take(#parameter_name, #position),
                    &__atman_context,
                )
                .map_err(|__atman_error| {
                    <E as #root::ValueError>::binding_error(
                        __atman_error.at_argument(#parameter_name),
                    )
                })?;
            }
        });
    let call = if tool.is_async {
        quote!(#function(#(#slots),*).await)
    } else {
        quote!(#function(#(#slots),*))
    };
    let call = if tool.result.error.is_some() {
        quote!((#call).map_err(::core::convert::Into::<E>::into)?)
    } else {
        call
    };
    let body = quote! {
        let __atman_context = #root::binding::Context::<P, E>::value_only();
        #(#bindings)*
        let __atman_result = #call;
        __atman_context.output_transaction(||
            <#result_ty as #root::binding::Output<P, E>>::encode_output(
                __atman_result,
                &__atman_context,
            )
        )
        .map_err(|__atman_error| {
            <E as #root::ValueError>::binding_error(
                __atman_error.at_argument("return"),
            )
        })
    };
    let handler = if tool.is_async {
        quote! {
            |mut __atman_args: #root::ToolArgs<P, E>| async move { #body }
        }
    } else {
        quote! {
            |mut __atman_args: #root::ToolArgs<P, E>| { #body }
        }
    };
    quote! {
        {
            let mut __atman_params: #root::__private::Vec<#root::catalog::ToolParamSpec> =
                #root::__private::Vec::new();
            #(#param_specs)*
            let __atman_spec = #root::catalog::ToolSpec {
                name: #root::__private::String::from(#name),
                namespace: #root::__private::String::from(#namespace),
                description: #root::__private::String::from(#description),
                mode: #mode,
                params: __atman_params,
                result: <#result_ty as #root::binding::Output<P, E>>::output_type(),
            };
            __atman_router.#register(__atman_spec, #handler)?;
        }
    }
}
