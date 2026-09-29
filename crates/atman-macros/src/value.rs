use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{
    Data, DataEnum, DataStruct, DeriveInput, Expr, Fields, Meta, Path, Token, Type, parse::Parser,
    parse_quote, punctuated::Punctuated,
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
    let root = parse_options(attr)?;
    let item: DeriveInput = syn::parse2(item)?;
    if !item.generics.params.is_empty() || item.generics.where_clause.is_some() {
        return Err(syn::Error::new_spanned(
            &item.generics,
            "value does not support generics, lifetimes, or where clauses",
        ));
    }

    let implementations = match &item.data {
        Data::Struct(data) => expand_struct(&item, data, &root)?,
        Data::Enum(data) => expand_enum(&item, data, &root)?,
        Data::Union(data) => {
            return Err(syn::Error::new_spanned(
                data.union_token,
                "value does not support unions",
            ));
        }
    };

    Ok(quote! {
        #item
        #implementations
    })
}

fn parse_options(attr: TokenStream) -> syn::Result<Path> {
    let options = Punctuated::<Meta, Token![,]>::parse_terminated.parse2(attr)?;
    let mut crate_path = None;
    for option in options {
        match option {
            Meta::NameValue(value) if value.path.is_ident("crate_path") => {
                if crate_path.is_some() {
                    return Err(syn::Error::new_spanned(value, "duplicate crate_path"));
                }
                let Expr::Path(path) = &value.value else {
                    return Err(syn::Error::new_spanned(
                        &value.value,
                        "crate_path must be a Rust path, such as ::my_rt",
                    ));
                };
                crate_path = Some(path.path.clone());
            }
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "unsupported value option; expected crate_path = ::path",
                ));
            }
        }
    }
    Ok(crate_path.unwrap_or_else(|| parse_quote!(::atman_rt)))
}

fn expand_struct(item: &DeriveInput, data: &DataStruct, root: &Path) -> syn::Result<TokenStream> {
    let Fields::Named(fields) = &data.fields else {
        return Err(syn::Error::new_spanned(
            &data.fields,
            "value requires a struct with named fields",
        ));
    };
    let ident = &item.ident;
    let type_name = rust_name(ident);
    let field_idents = fields
        .named
        .iter()
        .map(|field| field.ident.as_ref().expect("named field"))
        .collect::<Vec<_>>();
    let field_types = fields
        .named
        .iter()
        .map(|field| &field.ty)
        .collect::<Vec<&Type>>();
    let field_names = field_idents
        .iter()
        .map(|ident| rust_name(ident))
        .collect::<Vec<_>>();
    let field_slots = field_idents
        .iter()
        .enumerate()
        .map(|(index, _)| format_ident!("__atman_value_field_{index}"))
        .collect::<Vec<_>>();

    let input_specs = field_names.iter().zip(&field_types).map(|(name, ty)| {
        quote! {
            __atman_fields.push(#root::catalog::FieldSpec {
                name: #name.into(),
                ty: <#ty as #root::binding::Input<__AtmanValueP, __AtmanValueE>>::input_type(),
            });
        }
    });
    let output_specs = field_names.iter().zip(&field_types).map(|(name, ty)| {
        quote! {
            __atman_fields.push(#root::catalog::FieldSpec {
                name: #name.into(),
                ty: <#ty as #root::binding::Output<__AtmanValueP, __AtmanValueE>>::output_type(),
            });
        }
    });
    let slot_declarations = field_slots.iter().map(|slot| {
        quote! {
            let mut #slot: ::core::option::Option<#root::Value<__AtmanValueP, __AtmanValueE>> =
                ::core::option::Option::None;
        }
    });
    let field_matches = field_names.iter().zip(&field_slots).map(|(name, slot)| {
        quote! {
            #name => {
                if #slot.is_some() {
                    return ::core::result::Result::Err(
                        #root::binding::BindingError::duplicate_field(#name),
                    );
                }
                #slot = ::core::option::Option::Some(__atman_field_value);
            }
        }
    });
    let required_checks =
        field_names
            .iter()
            .zip(&field_types)
            .zip(&field_slots)
            .map(|((name, ty), slot)| {
                quote! {
                    if <#ty as #root::binding::Input<__AtmanValueP, __AtmanValueE>>::REQUIRED
                        && #slot.is_none()
                    {
                        return ::core::result::Result::Err(
                            #root::binding::BindingError::missing_field(#name),
                        );
                    }
                }
            });
    let decoded_fields = field_idents
        .iter()
        .zip(&field_types)
        .zip(&field_names)
        .zip(&field_slots)
        .map(|(((field, ty), name), slot)| {
            quote! {
                #field: <#ty as #root::binding::Input<__AtmanValueP, __AtmanValueE>>::decode_input(
                    #slot,
                    __atman_context,
                )
                .map_err(|__atman_error| __atman_error.at_field(#name))?
            }
        });
    let output_bindings = field_idents
        .iter()
        .zip(&field_slots)
        .map(|(field, slot)| quote!(#field: #slot));
    let encoded_fields = field_types.iter().zip(&field_names).zip(&field_slots).map(
        |((ty, name), slot)| {
            quote! {
                __atman_fields.push((
                    #name.into(),
                    <#ty as #root::binding::Output<__AtmanValueP, __AtmanValueE>>::encode_output(
                        #slot,
                        __atman_context,
                    )
                    .map_err(|__atman_error| __atman_error.at_field(#name))?,
                ));
            }
        },
    );

    Ok(quote! {
        impl<__AtmanValueP, __AtmanValueE> #root::binding::Input<__AtmanValueP, __AtmanValueE>
            for #ident
        where
            __AtmanValueP: #root::HostPayload,
            #(#field_types: #root::binding::Input<__AtmanValueP, __AtmanValueE>,)*
        {
            const REQUIRED: bool = true;

            fn input_type() -> #root::catalog::TypeSpec {
                let mut __atman_fields: #root::__private::Vec<#root::catalog::FieldSpec> =
                    #root::__private::Vec::new();
                #(#input_specs)*
                #root::catalog::TypeSpec::Struct(#root::catalog::StructSpec {
                    name: #type_name.into(),
                    fields: __atman_fields,
                })
            }

            fn decode_input(
                __atman_value: ::core::option::Option<#root::Value<__AtmanValueP, __AtmanValueE>>,
                __atman_context: &#root::binding::Context<__AtmanValueP, __AtmanValueE>,
            ) -> ::core::result::Result<Self, #root::binding::BindingError> {
                let __atman_fields = match __atman_value {
                    ::core::option::Option::Some(#root::Value::Struct(__atman_fields)) => {
                        __atman_fields
                    }
                    ::core::option::Option::Some(__atman_other) => {
                        return ::core::result::Result::Err(
                            #root::binding::BindingError::type_mismatch(
                                <Self as #root::binding::Input<
                                    __AtmanValueP,
                                    __AtmanValueE,
                                >>::input_type(),
                                __atman_other.kind_name(),
                            ),
                        );
                    }
                    ::core::option::Option::None => {
                        return ::core::result::Result::Err(
                            #root::binding::BindingError::missing_value(),
                        );
                    }
                };

                #(#slot_declarations)*
                for (__atman_field_name, __atman_field_value) in __atman_fields {
                    match __atman_field_name.as_str() {
                        #(#field_matches)*
                        _ => {}
                    }
                }
                #(#required_checks)*

                ::core::result::Result::Ok(Self {
                    #(#decoded_fields,)*
                })
            }
        }

        impl<__AtmanValueP, __AtmanValueE> #root::binding::Output<__AtmanValueP, __AtmanValueE>
            for #ident
        where
            #(#field_types: #root::binding::Output<__AtmanValueP, __AtmanValueE>,)*
        {
            fn output_type() -> #root::catalog::TypeSpec {
                let mut __atman_fields: #root::__private::Vec<#root::catalog::FieldSpec> =
                    #root::__private::Vec::new();
                #(#output_specs)*
                #root::catalog::TypeSpec::Struct(#root::catalog::StructSpec {
                    name: #type_name.into(),
                    fields: __atman_fields,
                })
            }

            fn encode_output(
                self,
                __atman_context: &#root::binding::Context<__AtmanValueP, __AtmanValueE>,
            ) -> ::core::result::Result<
                #root::Value<__AtmanValueP, __AtmanValueE>,
                #root::binding::BindingError,
            > {
                __atman_context.output_transaction(|| {
                    let Self { #(#output_bindings,)* } = self;
                    let mut __atman_fields: #root::__private::Vec<(
                        #root::__private::String,
                        #root::Value<__AtmanValueP, __AtmanValueE>,
                    )> = #root::__private::Vec::new();
                    #(#encoded_fields)*
                    ::core::result::Result::Ok(#root::Value::Struct(__atman_fields))
                })
            }
        }

        impl #root::binding::PresentValue for #ident {}
    })
}

fn expand_enum(item: &DeriveInput, data: &DataEnum, root: &Path) -> syn::Result<TokenStream> {
    for variant in &data.variants {
        if !matches!(variant.fields, Fields::Unit) {
            return Err(syn::Error::new_spanned(
                &variant.fields,
                "value only supports fieldless enum variants",
            ));
        }
    }

    let ident = &item.ident;
    let type_name = rust_name(ident);
    let variants = data
        .variants
        .iter()
        .map(|variant| &variant.ident)
        .collect::<Vec<_>>();
    let variant_names = variants
        .iter()
        .map(|variant| rust_name(variant))
        .collect::<Vec<_>>();
    let input_variants = variant_names
        .iter()
        .zip(&variants)
        .map(|(name, variant)| quote!(#name => Self::#variant));
    let decode_variant = if variants.is_empty() {
        quote! {
            ::core::result::Result::Err(#root::binding::BindingError::unknown_variant(
                <Self as #root::binding::Input<
                    __AtmanValueP,
                    __AtmanValueE,
                >>::input_type(),
                __atman_variant,
            ))
        }
    } else {
        quote! {
            let __atman_value = match __atman_variant.as_str() {
                #(#input_variants,)*
                _ => {
                    return ::core::result::Result::Err(
                        #root::binding::BindingError::unknown_variant(
                            <Self as #root::binding::Input<
                                __AtmanValueP,
                                __AtmanValueE,
                            >>::input_type(),
                            __atman_variant,
                        ),
                    );
                }
            };
            ::core::result::Result::Ok(__atman_value)
        }
    };
    let output_variants = variants
        .iter()
        .zip(&variant_names)
        .map(|(variant, name)| quote!(Self::#variant => #root::Value::Str(#name.into())));
    let encode_output = if variants.is_empty() {
        quote!(match self {})
    } else {
        quote! {
            ::core::result::Result::Ok(match self {
                #(#output_variants,)*
            })
        }
    };
    let input_specs = variant_names.iter().map(|name| {
        quote! {
            __atman_variants.push(#name.into());
        }
    });
    let output_specs = variant_names.iter().map(|name| {
        quote! {
            __atman_variants.push(#name.into());
        }
    });

    Ok(quote! {
        impl<__AtmanValueP, __AtmanValueE> #root::binding::Input<__AtmanValueP, __AtmanValueE>
            for #ident
        where
            __AtmanValueP: #root::HostPayload,
        {
            const REQUIRED: bool = true;

            fn input_type() -> #root::catalog::TypeSpec {
                let mut __atman_variants: #root::__private::Vec<#root::__private::String> =
                    #root::__private::Vec::new();
                #(#input_specs)*
                #root::catalog::TypeSpec::Enum(#root::catalog::EnumSpec {
                    name: #type_name.into(),
                    variants: __atman_variants,
                })
            }

            fn decode_input(
                __atman_value: ::core::option::Option<#root::Value<__AtmanValueP, __AtmanValueE>>,
                _context: &#root::binding::Context<__AtmanValueP, __AtmanValueE>,
            ) -> ::core::result::Result<Self, #root::binding::BindingError> {
                let __atman_variant = match __atman_value {
                    ::core::option::Option::Some(#root::Value::Str(__atman_variant)) => {
                        __atman_variant
                    }
                    ::core::option::Option::Some(__atman_other) => {
                        return ::core::result::Result::Err(
                            #root::binding::BindingError::type_mismatch(
                                <Self as #root::binding::Input<
                                    __AtmanValueP,
                                    __AtmanValueE,
                                >>::input_type(),
                                __atman_other.kind_name(),
                            ),
                        );
                    }
                    ::core::option::Option::None => {
                        return ::core::result::Result::Err(
                            #root::binding::BindingError::missing_value(),
                        );
                    }
                };
                #decode_variant
            }
        }

        impl<__AtmanValueP, __AtmanValueE> #root::binding::Output<__AtmanValueP, __AtmanValueE>
            for #ident
        {
            fn output_type() -> #root::catalog::TypeSpec {
                let mut __atman_variants: #root::__private::Vec<#root::__private::String> =
                    #root::__private::Vec::new();
                #(#output_specs)*
                #root::catalog::TypeSpec::Enum(#root::catalog::EnumSpec {
                    name: #type_name.into(),
                    variants: __atman_variants,
                })
            }

            fn encode_output(
                self,
                _context: &#root::binding::Context<__AtmanValueP, __AtmanValueE>,
            ) -> ::core::result::Result<
                #root::Value<__AtmanValueP, __AtmanValueE>,
                #root::binding::BindingError,
            > {
                #encode_output
            }
        }

        impl #root::binding::PresentValue for #ident {}
    })
}

fn rust_name(ident: &syn::Ident) -> String {
    let name = ident.to_string();
    name.strip_prefix("r#").unwrap_or(&name).to_owned()
}
