use crate::error::RuntimeError;
use crate::eval::llm_args::parse_llm_args_from_toolargs;
use crate::eval::llm_dispatch::dispatch_llm;
use crate::eval::llm_parse::{llm_result_to_text, parse_bool_from_text, parse_category_from_text};
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

pub struct LlmClassifyTool;

impl Tool for LlmClassifyTool {
    fn name(&self) -> &str {
        "llm.classify"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let Some(registry) = ctx.registry.as_deref() else {
                return Err(RuntimeError::ToolFailed(
                    "llm.classify: no tool registry available".into(),
                ));
            };

            let prompt = args
                .named("prompt")
                .and_then(|v| match v {
                    Value::Str(s) => Some(s.clone()),
                    _ => None,
                })
                .ok_or_else(|| RuntimeError::MissingArg("llm.classify: prompt".into()))?;

            if prompt.trim().is_empty() {
                return Err(RuntimeError::ToolFailed(
                    "llm.classify: prompt is required".into(),
                ));
            }

            // Parse categories (default: binary yes/no)
            let categories: Vec<String> = match args.named("categories") {
                Some(Value::List(items)) => items
                    .iter()
                    .filter_map(|v| match v {
                        Value::Str(s) => Some(s.clone()),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            let is_binary = categories.is_empty();

            // Construct tool-specific prompt
            let tool_prompt = if is_binary {
                format!("Answer with exactly one word: \"yes\" or \"no\".\n\n{prompt}")
            } else {
                let labels = categories.join(", ");
                format!("Answer with exactly one of these labels: {labels}.\n\n{prompt}")
            };

            let parse_retry_count = extract_retry(&args);

            for attempt in 0..=parse_retry_count {
                let mut llm_args = parse_llm_args_from_toolargs(&args, registry)?;
                llm_args.retry_count = 0;
                llm_args.messages_override = None;
                llm_args.fallback_value = None;
                llm_args.prompt = Some(if attempt == 0 {
                    tool_prompt.clone()
                } else {
                    format!(
                        "{tool_prompt}\n\nYour previous response could not be parsed. Please respond with ONLY the requested format."
                    )
                });

                let result = dispatch_llm(llm_args, ctx).await;
                if let Value::Err(e) = result {
                    return Err(e);
                }

                match parse_classify_result(&result, is_binary, &categories) {
                    Ok(v) => return Ok(v),
                    Err(_) if attempt < parse_retry_count => continue,
                    Err(e) => {
                        return if is_binary {
                            crate::notify!(warn, "llm.classify: parse failed, returning false");
                            Ok(Value::Bool(false))
                        } else {
                            Err(RuntimeError::ToolFailed(format!("llm.classify: {e}")))
                        };
                    }
                }
            }

            // Compiler can't prove loop executes at least once
            Ok(Value::Bool(false))
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

fn parse_classify_result(
    result: &Value,
    is_binary: bool,
    categories: &[String],
) -> Result<Value, String> {
    // Handle pre-parsed types from assistant_message_to_value
    match result {
        Value::Bool(b) if is_binary => return Ok(Value::Bool(*b)),
        Value::Int(n) if is_binary => return Ok(Value::Bool(*n != 0)),
        Value::Float(f) if is_binary => return Ok(Value::Bool(*f != 0.0)),
        _ => {}
    }

    // Extract text
    let text = match llm_result_to_text(result) {
        Some(t) => t,
        None => return Err("could not extract text from LLM response".into()),
    };

    if is_binary {
        match parse_bool_from_text(&text) {
            Some(b) => Ok(Value::Bool(b)),
            None => Err(format!("could not parse as yes/no: {text}")),
        }
    } else {
        match parse_category_from_text(&text, categories) {
            Some(cat) => Ok(Value::Str(cat)),
            None => Err(format!("could not match any category: {text}")),
        }
    }
}
