use proc_macro2::TokenStream;
use syn::{
    Attribute, Expr, FnArg, GenericArgument, Ident, Item, ItemFn, ItemMod, Lit, Meta, Pat, Path,
    PathArguments, ReturnType, Token, Type, parse::Parser, parse_quote, punctuated::Punctuated,
    spanned::Spanned,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Rt,
    Runtime,
}

pub(crate) struct ParsedModule {
    pub item: ItemMod,
    pub tools: Vec<ToolFn>,
    pub crate_path: Path,
}

pub(crate) struct ToolFn {
    pub ident: Ident,
    pub name: String,
    pub params: Vec<ToolParam>,
    pub result: ReturnSpec,
    pub is_async: bool,
    pub docs: String,
    pub tier: Option<u8>,
    pub cancel: Option<CancelKind>,
}

pub(crate) struct ToolParam {
    pub ident: Ident,
    pub ty: ValueType,
}

pub(crate) struct ReturnSpec {
    pub value: ValueType,
    pub error: Option<Type>,
}

#[derive(Clone)]
pub(crate) enum ValueType {
    Unit,
    Int,
    Float,
    Bool,
    String,
    Option(Box<Self>),
    Vec(Box<Self>),
    ToolCtx,
}

#[derive(Clone, Copy)]
pub(crate) enum CancelKind {
    AbortSafe,
    Revertible,
    Atomic,
    Irreversible,
}

pub(crate) fn parse_module(
    attr: TokenStream,
    item: TokenStream,
    mode: Mode,
) -> syn::Result<ParsedModule> {
    let crate_path = parse_module_options(attr, mode)?;
    let mut item: ItemMod = syn::parse2(item)?;
    if item.content.is_none() {
        return Err(syn::Error::new_spanned(
            &item.ident,
            "tools requires an inline module",
        ));
    }
    let (_, items) = item.content.as_mut().expect("checked inline module");
    let generated_name = match mode {
        Mode::Rt => "router",
        Mode::Runtime => "register",
    };
    if let Some(existing) = items
        .iter()
        .find(|item| matches!(item, Item::Fn(function) if function.sig.ident == generated_name))
    {
        return Err(syn::Error::new_spanned(
            existing,
            format!("tools generates `{generated_name}`; rename the existing function"),
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
        let attr = function.attrs.remove(tool_attr_index);
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
        tools.push(parse_tool(function, &attr, mode)?);
    }
    Ok(ParsedModule {
        item,
        tools,
        crate_path,
    })
}

fn parse_module_options(attr: TokenStream, mode: Mode) -> syn::Result<Path> {
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
                    "unsupported tools option; expected crate_path = ::path",
                ));
            }
        }
    }
    Ok(crate_path.unwrap_or_else(|| match mode {
        Mode::Rt => parse_quote!(::atman_rt),
        Mode::Runtime => parse_quote!(::atman_runtime),
    }))
}

fn parse_tool(function: &ItemFn, attr: &Attribute, mode: Mode) -> syn::Result<ToolFn> {
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
    let mut tier = None;
    let mut cancel = None;
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
                name = Some(literal.value());
            }
            Meta::NameValue(value) if value.path.is_ident("tier") && mode == Mode::Runtime => {
                if tier.is_some() {
                    return Err(syn::Error::new_spanned(value, "duplicate tool tier"));
                }
                let Expr::Lit(expr) = &value.value else {
                    return Err(syn::Error::new_spanned(
                        &value.value,
                        "tier must be 0, 1, 2, 3, or 4",
                    ));
                };
                let Lit::Int(literal) = &expr.lit else {
                    return Err(syn::Error::new_spanned(
                        &expr.lit,
                        "tier must be 0, 1, 2, 3, or 4",
                    ));
                };
                let value = literal.base10_parse::<u8>()?;
                if value > 4 {
                    return Err(syn::Error::new_spanned(
                        literal,
                        "tier must be 0, 1, 2, 3, or 4",
                    ));
                }
                tier = Some(value);
            }
            Meta::NameValue(value) if value.path.is_ident("cancel") && mode == Mode::Runtime => {
                if cancel.is_some() {
                    return Err(syn::Error::new_spanned(value, "duplicate cancel behavior"));
                }
                let Expr::Lit(expr) = &value.value else {
                    return Err(syn::Error::new_spanned(
                        &value.value,
                        "cancel must be a string",
                    ));
                };
                let Lit::Str(literal) = &expr.lit else {
                    return Err(syn::Error::new_spanned(
                        &expr.lit,
                        "cancel must be a string",
                    ));
                };
                cancel = Some(match literal.value().as_str() {
                    "abort_safe" => CancelKind::AbortSafe,
                    "revertible" => CancelKind::Revertible,
                    "atomic" => CancelKind::Atomic,
                    "irreversible" => CancelKind::Irreversible,
                    _ => {
                        return Err(syn::Error::new_spanned(
                            literal,
                            "cancel must be abort_safe, revertible, atomic, or irreversible",
                        ));
                    }
                });
            }
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    match mode {
                        Mode::Rt => "unsupported tool option; expected name = \"...\"",
                        Mode::Runtime => "unsupported tool option; expected name, tier, or cancel",
                    },
                ));
            }
        }
    }

    if mode == Mode::Runtime {
        let selected_tier = tier.ok_or_else(|| {
            syn::Error::new_spanned(
                &function.sig.ident,
                "product tools require #[tool(tier = 0..4)]",
            )
        })?;
        if selected_tier > 0 && cancel.is_none() {
            return Err(syn::Error::new_spanned(
                &function.sig.ident,
                "tier 1–4 tools require cancel = \"abort_safe|revertible|atomic|irreversible\"",
            ));
        }
        if selected_tier == 0 && cancel.is_none() {
            cancel = Some(CancelKind::AbortSafe);
        }
    }

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
        let ty = parse_value_type(&input.ty, mode == Mode::Runtime)?;
        if matches!(ty, ValueType::ToolCtx)
            && (mode == Mode::Rt || params.len() + 1 != function.sig.inputs.len())
        {
            return Err(syn::Error::new_spanned(
                &input.ty,
                "ToolCtx is only supported as the final product tool parameter",
            ));
        }
        params.push(ToolParam {
            ident: pattern.ident.clone(),
            ty,
        });
    }

    let result = parse_return_type(&function.sig.output)?;
    let docs = function
        .attrs
        .iter()
        .filter_map(|attr| {
            let Meta::NameValue(meta) = &attr.meta else {
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
        .join("\n");
    Ok(ToolFn {
        ident: function.sig.ident.clone(),
        name: name.unwrap_or_else(|| {
            function
                .sig
                .ident
                .to_string()
                .trim_start_matches("r#")
                .to_owned()
        }),
        params,
        result,
        is_async: function.sig.asyncness.is_some(),
        docs,
        tier,
        cancel,
    })
}

fn parse_return_type(output: &ReturnType) -> syn::Result<ReturnSpec> {
    let ReturnType::Type(_, ty) = output else {
        return Ok(ReturnSpec {
            value: ValueType::Unit,
            error: None,
        });
    };
    let result_segment = match ty.as_ref() {
        Type::Path(path) if path.qself.is_none() => path
            .path
            .segments
            .last()
            .filter(|segment| segment.ident == "Result"),
        _ => None,
    };
    if let Some(segment) = result_segment {
        let PathArguments::AngleBracketed(args) = &segment.arguments else {
            return Err(syn::Error::new_spanned(
                ty,
                "Result must have value and error types",
            ));
        };
        let mut args = args.args.iter();
        let Some(GenericArgument::Type(value)) = args.next() else {
            return Err(syn::Error::new_spanned(
                ty,
                "Result must have value and error types",
            ));
        };
        let Some(GenericArgument::Type(error)) = args.next() else {
            return Err(syn::Error::new_spanned(
                ty,
                "Result must have value and error types",
            ));
        };
        if args.next().is_some() {
            return Err(syn::Error::new_spanned(
                ty,
                "Result must have exactly two type arguments",
            ));
        }
        return Ok(ReturnSpec {
            value: parse_value_type(value, false)?,
            error: Some(error.clone()),
        });
    }
    Ok(ReturnSpec {
        value: parse_value_type(ty, false)?,
        error: None,
    })
}

fn parse_value_type(ty: &Type, allow_tool_ctx: bool) -> syn::Result<ValueType> {
    parse_value_type_inner(ty, allow_tool_ctx, false)
}

fn parse_value_type_inner(
    ty: &Type,
    allow_tool_ctx: bool,
    inside_container: bool,
) -> syn::Result<ValueType> {
    if let Type::Tuple(tuple) = ty {
        if tuple.elems.is_empty() {
            return Ok(ValueType::Unit);
        }
    }
    let Type::Path(path) = ty else {
        return Err(unsupported_type(ty));
    };
    if path.qself.is_some() {
        return Err(unsupported_type(ty));
    }
    let Some(segment) = path.path.segments.last() else {
        return Err(unsupported_type(ty));
    };
    match segment.ident.to_string().as_str() {
        "i64" if matches!(segment.arguments, PathArguments::None) => Ok(ValueType::Int),
        "f64" if matches!(segment.arguments, PathArguments::None) => Ok(ValueType::Float),
        "bool" if matches!(segment.arguments, PathArguments::None) => Ok(ValueType::Bool),
        "String" if matches!(segment.arguments, PathArguments::None) => Ok(ValueType::String),
        "ToolCtx" if allow_tool_ctx && matches!(segment.arguments, PathArguments::None) => {
            Ok(ValueType::ToolCtx)
        }
        "Option" | "Vec" => {
            if inside_container {
                return Err(syn::Error::new_spanned(
                    ty,
                    "nested Option<T> and Vec<T> are not supported; wrap a basic type once",
                ));
            }
            let PathArguments::AngleBracketed(args) = &segment.arguments else {
                return Err(unsupported_type(ty));
            };
            let mut args = args.args.iter();
            let Some(GenericArgument::Type(inner)) = args.next() else {
                return Err(unsupported_type(ty));
            };
            if args.next().is_some() {
                return Err(unsupported_type(ty));
            }
            let inner = parse_value_type_inner(inner, false, true)?;
            if segment.ident == "Option" {
                if matches!(inner, ValueType::Unit) {
                    return Err(syn::Error::new_spanned(
                        ty,
                        "Option<()> cannot be represented without ambiguity; use bool instead",
                    ));
                }
                Ok(ValueType::Option(Box::new(inner)))
            } else {
                Ok(ValueType::Vec(Box::new(inner)))
            }
        }
        _ => Err(unsupported_type(ty)),
    }
}

fn unsupported_type(ty: &Type) -> syn::Error {
    syn::Error::new(
        ty.span(),
        "unsupported tool type; use i64, f64, bool, String, (), Option<T>, or Vec<T>",
    )
}
