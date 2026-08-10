use atman_dsl::ast::Expr;

use crate::error::RuntimeError;
use crate::eval::ContextMode;
use crate::tool::ToolArgs;
use crate::value::Value;

pub struct LlmNodeArgs {
    pub model: Option<String>,
    pub prompt: Option<String>,
    pub messages_override: Option<Vec<crate::message::Message>>,
    pub system: Option<String>,
    pub input: Value,
    pub retry_count: u32,
    pub retry_kinds: Option<std::collections::HashSet<crate::error::ErrorKind>>,
    pub cache_prompt: bool,
    pub context_budget: Option<u64>,
    pub context_mode: String,
    pub fallback_expr: Option<Expr>,
    pub tool_specs: Vec<crate::tool::ToolSpec>,
    pub stall_timeout_secs: u64,
}

pub async fn parse_llm_args<'a>(
    kwargs: &'a [(atman_dsl::ast::Ident, Expr)],
    env: &'a crate::env::Env,
    ctx: &'a crate::eval::EvalCtx<'a>,
) -> Result<LlmNodeArgs, Value> {
    let _context_mode_type_marker: Option<ContextMode> = None;
    let mut model: Option<String> = None;
    let mut prompt: Option<String> = None;
    let mut messages_override: Option<Vec<crate::message::Message>> = None;
    let mut system: Option<String> = None;
    let mut input: Value = Value::Unit;
    let mut retry_count: u32 = 0;
    let mut retry_kinds: Option<std::collections::HashSet<crate::error::ErrorKind>> = None;
    let mut cache_prompt = false;
    let mut context_budget: Option<u64> = None;
    let mut context_mode = String::from("none");
    let mut fallback_expr: Option<Expr> = None;
    let mut tool_specs: Vec<crate::tool::ToolSpec> = Vec::new();
    let mut stall_timeout_secs: u64 = 120;
    for (k, v) in kwargs {
        match k.name.as_str() {
            "schema" => continue,
            "fallback" => {
                fallback_expr = Some(v.clone());
                continue;
            }
            "retry_classified" => {
                let idents = match super::parse_error_kind_list(v) {
                    Ok(k) => k,
                    Err(msg) => return Err(Value::Err(RuntimeError::ToolFailed(msg))),
                };
                retry_kinds = Some(idents);
                continue;
            }
            "tools" => {
                match super::resolve_tool_specs(v, ctx.tools) {
                    Ok(specs) => tool_specs = specs,
                    Err(msg) => return Err(Value::Err(RuntimeError::ToolFailed(msg))),
                }
                continue;
            }
            "context" => {
                context_mode = match v {
                    atman_dsl::ast::Expr::Ident(id) => id.name.clone(),
                    atman_dsl::ast::Expr::Member { base, field } => {
                        let mut full = String::new();
                        if let atman_dsl::ast::Expr::Ident(id) = base.as_ref() {
                            full.push_str(&id.name);
                        }
                        full.push('.');
                        full.push_str(&field.name);
                        full
                    }
                    other => {
                        return Err(Value::Err(RuntimeError::ToolFailed(format!(
                            "llm.context: expected ident like `session` or `none`, got {}",
                            super::expr_shape(other)
                        ))));
                    }
                };
                continue;
            }
            _ => {}
        }
        let val = super::eval_expr(v, env, ctx).await;
        if val.is_err() {
            return Err(val);
        }
        match k.name.as_str() {
            "model" => match val {
                Value::Str(s) => model = Some(crate::model_registry::resolve_alias(&s)),
                other => {
                    return Err(Value::Err(RuntimeError::TypeMismatch {
                        expected: "string".into(),
                        actual: other.kind_name().into(),
                    }));
                }
            },
            "prompt" => match val {
                Value::Str(s) => prompt = Some(s),
                other => {
                    return Err(Value::Err(RuntimeError::TypeMismatch {
                        expected: "string or @\"path\"".into(),
                        actual: other.kind_name().into(),
                    }));
                }
            },
            "messages" => match val {
                Value::List(items) => {
                    let mut msgs = Vec::with_capacity(items.len());
                    for item in items {
                        match item {
                            Value::Message(m) => msgs.push(m),
                            other => {
                                return Err(Value::Err(RuntimeError::TypeMismatch {
                                    expected: "message".into(),
                                    actual: other.kind_name().into(),
                                }));
                            }
                        }
                    }
                    messages_override = Some(msgs);
                }
                other => {
                    return Err(Value::Err(RuntimeError::TypeMismatch {
                        expected: "list of message".into(),
                        actual: other.kind_name().into(),
                    }));
                }
            },
            "system" => match val {
                Value::Str(s) => system = Some(s),
                other => {
                    return Err(Value::Err(RuntimeError::TypeMismatch {
                        expected: "string (system prompt)".into(),
                        actual: other.kind_name().into(),
                    }));
                }
            },
            "input" => input = val,
            "retry" => match val {
                Value::Int(n) if n >= 0 => retry_count = n as u32,
                other => {
                    return Err(Value::Err(RuntimeError::TypeMismatch {
                        expected: "non-negative int".into(),
                        actual: other.kind_name().into(),
                    }));
                }
            },
            "cache" => match val {
                Value::Bool(b) => cache_prompt = b,
                other => {
                    return Err(Value::Err(RuntimeError::TypeMismatch {
                        expected: "bool".into(),
                        actual: other.kind_name().into(),
                    }));
                }
            },
            "context_budget" => match val {
                Value::Int(n) if n > 0 => context_budget = Some(n as u64),
                other => {
                    return Err(Value::Err(RuntimeError::TypeMismatch {
                        expected: "positive int".into(),
                        actual: other.kind_name().into(),
                    }));
                }
            },
            "stall_timeout" => match val {
                Value::Int(n) if n >= 0 => stall_timeout_secs = n as u64,
                other => {
                    return Err(Value::Err(RuntimeError::TypeMismatch {
                        expected: "non-negative int (seconds)".into(),
                        actual: other.kind_name().into(),
                    }));
                }
            },
            _ => {}
        }
    }

    Ok(LlmNodeArgs {
        model,
        prompt,
        messages_override,
        system,
        input,
        retry_count,
        retry_kinds,
        cache_prompt,
        context_budget,
        context_mode,
        fallback_expr,
        tool_specs,
        stall_timeout_secs,
    })
}

pub fn parse_llm_args_from_toolargs(
    args: &ToolArgs,
    tools: &crate::tool::ToolRegistry,
) -> Result<LlmNodeArgs, RuntimeError> {
    let mut model: Option<String> = None;
    let mut prompt: Option<String> = None;
    let mut messages_override: Option<Vec<crate::message::Message>> = None;
    let mut system: Option<String> = None;
    let mut input: Value = Value::Unit;
    let mut retry_count: u32 = 0;
    let mut retry_kinds: Option<std::collections::HashSet<crate::error::ErrorKind>> = None;
    let mut cache_prompt = false;
    let mut context_budget: Option<u64> = None;
    let mut context_mode = String::from("none");
    let mut tool_specs: Vec<crate::tool::ToolSpec> = Vec::new();
    let mut stall_timeout_secs: u64 = 120;
    for (k, v) in &args.named {
        match k.as_str() {
            "retry_classified" => {
                let items = match v {
                    Value::List(items) => items,
                    other => {
                        return Err(RuntimeError::TypeMismatch {
                            expected: "list of strings".into(),
                            actual: other.kind_name().into(),
                        });
                    }
                };
                let mut kinds = std::collections::HashSet::new();
                for item in items {
                    let name = match item {
                        Value::Str(s) => s.clone(),
                        other => {
                            return Err(RuntimeError::TypeMismatch {
                                expected: "string kind name".into(),
                                actual: other.kind_name().into(),
                            });
                        }
                    };
                    match crate::error::ErrorKind::from_name(&name) {
                        Some(k) => {
                            kinds.insert(k);
                        }
                        None => {
                            return Err(RuntimeError::ToolFailed(format!(
                                "retry_classified: unknown error kind `{name}`"
                            )));
                        }
                    }
                }
                retry_kinds = Some(kinds);
                continue;
            }
            "tools" => {
                let items = match v {
                    Value::List(items) => items,
                    other => {
                        return Err(RuntimeError::TypeMismatch {
                            expected: "list of tool references".into(),
                            actual: other.kind_name().into(),
                        });
                    }
                };
                match resolve_tool_specs_from_values(items, tools) {
                    Ok(specs) => tool_specs = specs,
                    Err(msg) => return Err(RuntimeError::ToolFailed(msg)),
                }
                continue;
            }
            "context" => {
                context_mode = match v {
                    Value::Str(s) => s.clone(),
                    other => {
                        return Err(RuntimeError::ToolFailed(format!(
                            "llm.context: expected string like `session` or `none`, got {}",
                            other.kind_name()
                        )));
                    }
                };
                continue;
            }
            _ => {}
        }
        match k.as_str() {
            "model" => match v {
                Value::Str(s) => model = Some(crate::model_registry::resolve_alias(s)),
                other => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "string".into(),
                        actual: other.kind_name().into(),
                    });
                }
            },
            "prompt" => match v {
                Value::Str(s) => prompt = Some(s.clone()),
                other => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "string".into(),
                        actual: other.kind_name().into(),
                    });
                }
            },
            "messages" => match v {
                Value::List(items) => {
                    let mut msgs = Vec::with_capacity(items.len());
                    for item in items {
                        match item {
                            Value::Message(m) => msgs.push(m.clone()),
                            other => {
                                return Err(RuntimeError::TypeMismatch {
                                    expected: "message".into(),
                                    actual: other.kind_name().into(),
                                });
                            }
                        }
                    }
                    messages_override = Some(msgs);
                }
                other => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "list of message".into(),
                        actual: other.kind_name().into(),
                    });
                }
            },
            "system" => match v {
                Value::Str(s) => system = Some(s.clone()),
                other => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "string (system prompt)".into(),
                        actual: other.kind_name().into(),
                    });
                }
            },
            "input" => input = v.clone(),
            "retry" => match v {
                Value::Int(n) if *n >= 0 => retry_count = *n as u32,
                other => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "non-negative int".into(),
                        actual: other.kind_name().into(),
                    });
                }
            },
            "cache" => match v {
                Value::Bool(b) => cache_prompt = *b,
                other => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "bool".into(),
                        actual: other.kind_name().into(),
                    });
                }
            },
            "context_budget" => match v {
                Value::Int(n) if *n > 0 => context_budget = Some(*n as u64),
                other => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "positive int".into(),
                        actual: other.kind_name().into(),
                    });
                }
            },
            "stall_timeout" => match v {
                Value::Int(n) if *n >= 0 => stall_timeout_secs = *n as u64,
                other => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "non-negative int (seconds)".into(),
                        actual: other.kind_name().into(),
                    });
                }
            },
            _ => {}
        }
    }

    Ok(LlmNodeArgs {
        model,
        prompt,
        messages_override,
        system,
        input,
        retry_count,
        retry_kinds,
        cache_prompt,
        context_budget,
        context_mode,
        fallback_expr: None,
        tool_specs,
        stall_timeout_secs,
    })
}

pub fn resolve_tool_specs_from_values(
    values: &[Value],
    tools: &crate::tool::ToolRegistry,
) -> Result<Vec<crate::tool::ToolSpec>, String> {
    let mut out = Vec::with_capacity(values.len());
    for item in values {
        match item {
            Value::Str(name) => {
                if let Some(prefix) = wildcard_prefix_value(name) {
                    for tool_name in tools.names() {
                        if tool_name.starts_with(&prefix) {
                            if let Some(tool) = tools.get(&tool_name) {
                                out.push(crate::tool::tool_spec(tool.as_ref()));
                            }
                        }
                    }
                    continue;
                }
                let tool = tools
                    .get(name)
                    .ok_or_else(|| format!("llm.tools: unknown tool `{name}`"))?;
                out.push(crate::tool::tool_spec(tool.as_ref()));
            }
            other => {
                return Err(format!(
                    "llm.tools: item is not a tool reference (want a string name or \"ns.*\" wildcard), got {:?}",
                    other.kind_name()
                ));
            }
        }
    }
    Ok(out)
}

/// If the value is a string ending with `.*`, return the prefix
/// (everything before the `.*`), e.g. `"mcp.*"` → `Some("mcp.")`.
fn wildcard_prefix_value(s: &str) -> Option<String> {
    s.strip_suffix(".*").map(|prefix| {
        if prefix.is_empty() {
            String::new()
        } else {
            format!("{prefix}.")
        }
    })
}
