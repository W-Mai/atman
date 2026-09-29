use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{
    Attribute, Expr, FnArg, GenericArgument, Ident, ImplItem, ImplItemFn, Item, ItemFn, ItemImpl,
    ItemMod, Lit, Meta, MetaList, Pat, Path, PathArguments, ReturnType, Token, Type, ext::IdentExt,
    parse::Parser, parse_quote, punctuated::Punctuated, visit::Visit,
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
        if attributes_introduce(&function.attrs, "tool")? {
            return Err(conditional_tool_error(
                &function.sig.ident,
                "function",
                "module",
            ));
        }
        let Some(tool_attr_index) = function
            .attrs
            .iter()
            .position(|attr| attr.path().is_ident("tool"))
        else {
            continue;
        };
        reject_conditional_tool(&function.attrs, &function.sig.ident, "function", "module")?;
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
        validate_generated_binding_type(&input.ty, "tool parameter")?;
        params.push(ModuleParam {
            ident: pattern.ident.clone(),
            ty: input.ty.as_ref().clone(),
        });
    }

    validate_return_type(&function.sig.output, "tool return type")?;
    let result = parse_return(&function.sig.output)?;
    if type_contains_reference(&result.value) {
        return Err(syn::Error::new_spanned(
            &result.value,
            "tool functions cannot return references",
        ));
    }

    Ok(ModuleTool {
        ident: function.sig.ident.clone(),
        name: parse_tool_name(&function.sig.ident, attr, DottedNameStart::Root)?,
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
        if attributes_introduce(&method.attrs, "tool")? {
            return Err(conditional_tool_error(&method.sig.ident, "method", "impl"));
        }
        let Some(tool_attr_index) = method
            .attrs
            .iter()
            .position(|attr| attr.path().is_ident("tool"))
        else {
            continue;
        };
        reject_conditional_tool(&method.attrs, &method.sig.ident, "method", "impl")?;
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
                validate_dotted_name(
                    literal.value().as_str(),
                    literal,
                    "namespace",
                    DottedNameStart::Root,
                )?;
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
    let leaf_name = parse_tool_name(&method.sig.ident, attr, DottedNameStart::Member)?;
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
        validate_generated_binding_type(&input.ty, "tool parameter")?;
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

    validate_return_type(&method.sig.output, "tool return type")?;
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

fn parse_tool_name(ident: &Ident, attr: &Attribute, start: DottedNameStart) -> syn::Result<String> {
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
                validate_dotted_name(literal.value().as_str(), literal, "tool name", start)?;
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
    if let Some(name) = name {
        return Ok(name);
    }
    let name = rust_name(ident);
    validate_dotted_name(&name, ident, "tool name", start)?;
    Ok(name)
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

#[derive(Clone, Copy)]
enum DottedNameStart {
    Root,
    Member,
}

fn validate_dotted_name(
    value: &str,
    span: impl quote::ToTokens,
    kind: &str,
    start: DottedNameStart,
) -> syn::Result<()> {
    let mut segments = value.split('.');
    let first = segments.next().unwrap_or_default();
    if first.is_empty()
        || first.starts_with("r#")
        || match start {
            DottedNameStart::Root => {
                syn::parse_str::<Ident>(first).is_err() || is_reserved_atman_tool_root(first)
            }
            DottedNameStart::Member => Ident::parse_any.parse_str(first).is_err(),
        }
    {
        return Err(syn::Error::new_spanned(
            span,
            format!("{kind} must start with an identifier accepted as an Atman tool call path"),
        ));
    }
    if segments.any(|segment| {
        segment.is_empty()
            || segment.starts_with("r#")
            || Ident::parse_any.parse_str(segment).is_err()
    }) {
        return Err(syn::Error::new_spanned(
            span,
            format!("{kind} must be a dot-separated identifier path"),
        ));
    }
    Ok(())
}

fn is_reserved_atman_tool_root(value: &str) -> bool {
    matches!(
        value,
        "when"
            | "watch"
            | "fanout"
            | "user_confirm"
            | "user_msg"
            | "assistant_msg"
            | "system_msg"
            | "tool_result"
            | "fix_until_test_passes"
    )
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

fn reject_conditional_tool(
    attributes: &[Attribute],
    ident: &Ident,
    kind: &str,
    container: &str,
) -> syn::Result<()> {
    if attributes
        .iter()
        .any(|attribute| attribute.path().is_ident("cfg"))
        || attributes_introduce(attributes, "cfg")?
    {
        return Err(conditional_tool_error(ident, kind, container));
    }
    Ok(())
}

fn conditional_tool_error(ident: &Ident, kind: &str, container: &str) -> syn::Error {
    syn::Error::new_spanned(
        ident,
        format!(
            "conditional #[tool] {kind}s are not supported; place the condition on the containing #[tools] {container}"
        ),
    )
}

fn attributes_introduce(attributes: &[Attribute], target: &str) -> syn::Result<bool> {
    for attribute in attributes {
        if !attribute.path().is_ident("cfg_attr") {
            continue;
        }
        let Meta::List(list) = &attribute.meta else {
            return Err(syn::Error::new_spanned(
                attribute,
                "cfg_attr must contain a predicate and at least one attribute",
            ));
        };
        if cfg_attr_introduces(list, target)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn cfg_attr_introduces(list: &MetaList, target: &str) -> syn::Result<bool> {
    let arguments = Punctuated::<Meta, Token![,]>::parse_terminated.parse2(list.tokens.clone())?;
    if arguments.len() < 2 {
        return Err(syn::Error::new_spanned(
            list,
            "cfg_attr must contain a predicate and at least one attribute",
        ));
    }
    for attribute in arguments.iter().skip(1) {
        if attribute.path().is_ident(target) {
            return Ok(true);
        }
        if attribute.path().is_ident("cfg_attr") {
            let Meta::List(nested) = attribute else {
                return Err(syn::Error::new_spanned(
                    attribute,
                    "nested cfg_attr must contain a predicate and at least one attribute",
                ));
            };
            if cfg_attr_introduces(nested, target)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn validate_return_type(output: &ReturnType, kind: &str) -> syn::Result<()> {
    if let ReturnType::Type(_, ty) = output {
        validate_generated_binding_type(ty, kind)?;
    }
    Ok(())
}

fn validate_generated_binding_type(ty: &Type, kind: &str) -> syn::Result<()> {
    let mut visitor = GeneratedBindingTypeVisitor::default();
    visitor.visit_type(ty);
    if visitor.contains_self {
        return Err(syn::Error::new_spanned(
            ty,
            format!(
                "{kind} cannot contain `Self`; use the concrete type name in generated tool bindings"
            ),
        ));
    }
    if visitor.contains_impl_trait {
        return Err(syn::Error::new_spanned(
            ty,
            format!(
                "{kind} cannot contain `impl Trait`; use a concrete type in generated tool bindings"
            ),
        ));
    }
    Ok(())
}

#[derive(Default)]
struct GeneratedBindingTypeVisitor {
    contains_self: bool,
    contains_impl_trait: bool,
}

impl<'ast> Visit<'ast> for GeneratedBindingTypeVisitor {
    fn visit_path(&mut self, path: &'ast Path) {
        if path.segments.iter().any(|segment| segment.ident == "Self") {
            self.contains_self = true;
        }
        syn::visit::visit_path(self, path);
    }

    fn visit_type_impl_trait(&mut self, ty: &'ast syn::TypeImplTrait) {
        self.contains_impl_trait = true;
        syn::visit::visit_type_impl_trait(self, ty);
    }
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

    #[test]
    fn generated_bindings_reject_conditional_tools() {
        let module: ItemMod = parse_quote! {
            mod demo {
                #[cfg(feature = "demo")]
                #[tool]
                fn ping() {}
            }
        };
        let error = match parse_module(TokenStream::new(), module) {
            Ok(_) => panic!("conditional module tool must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("conditional #[tool] functions"));

        let unrelated_cfg_attr: ItemImpl = parse_quote! {
            impl Host {
                #[cfg_attr(feature = "demo", allow(dead_code))]
                #[tool]
                fn ping(&self) {}
            }
        };
        assert_eq!(
            parse_impl(quote!(namespace = "demo"), unrelated_cfg_attr)
                .unwrap()
                .tools
                .len(),
            1
        );

        let conditional_attr: ItemImpl = parse_quote! {
            impl Host {
                #[cfg_attr(feature = "demo", tool)]
                fn ping(&self) {}
            }
        };
        let error = match parse_impl(quote!(namespace = "demo"), conditional_attr) {
            Ok(_) => panic!("conditional impl tool must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("conditional #[tool] methods"));

        let conditional_cfg: ItemImpl = parse_quote! {
            impl Host {
                #[cfg_attr(feature = "demo", cfg(target_os = "none"))]
                #[tool]
                fn ping(&self) {}
            }
        };
        let error = match parse_impl(quote!(namespace = "demo"), conditional_cfg) {
            Ok(_) => panic!("conditionally removed impl tool must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("conditional #[tool] methods"));
    }

    #[test]
    fn generated_bindings_reject_self_and_impl_trait_types() {
        let self_type: ItemImpl = parse_quote! {
            impl Host {
                #[tool]
                fn echo(&self, value: Self) -> Self {
                    value
                }
            }
        };
        let error = match parse_impl(quote!(namespace = "demo"), self_type) {
            Ok(_) => panic!("Self in a generated signature must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("cannot contain `Self`"));

        let opaque_input: ItemMod = parse_quote! {
            mod demo {
                #[tool]
                fn display(value: impl core::fmt::Display) -> String {
                    value.to_string()
                }
            }
        };
        let error = match parse_module(TokenStream::new(), opaque_input) {
            Ok(_) => panic!("impl Trait input must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("cannot contain `impl Trait`"));

        let opaque_output: ItemImpl = parse_quote! {
            impl Host {
                #[tool]
                fn display(&self) -> impl core::fmt::Display {
                    "value"
                }
            }
        };
        let error = match parse_impl(quote!(namespace = "demo"), opaque_output) {
            Ok(_) => panic!("impl Trait output must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("cannot contain `impl Trait`"));

        let macro_type: Type = parse_quote!(concrete_type!(Self, impl));
        assert!(validate_generated_binding_type(&macro_type, "tool parameter").is_ok());
    }

    #[test]
    fn generated_tool_paths_reject_keyword_roots_and_accept_keyword_members() {
        for reserved in ["loop", "when", "fanout"] {
            let keyword_namespace: ItemImpl = parse_quote! {
                impl Host {
                    #[tool]
                    fn ping(&self) {}
                }
            };
            let error = match parse_impl(quote!(namespace = #reserved), keyword_namespace) {
                Ok(_) => panic!("reserved namespace root must be rejected"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("Atman tool call path"));
        }

        let expression_name: ItemImpl = parse_quote! {
            impl Host {
                #[tool]
                fn ping(&self) {}
            }
        };
        assert_eq!(
            parse_impl(quote!(namespace = "flow"), expression_name)
                .unwrap()
                .namespace,
            "flow"
        );

        let keyword_member: ItemImpl = parse_quote! {
            impl Host {
                #[tool(name = "fanout.loop")]
                fn ping(&self) {}
            }
        };
        let parsed = parse_impl(quote!(namespace = "gfx.loop"), keyword_member).unwrap();
        assert_eq!(parsed.namespace, "gfx.loop");
        assert_eq!(parsed.tools[0].leaf_name, "fanout.loop");

        let keyword_module_tool: ItemMod = parse_quote! {
            mod demo {
                #[tool]
                fn r#fanout() {}
            }
        };
        let error = match parse_module(TokenStream::new(), keyword_module_tool) {
            Ok(_) => panic!("module tool keyword root must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("Atman tool call path"));
    }
}
