use proc_macro2::TokenStream;
use quote::quote;
use syn::{
    Data, DeriveInput, Expr, Meta, Path, Token, parse::Parser, parse_quote, punctuated::Punctuated,
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
            "resource does not support generics, lifetimes, or where clauses",
        ));
    }
    if let Data::Union(data) = &item.data {
        return Err(syn::Error::new_spanned(
            data.union_token,
            "resource does not support unions",
        ));
    }

    let ident = &item.ident;
    let type_name = rust_name(ident);

    Ok(quote! {
        #item

        impl #root::resource::ResourceType for #ident {
            const TYPE_NAME: &'static str = #type_name;
        }

        impl<__AtmanResourceP, __AtmanResourceE>
            #root::binding::Output<__AtmanResourceP, __AtmanResourceE> for #ident
        where
            __AtmanResourceP: #root::resource::ResourcePayload,
            #ident: ::core::marker::Send + ::core::marker::Sync + 'static,
        {
            fn output_type() -> #root::catalog::TypeSpec {
                #root::catalog::TypeSpec::Resource(#root::catalog::ResourceSpec {
                    name: ::core::option::Option::Some(
                        <Self as #root::resource::ResourceType>::TYPE_NAME.into(),
                    ),
                })
            }

            fn encode_output(
                self,
                __atman_context: &#root::binding::Context<__AtmanResourceP, __AtmanResourceE>,
            ) -> ::core::result::Result<
                #root::Value<__AtmanResourceP, __AtmanResourceE>,
                #root::binding::BindingError,
            > {
                __atman_context.output_transaction(|| {
                    let __atman_handle = __atman_context.resources()?.insert(self)?;
                    __atman_context.record_output_resource(__atman_handle.erased());
                    ::core::result::Result::Ok(#root::Value::Host(
                        <__AtmanResourceP as #root::resource::ResourcePayload>::from_resource(
                            __atman_handle.erased(),
                        ),
                    ))
                })
            }
        }

        impl #root::binding::PresentValue for #ident {}
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
                    "unsupported resource option; expected crate_path = ::path",
                ));
            }
        }
    }
    Ok(crate_path.unwrap_or_else(|| parse_quote!(::atman_rt)))
}

fn rust_name(ident: &syn::Ident) -> String {
    let name = ident.to_string();
    name.strip_prefix("r#").unwrap_or(&name).to_owned()
}
