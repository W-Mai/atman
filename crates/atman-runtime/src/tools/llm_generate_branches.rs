use crate::error::RuntimeError;
use crate::eval::llm_args::parse_llm_args_from_toolargs;
use crate::eval::llm_dispatch::dispatch_llm;
use crate::eval::llm_parse::{llm_result_to_text, parse_list_from_text};
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

pub struct LlmGenerateBranchesTool;

impl Tool for LlmGenerateBranchesTool {
    fn name(&self) -> &str {
        "llm.generate_branches"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let Some(registry) = ctx.registry.as_deref() else {
                return Err(RuntimeError::ToolFailed(
                    "llm.generate_branches: no tool registry available".into(),
                ));
            };

            let prompt = args
                .named("prompt")
                .and_then(|v| match v {
                    Value::Str(s) => Some(s.clone()),
                    _ => None,
                })
                .ok_or_else(|| RuntimeError::MissingArg("llm.generate_branches: prompt".into()))?;

            if prompt.trim().is_empty() {
                return Err(RuntimeError::ToolFailed(
                    "llm.generate_branches: prompt is required".into(),
                ));
            }

            // Parse optional count
            let count = args.named("count").and_then(|v| match v {
                Value::Int(n) if *n > 0 => Some(*n as usize),
                _ => None,
            });

            // Construct prompt
            let count_str = count
                .map(|c| c.to_string())
                .unwrap_or_else(|| "3-8".to_string());
            let tool_prompt = format!(
                "Decompose the following task into {count_str} independent parallel subtasks.\n\
                 Each subtask should be a concise one-line description.\n\
                 Respond as a JSON array of strings, e.g. [\"subtask 1\", \"subtask 2\", ...].\n\
                 No markdown, no explanation, just the JSON array.\n\n{prompt}"
            );

            let parse_retry_count = extract_retry(&args);

            for attempt in 0..=parse_retry_count {
                let mut llm_args = parse_llm_args_from_toolargs(&args, registry)?;
                llm_args.retry_count = 0;
                llm_args.messages_override = None;
                llm_args.fallback_value = None;
                llm_args.call_purpose = crate::context_plan::ContextCallPurpose::BranchGeneration;
                llm_args.prompt = Some(if attempt == 0 {
                    tool_prompt.clone()
                } else {
                    format!(
                        "{tool_prompt}\n\nYour previous response could not be parsed. Please respond with ONLY a JSON array of strings."
                    )
                });

                let result = dispatch_llm(llm_args, ctx).await;
                if let Value::Err(e) = result {
                    return Err(e);
                }

                match parse_branches_result(&result) {
                    Ok(list) => {
                        let mut list = list;
                        if let Some(c) = count {
                            if list.len() > c {
                                crate::notify!(
                                    warn,
                                    "llm.generate_branches: generated {} branches, truncating to {c}",
                                    list.len()
                                );
                                list.truncate(c);
                            }
                        }
                        return Ok(Value::List(list));
                    }
                    Err(_) if attempt < parse_retry_count => continue,
                    Err(e) => {
                        return Err(RuntimeError::ToolFailed(format!(
                            "llm.generate_branches: {e}"
                        )));
                    }
                }
            }

            Err(RuntimeError::ToolFailed(
                "llm.generate_branches: unreachable".into(),
            ))
        })
    }
}

fn extract_retry(args: &ToolArgs) -> u32 {
    args.named("retry")
        .and_then(|v| match v {
            Value::Int(n) if *n >= 0 => Some(*n as u32),
            _ => None,
        })
        .unwrap_or(0)
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        Value::Int(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Unit => String::new(),
        other => format!("{:?}", other),
    }
}

fn parse_branches_result(result: &Value) -> Result<Vec<Value>, String> {
    // If already List (assistant_message_to_value pre-parsed JSON array)
    if let Value::List(items) = result {
        return Ok(items
            .iter()
            .map(|v| match v {
                Value::Str(_) => v.clone(),
                other => Value::Str(value_to_string(other)),
            })
            .collect());
    }

    // Extract text and parse
    let text = match llm_result_to_text(result) {
        Some(t) => t,
        None => return Err("could not extract text from LLM response".into()),
    };

    let items = parse_list_from_text(&text);
    if items.is_empty() {
        return Err("could not parse any branches from response".into());
    }

    Ok(items.into_iter().map(Value::Str).collect())
}
