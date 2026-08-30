use crate::error::RuntimeError;
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
    pub fallback_value: Option<crate::value::Value>,
    pub tool_specs: Vec<crate::tool::ToolSpec>,
    pub reasoning: Option<crate::provider::ReasoningSelection>,
    pub stall_timeout_secs: u64,
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
    let mut reasoning: Option<crate::provider::ReasoningSelection> = None;
    let mut reasoning_arg: Option<&'static str> = None;
    let mut legacy_thinking: Option<bool> = None;
    let mut stall_timeout_secs: u64 = 120;
    let mut fallback_value: Option<crate::value::Value> = None;
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
            "reasoning" => match v {
                Value::Str(value) => {
                    set_reasoning_arg(
                        &mut reasoning,
                        &mut reasoning_arg,
                        "reasoning",
                        value.parse().map_err(|error: String| {
                            RuntimeError::ToolFailed(format!("llm.reasoning: {error}"))
                        })?,
                    )?;
                }
                other => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "string (default, off, auto, effort, effort@mode, or budget:N)"
                            .into(),
                        actual: other.kind_name().into(),
                    });
                }
            },
            "effort" => match v {
                Value::Unit => {}
                Value::Str(value) => {
                    set_reasoning_arg(
                        &mut reasoning,
                        &mut reasoning_arg,
                        "effort",
                        value.parse().map_err(|error: String| {
                            RuntimeError::ToolFailed(format!("llm.effort: {error}"))
                        })?,
                    )?;
                }
                other => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "string (off, auto, effort, effort@mode, or budget:N), or unit"
                            .into(),
                        actual: other.kind_name().into(),
                    });
                }
            },
            "reasoning_budget" => match v {
                Value::Int(tokens) if *tokens > 0 => {
                    let tokens =
                        u32::try_from(*tokens).map_err(|_| RuntimeError::TypeMismatch {
                            expected: "positive int within u32 range (reasoning token budget)"
                                .into(),
                            actual: tokens.to_string(),
                        })?;
                    set_reasoning_arg(
                        &mut reasoning,
                        &mut reasoning_arg,
                        "reasoning_budget",
                        crate::provider::ReasoningSelection::BudgetTokens { tokens },
                    )?;
                }
                other => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "positive int (reasoning token budget)".into(),
                        actual: other.kind_name().into(),
                    });
                }
            },
            "thinking" => match v {
                Value::Bool(enabled) => legacy_thinking = Some(*enabled),
                other => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "bool (legacy thinking toggle)".into(),
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
            "fallback" => {
                fallback_value = Some(v.clone());
            }
            _ => {}
        }
    }

    if matches!(input, Value::Unit) {
        if let Ok(v) = args.positional(0) {
            input = v.clone();
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
        fallback_value,
        tool_specs,
        reasoning: reasoning.or_else(|| {
            legacy_thinking.map(|enabled| {
                if enabled {
                    crate::provider::ReasoningSelection::Auto {
                        execution_mode: None,
                    }
                } else {
                    crate::provider::ReasoningSelection::Disabled
                }
            })
        }),
        stall_timeout_secs,
    })
}

fn set_reasoning_arg(
    reasoning: &mut Option<crate::provider::ReasoningSelection>,
    source: &mut Option<&'static str>,
    next_source: &'static str,
    selection: crate::provider::ReasoningSelection,
) -> Result<(), RuntimeError> {
    if let Some(previous) = source {
        return Err(RuntimeError::ToolFailed(format!(
            "llm: `{previous}` and `{next_source}` cannot be used together"
        )));
    }
    *reasoning = Some(selection);
    *source = Some(next_source);
    Ok(())
}

pub fn resolve_tool_specs_from_values(
    values: &[Value],
    tools: &crate::tool::ToolRegistry,
) -> Result<Vec<crate::tool::ToolSpec>, String> {
    let mut out = Vec::with_capacity(values.len());
    let mut seen = std::collections::HashSet::new();
    for item in values {
        match item {
            Value::Str(name) => {
                if let Some(prefix) = wildcard_prefix_value(name) {
                    let mut matches: Vec<_> = tools
                        .names()
                        .into_iter()
                        .filter(|tool_name| tool_name.starts_with(&prefix))
                        .collect();
                    matches.sort_unstable();
                    for tool_name in matches {
                        if seen.insert(tool_name.clone())
                            && let Some(tool) = tools.get(&tool_name)
                        {
                            out.push(crate::tool::tool_spec(tool.as_ref()));
                        }
                    }
                    continue;
                }
                let tool = tools
                    .get(name)
                    .ok_or_else(|| format!("llm.tools: unknown tool `{name}`"))?;
                if seen.insert(name.clone()) {
                    out.push(crate::tool::tool_spec(tool.as_ref()));
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    struct NamedTool(String);

    impl crate::tool::Tool for NamedTool {
        fn name(&self) -> &str {
            &self.0
        }

        fn tier(&self) -> crate::tool::Tier {
            crate::tool::Tier::Zero
        }

        fn call<'a>(
            &'a self,
            _args: crate::tool::ToolArgs,
            _ctx: &'a crate::tool::ToolCtx,
        ) -> crate::tool::BoxFut<'a, crate::tool::ToolResult> {
            Box::pin(async { Ok(Value::Unit) })
        }
    }

    fn parse(named: Vec<(String, Value)>) -> Result<LlmNodeArgs, RuntimeError> {
        parse_llm_args_from_toolargs(
            &ToolArgs {
                positional: Vec::new(),
                named,
            },
            &crate::tool::ToolRegistry::new(),
        )
    }

    #[test]
    fn parses_exact_reasoning_selection() {
        let args = parse(vec![("reasoning".into(), Value::Str("high@pro".into()))]).unwrap();
        assert_eq!(
            args.reasoning,
            Some(crate::provider::ReasoningSelection::Effort {
                effort: crate::provider::ReasoningEffort::High,
                execution_mode: Some(crate::provider::ReasoningExecutionMode::Pro),
            })
        );
    }

    #[test]
    fn exact_reasoning_takes_precedence_over_legacy_thinking() {
        let args = parse(vec![
            ("thinking".into(), Value::Bool(true)),
            ("reasoning".into(), Value::Str("off".into())),
        ])
        .unwrap();
        assert_eq!(
            args.reasoning,
            Some(crate::provider::ReasoningSelection::Disabled)
        );
    }

    #[test]
    fn rejects_reasoning_budget_outside_u32_range() {
        let result = parse(vec![(
            "reasoning_budget".into(),
            Value::Int(i64::from(u32::MAX) + 1),
        )]);
        assert!(matches!(result, Err(RuntimeError::TypeMismatch { .. })));
    }

    #[test]
    fn parses_effort_alias() {
        let args = parse(vec![("effort".into(), Value::Str("high".into()))]).unwrap();
        assert_eq!(
            args.reasoning,
            Some(crate::provider::ReasoningSelection::Effort {
                effort: crate::provider::ReasoningEffort::High,
                execution_mode: None,
            })
        );
    }

    #[test]
    fn absent_invocation_effort_uses_no_explicit_selection() {
        let args = parse(vec![("effort".into(), Value::Unit)]).unwrap();
        assert_eq!(args.reasoning, None);
    }

    #[test]
    fn rejects_multiple_explicit_reasoning_arguments() {
        let result = parse(vec![
            ("effort".into(), Value::Str("high".into())),
            ("reasoning".into(), Value::Str("auto".into())),
        ]);
        assert!(
            matches!(result, Err(RuntimeError::ToolFailed(message)) if message.contains("cannot be used together"))
        );
    }

    #[test]
    fn rejects_multiple_explicit_reasoning_arguments_in_reverse_order() {
        let result = parse(vec![
            ("reasoning".into(), Value::Str("auto".into())),
            ("effort".into(), Value::Str("high".into())),
        ]);
        assert!(
            matches!(result, Err(RuntimeError::ToolFailed(message)) if message.contains("cannot be used together"))
        );
    }

    #[test]
    fn rejects_reasoning_with_reasoning_budget() {
        let result = parse(vec![
            ("reasoning".into(), Value::Str("auto".into())),
            ("reasoning_budget".into(), Value::Int(4_096)),
        ]);
        assert!(
            matches!(result, Err(RuntimeError::ToolFailed(message)) if message.contains("cannot be used together"))
        );
    }

    #[test]
    fn rejects_effort_with_reasoning_budget_in_either_order() {
        for named in [
            vec![
                ("effort".into(), Value::Str("high".into())),
                ("reasoning_budget".into(), Value::Int(4_096)),
            ],
            vec![
                ("reasoning_budget".into(), Value::Int(4_096)),
                ("effort".into(), Value::Str("high".into())),
            ],
        ] {
            let result = parse(named);
            assert!(
                matches!(result, Err(RuntimeError::ToolFailed(message)) if message.contains("cannot be used together"))
            );
        }
    }

    #[test]
    fn tool_resolution_is_stable_and_deduplicates_overlapping_selectors() {
        fn registry(names: &[&str]) -> crate::tool::ToolRegistry {
            let registry = crate::tool::ToolRegistry::new();
            for name in names {
                registry.register(std::sync::Arc::new(NamedTool((*name).into())));
            }
            registry
        }

        let selectors = vec![
            Value::Str("native.read".into()),
            Value::Str("mcp.*".into()),
            Value::Str("mcp.zeta".into()),
        ];
        let left = resolve_tool_specs_from_values(
            &selectors,
            &registry(&["mcp.zeta", "native.read", "mcp.alpha"]),
        )
        .unwrap();
        let right = resolve_tool_specs_from_values(
            &selectors,
            &registry(&["mcp.alpha", "mcp.zeta", "native.read"]),
        )
        .unwrap();

        let names: Vec<_> = left.iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(names, ["native.read", "mcp.alpha", "mcp.zeta"]);
        assert_eq!(
            serde_json::to_vec(&left).unwrap(),
            serde_json::to_vec(&right).unwrap()
        );
    }

    #[test]
    fn effort_takes_precedence_over_legacy_thinking_in_either_order() {
        for named in [
            vec![
                ("effort".into(), Value::Str("high".into())),
                ("thinking".into(), Value::Bool(false)),
            ],
            vec![
                ("thinking".into(), Value::Bool(false)),
                ("effort".into(), Value::Str("high".into())),
            ],
        ] {
            let args = parse(named).unwrap();
            assert_eq!(
                args.reasoning,
                Some(crate::provider::ReasoningSelection::Effort {
                    effort: crate::provider::ReasoningEffort::High,
                    execution_mode: None,
                })
            );
        }
    }
}
