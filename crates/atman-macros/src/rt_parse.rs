use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{
    Attribute, Expr, FnArg, GenericArgument, Ident, ImplItem, ImplItemFn, Item, ItemFn, ItemImpl,
    ItemMod, Lit, Meta, Pat, Path, PathArguments, ReturnType, Token, Type, ext::IdentExt,
    parse::Parser, parse_quote, punctuated::Punctuated,
};

pub(crate) struct ParsedModule {
    pub item: ItemMod,
    pub tools: Vec<ModuleTool>,
    pub root: Path,
}

pub(crate) struct ModuleTool {
    pub ident: Ident,
    pub name: String,
    pub params: Vec<ModuleParam>,
    pub result: ImplReturn,
    pub is_async: bool,
    pub docs: String,
}

pub(crate) struct ModuleParam {
    pub ident: Ident,
    pub ty: Type,
}

pub(crate) struct ParsedImpl {
    pub item: ItemImpl,
    pub tools: Vec<ImplTool>,
    pub root: Path,
    pub namespace: String,
    pub self_ty: Type,
    pub factory_ident: Ident,
}

pub(crate) struct ImplTool {
    pub ident: Ident,
    pub leaf_name: String,
    pub params: Vec<ImplParam>,
    pub result: ImplReturn,
    pub is_async: bool,
    pub docs: String,
}

pub(crate) struct ImplParam {
    pub name: String,
    pub kind: ImplParamKind,
}

pub(crate) enum ImplParamKind {
    Owned(Type),
    SharedResource(Type),
}

pub(crate) struct ImplReturn {
    pub value: Type,
    pub error: Option<Type>,
}

pub(crate) fn parse_module(attr: TokenStream, mut item: ItemMod) -> syn::Result<ParsedModule> {
    let root = parse_module_options(attr)?;
    let Some((_, items)) = item.content.as_mut() else {
        return Err(syn::Error::new_spanned(
            &item.ident,
            "tools requires an inline module",
        ));
    };
    if let Some(existing) = items
        .iter()
        .find(|item| matches!(item, Item::Fn(function) if function.sig.ident == "router"))
    {
        return Err(syn::Error::new_spanned(
            existing,
            "tools generates `router`; rename the existing function",
        ));
    }

    let mut tools = Vec::new();
    for item in items {
        let Item::Fn(function) = item else {
            continue;
        };
        let Some(tool_attr_index) = function
            .attrs
            .iter()
            .position(|attr| attr.path().is_ident("tool"))
        else {
            continue;
        };
        let tool_attr = function.attrs.remove(tool_attr_index);
        if function
            .attrs
            .iter()
            .any(|attr| attr.path().is_ident("tool"))
        {
            return Err(syn::Error::new_spanned(
                &function.sig.ident,
                "only one #[tool] attribute is allowed per function",
            ));
        }
        tools.push(parse_module_tool(function, &tool_attr)?);
    }

    Ok(ParsedModule { item, tools, root })
}

fn parse_module_options(attr: TokenStream) -> syn::Result<Path> {
    let options = Punctuated::<Meta, Token![,]>::parse_terminated.parse2(attr)?;
    let mut root = None;
    for option in options {
        match option {
            Meta::NameValue(value) if value.path.is_ident("crate_path") => {
                if root.is_some() {
                    return Err(syn::Error::new_spanned(value, "duplicate crate_path"));
                }
                let Expr::Path(path) = &value.value else {
                    return Err(syn::Error::new_spanned(
                        &value.value,
                        "crate_path must be a Rust path, such as ::my_rt",
                    ));
                };
                root = Some(path.path.clone());
            }
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "unsupported tools option; expected crate_path = ::path",
                ));
            }
        }
    }
    Ok(root.unwrap_or_else(|| parse_quote!(::atman_rt)))
}

fn parse_module_tool(function: &ItemFn, attr: &Attribute) -> syn::Result<ModuleTool> {
    if !function.sig.generics.params.is_empty() || function.sig.generics.where_clause.is_some() {
        return Err(syn::Error::new_spanned(
            &function.sig.generics,
            "tool functions cannot be generic; use a typed wrapper function",
        ));
    }
    if function.sig.variadic.is_some()
        || function.sig.abi.is_some()
        || function.sig.unsafety.is_some()
    {
        return Err(syn::Error::new_spanned(
            &function.sig,
            "tool functions cannot be variadic, unsafe, or extern",
        ));
    }

    let mut params = Vec::new();
    for input in &function.sig.inputs {
        let FnArg::Typed(input) = input else {
            return Err(syn::Error::new_spanned(
                input,
                "tool functions cannot have a receiver",
            ));
        };
        let Pat::Ident(pattern) = input.pat.as_ref() else {
            return Err(syn::Error::new_spanned(
                &input.pat,
                "tool parameters must have simple names",
            ));
        };
        if pattern.subpat.is_some() {
            return Err(syn::Error::new_spanned(
                &input.pat,
                "tool parameters must have simple names",
            ));
        }
        if type_contains_reference(&input.ty) {
            return Err(syn::Error::new_spanned(
                &input.ty,
                "module tool parameters must be owned values",
            ));
        }
        params.push(ModuleParam {
            ident: pattern.ident.clone(),
            ty: input.ty.as_ref().clone(),
        });
    }

    let result = parse_return(&function.sig.output)?;
    if type_contains_reference(&result.value) {
        return Err(syn::Error::new_spanned(
            &result.value,
            "tool functions cannot return references",
        ));
    }

    Ok(ModuleTool {
        ident: function.sig.ident.clone(),
        name: parse_tool_name(&function.sig.ident, attr)?,
        params,
        result,
        is_async: function.sig.asyncness.is_some(),
        docs: docs(&function.attrs),
    })
}

pub(crate) fn parse_impl(attr: TokenStream, mut item: ItemImpl) -> syn::Result<ParsedImpl> {
    let (namespace, root) = parse_options(attr, &item)?;

    if let Some((_, path, _)) = &item.trait_ {
        return Err(syn::Error::new_spanned(
            path,
            "stateful tools require an inherent impl, not a trait impl",
        ));
    }
    if item.unsafety.is_some() {
        return Err(syn::Error::new_spanned(
            item.impl_token,
            "stateful tools do not support unsafe impls",
        ));
    }
    if !item.generics.params.is_empty() || item.generics.where_clause.is_some() {
        return Err(syn::Error::new_spanned(
            &item.generics,
            "stateful tools require a concrete, non-generic impl",
        ));
    }

    let self_ty = item.self_ty.as_ref().clone();
    let type_ident = concrete_type_ident(&self_ty)?;
    let type_name = rust_name(type_ident);
    let factory_ident = format_ident!("__Atman{}Binding", type_name, span = type_ident.span(),);

    if let Some(existing) = item.items.iter().find(
        |item| matches!(item, ImplItem::Fn(method) if method.sig.ident == "into_atman_binding"),
    ) {
        return Err(syn::Error::new_spanned(
            existing,
            "tools generates `into_atman_binding`; rename the existing method",
        ));
    }

    let mut tools = Vec::new();
    for impl_item in &mut item.items {
        let ImplItem::Fn(method) = impl_item else {
            continue;
        };
        let Some(tool_attr_index) = method
            .attrs
            .iter()
            .position(|attr| attr.path().is_ident("tool"))
        else {
            continue;
        };
        let tool_attr = method.attrs.remove(tool_attr_index);
        if method.attrs.iter().any(|attr| attr.path().is_ident("tool")) {
            return Err(syn::Error::new_spanned(
                &method.sig.ident,
                "only one #[tool] attribute is allowed per method",
            ));
        }
        tools.push(parse_tool(method, &tool_attr)?);
    }

    Ok(ParsedImpl {
        item,
        tools,
        root,
        namespace,
        self_ty,
        factory_ident,
    })
}

fn parse_options(attr: TokenStream, item: &ItemImpl) -> syn::Result<(String, Path)> {
    let options = Punctuated::<Meta, Token![,]>::parse_terminated.parse2(attr)?;
    let mut namespace = None;
    let mut root = None;
    for option in options {
        match option {
            Meta::NameValue(value) if value.path.is_ident("namespace") => {
                if namespace.is_some() {
                    return Err(syn::Error::new_spanned(value, "duplicate namespace"));
                }
                let Expr::Lit(expr) = &value.value else {
                    return Err(syn::Error::new_spanned(
                        &value.value,
                        "namespace must be a string",
                    ));
                };
                let Lit::Str(literal) = &expr.lit else {
                    return Err(syn::Error::new_spanned(
                        &expr.lit,
                        "namespace must be a string",
                    ));
                };
                validate_dotted_name(literal.value().as_str(), literal, "namespace")?;
                namespace = Some(literal.value());
            }
            Meta::NameValue(value) if value.path.is_ident("crate_path") => {
                if root.is_some() {
                    return Err(syn::Error::new_spanned(value, "duplicate crate_path"));
                }
                let Expr::Path(path) = &value.value else {
                    return Err(syn::Error::new_spanned(
                        &value.value,
                        "crate_path must be a Rust path, such as ::my_rt",
                    ));
                };
                root = Some(path.path.clone());
            }
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "unsupported tools option; expected namespace = \"...\" or crate_path = ::path",
                ));
            }
        }
    }

    let namespace = namespace.ok_or_else(|| {
        syn::Error::new_spanned(
            item.impl_token,
            "stateful tools require namespace = \"...\"",
        )
    })?;
    Ok((namespace, root.unwrap_or_else(|| parse_quote!(::atman_rt))))
}

fn concrete_type_ident(ty: &Type) -> syn::Result<&Ident> {
    let Type::Path(path) = peel_type(ty) else {
        return Err(syn::Error::new_spanned(
            ty,
            "stateful tools require a concrete nominal self type",
        ));
    };
    if path.qself.is_some() {
        return Err(syn::Error::new_spanned(
            ty,
            "stateful tools require a concrete nominal self type",
        ));
    }
    let mut segments = path.path.segments.iter();
    let Some(segment) = segments.next() else {
        return Err(syn::Error::new_spanned(
            ty,
            "stateful tools require a concrete nominal self type",
        ));
    };
    if segments.next().is_some() || !matches!(segment.arguments, PathArguments::None) {
        return Err(syn::Error::new_spanned(
            ty,
            "stateful tools require an unqualified, non-generic self type; place the impl beside the type declaration",
        ));
    }
    Ok(&segment.ident)
}

fn parse_tool(method: &ImplItemFn, attr: &Attribute) -> syn::Result<ImplTool> {
    let leaf_name = parse_tool_name(&method.sig.ident, attr)?;
    if leaf_name == "release" {
        return Err(syn::Error::new_spanned(
            attr,
            "`release` is reserved for the generated resource release tool",
        ));
    }

    if !method.sig.generics.params.is_empty() || method.sig.generics.where_clause.is_some() {
        return Err(syn::Error::new_spanned(
            &method.sig.generics,
            "tool methods cannot be generic; use a typed wrapper method",
        ));
    }
    if method.sig.variadic.is_some() || method.sig.abi.is_some() || method.sig.unsafety.is_some() {
        return Err(syn::Error::new_spanned(
            &method.sig,
            "tool methods cannot be variadic, unsafe, or extern",
        ));
    }

    let mut inputs = method.sig.inputs.iter();
    let receiver = inputs.next().ok_or_else(|| {
        syn::Error::new_spanned(
            &method.sig.ident,
            "tool methods require an `&self` receiver",
        )
    })?;
    validate_receiver(receiver)?;

    let mut params = Vec::new();
    for input in inputs {
        let FnArg::Typed(input) = input else {
            return Err(syn::Error::new_spanned(
                input,
                "tool methods may only have one `&self` receiver",
            ));
        };
        let Pat::Ident(pattern) = input.pat.as_ref() else {
            return Err(syn::Error::new_spanned(
                &input.pat,
                "tool parameters must have simple names",
            ));
        };
        if pattern.subpat.is_some() {
            return Err(syn::Error::new_spanned(
                &input.pat,
                "tool parameters must have simple names",
            ));
        }
        let name = rust_name(&pattern.ident);
        let kind = match peel_type(&input.ty) {
            Type::Reference(reference) => {
                if reference.mutability.is_some() {
                    return Err(syn::Error::new_spanned(
                        &input.ty,
                        "tool resource parameters only support shared `&T` borrows",
                    ));
                }
                if reference.lifetime.is_some() {
                    return Err(syn::Error::new_spanned(
                        &input.ty,
                        "tool resource parameters must use an elided `&T` lifetime",
                    ));
                }
                if method.sig.asyncness.is_some() {
                    return Err(syn::Error::new_spanned(
                        &input.ty,
                        "async tool methods cannot borrow resource parameters",
                    ));
                }
                if matches!(peel_type(&reference.elem), Type::Reference(_)) {
                    return Err(syn::Error::new_spanned(
                        &input.ty,
                        "tool resource parameters only support a single shared `&T` borrow",
                    ));
                }
                ImplParamKind::SharedResource(reference.elem.as_ref().clone())
            }
            _ => ImplParamKind::Owned(input.ty.as_ref().clone()),
        };
        params.push(ImplParam { name, kind });
    }

    let result = parse_return(&method.sig.output)?;
    if type_contains_reference(&result.value) {
        return Err(syn::Error::new_spanned(
            &result.value,
            "tool methods cannot return references",
        ));
    }

    Ok(ImplTool {
        ident: method.sig.ident.clone(),
        leaf_name,
        params,
        result,
        is_async: method.sig.asyncness.is_some(),
        docs: docs(&method.attrs),
    })
}

fn validate_receiver(input: &FnArg) -> syn::Result<()> {
    let FnArg::Receiver(receiver) = input else {
        return Err(syn::Error::new_spanned(
            input,
            "tool methods require an `&self` receiver",
        ));
    };
    let has_elided_shared_reference = receiver
        .reference
        .as_ref()
        .is_some_and(|(_, lifetime)| lifetime.is_none());
    if receiver.colon_token.is_some()
        || !has_elided_shared_reference
        || receiver.mutability.is_some()
    {
        return Err(syn::Error::new_spanned(
            receiver,
            "tool methods require an `&self` receiver; owned, mutable, and typed receivers are not supported",
        ));
    }
    Ok(())
}

fn parse_tool_name(ident: &Ident, attr: &Attribute) -> syn::Result<String> {
    let options = match &attr.meta {
        Meta::Path(_) => Punctuated::new(),
        Meta::List(_) => attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?,
        Meta::NameValue(_) => {
            return Err(syn::Error::new_spanned(
                attr,
                "tool options must use #[tool(...)]",
            ));
        }
    };
    let mut name = None;
    for option in options {
        match option {
            Meta::NameValue(value) if value.path.is_ident("name") => {
                if name.is_some() {
                    return Err(syn::Error::new_spanned(value, "duplicate tool name"));
                }
                let Expr::Lit(expr) = &value.value else {
                    return Err(syn::Error::new_spanned(
                        &value.value,
                        "tool name must be a string",
                    ));
                };
                let Lit::Str(literal) = &expr.lit else {
                    return Err(syn::Error::new_spanned(
                        &expr.lit,
                        "tool name must be a string",
                    ));
                };
                validate_dotted_name(literal.value().as_str(), literal, "tool name")?;
                name = Some(literal.value());
            }
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "unsupported tool option; expected name = \"...\"",
                ));
            }
        }
    }
    Ok(name.unwrap_or_else(|| rust_name(ident)))
}

fn parse_return(output: &ReturnType) -> syn::Result<ImplReturn> {
    let ReturnType::Type(_, ty) = output else {
        return Ok(ImplReturn {
            value: parse_quote!(()),
            error: None,
        });
    };
    let peeled = peel_type(ty);
    let result_segment = match peeled {
        Type::Path(path) if path.qself.is_none() => path
            .path
            .segments
            .last()
            .filter(|segment| segment.ident == "Result"),
        _ => None,
    };
    let Some(segment) = result_segment else {
        return Ok(ImplReturn {
            value: ty.as_ref().clone(),
            error: None,
        });
    };
    let PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return Err(syn::Error::new_spanned(
            ty,
            "Result must have value and error types",
        ));
    };
    let mut arguments = arguments.args.iter();
    let Some(GenericArgument::Type(value)) = arguments.next() else {
        return Err(syn::Error::new_spanned(
            ty,
            "Result must have value and error types",
        ));
    };
    let Some(GenericArgument::Type(error)) = arguments.next() else {
        return Err(syn::Error::new_spanned(
            ty,
            "Result must have value and error types",
        ));
    };
    if arguments.next().is_some() {
        return Err(syn::Error::new_spanned(
            ty,
            "Result must have exactly two type arguments",
        ));
    }
    Ok(ImplReturn {
        value: value.clone(),
        error: Some(error.clone()),
    })
}

fn docs(attributes: &[Attribute]) -> String {
    attributes
        .iter()
        .filter_map(|attribute| {
            let Meta::NameValue(meta) = &attribute.meta else {
                return None;
            };
            if !meta.path.is_ident("doc") {
                return None;
            }
            let Expr::Lit(expr) = &meta.value else {
                return None;
            };
            let Lit::Str(text) = &expr.lit else {
                return None;
            };
            Some(text.value().trim().to_owned())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn validate_dotted_name(value: &str, span: impl quote::ToTokens, kind: &str) -> syn::Result<()> {
    if value.is_empty()
        || value.starts_with('.')
        || value.ends_with('.')
        || value.split('.').any(|segment| {
            segment.is_empty()
                || segment.starts_with("r#")
                || Ident::parse_any.parse_str(segment).is_err()
        })
    {
        return Err(syn::Error::new_spanned(
            span,
            format!("{kind} must be a dot-separated identifier path"),
        ));
    }
    Ok(())
}

fn peel_type(mut ty: &Type) -> &Type {
    loop {
        ty = match ty {
            Type::Group(group) => &group.elem,
            Type::Paren(paren) => &paren.elem,
            _ => return ty,
        };
    }
}

fn type_contains_reference(ty: &Type) -> bool {
    quote!(#ty).to_string().contains('&')
}

fn rust_name(ident: &Ident) -> String {
    let name = ident.to_string();
    name.strip_prefix("r#").unwrap_or(&name).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stateful_self_type_is_unqualified_and_non_generic() {
        let plain: Type = parse_quote!(Host);
        assert_eq!(concrete_type_ident(&plain).unwrap(), "Host");

        let generic: Type = parse_quote!(Host<u8>);
        assert!(concrete_type_ident(&generic).is_err());

        let qualified: Type = parse_quote!(ui::Host);
        assert!(concrete_type_ident(&qualified).is_err());
    }
}
