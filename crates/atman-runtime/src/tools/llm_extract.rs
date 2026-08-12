use crate::error::RuntimeError;
use crate::eval::llm_args::parse_llm_args_from_toolargs;
use crate::eval::llm_dispatch::dispatch_llm;
use crate::eval::llm_parse::{
    FieldDef, coerce_field, extract_json_from_text, parse_field_definitions, validate_struct_fields,
};
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

pub struct LlmExtractTool;

impl Tool for LlmExtractTool {
    fn name(&self) -> &str {
        "llm.extract"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let Some(registry) = ctx.registry.as_deref() else {
                return Err(RuntimeError::ToolFailed(
                    "llm.extract: no tool registry available".into(),
                ));
            };

            let prompt = args
                .named("prompt")
                .and_then(|v| match v {
                    Value::Str(s) => Some(s.clone()),
                    _ => None,
                })
                .ok_or_else(|| RuntimeError::MissingArg("llm.extract: prompt".into()))?;

            if prompt.trim().is_empty() {
                return Err(RuntimeError::ToolFailed(
                    "llm.extract: prompt is required".into(),
                ));
            }

            // Parse fields
            let fields_raw = match args.named("fields") {
                Some(Value::Struct(pairs)) => pairs.clone(),
                _ => {
                    return Err(RuntimeError::MissingArg(
                        "llm.extract: fields (struct of field definitions)".into(),
                    ));
                }
            };

            if fields_raw.is_empty() {
                return Err(RuntimeError::ToolFailed(
                    "llm.extract: fields cannot be empty".into(),
                ));
            }

            let field_defs = parse_field_definitions(&fields_raw)
                .map_err(|e| RuntimeError::ToolFailed(format!("llm.extract: {e}")))?;

            // Construct prompt
            let field_descs: Vec<String> = field_defs
                .iter()
                .map(|f| {
                    if f.ty.is_empty() {
                        format!("  - \"{}\": {}", f.name, f.description)
                    } else {
                        format!("  - \"{}\": {} — {}", f.name, f.ty, f.description)
                    }
                })
                .collect();

            let tool_prompt = format!(
                "Extract structured information. Respond as a JSON object with exactly these fields:\n\n{}\n\nRespond with ONLY the JSON object, no markdown, no explanation.\n\n{}",
                field_descs.join("\n"),
                prompt
            );

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
                        "{tool_prompt}\n\nYour previous response could not be parsed. Please respond with ONLY a JSON object."
                    )
                });

                let result = dispatch_llm(llm_args, ctx).await;
                if let Value::Err(e) = result {
                    return Err(e);
                }

                match parse_extract_result(&result, &field_defs) {
                    Ok(v) => return Ok(v),
                    Err(_) if attempt < parse_retry_count => continue,
                    Err(e) => {
                        return Err(RuntimeError::ToolFailed(format!("llm.extract: {e}")));
                    }
                }
            }

            Err(RuntimeError::ToolFailed("llm.extract: unreachable".into()))
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

fn parse_extract_result(result: &Value, field_defs: &[FieldDef]) -> Result<Value, String> {
    // If already Struct (assistant_message_to_value pre-parsed JSON)
    let pairs = match result {
        Value::Struct(pairs) => pairs.clone(),
        Value::Str(s) => {
            let json = extract_json_from_text(s).ok_or_else(|| {
                format!(
                    "could not parse response as JSON: {}",
                    &s[..s.len().min(100)]
                )
            })?;
            match Value::from_json(json) {
                Value::Struct(pairs) => pairs,
                other => return Err(format!("expected JSON object, got {}", other.kind_name())),
            }
        }
        Value::Message(m) => {
            let text = m.text_concat();
            if text.is_empty() {
                return Err("empty response from LLM".into());
            }
            let json = extract_json_from_text(&text).ok_or_else(|| {
                format!(
                    "could not parse response as JSON: {}",
                    &text[..text.len().min(100)]
                )
            })?;
            match Value::from_json(json) {
                Value::Struct(pairs) => pairs,
                other => return Err(format!("expected JSON object, got {}", other.kind_name())),
            }
        }
        _ => return Err("unexpected value type from LLM".into()),
    };

    // Validate required fields
    let required: Vec<&str> = field_defs.iter().map(|f| f.name.as_str()).collect();
    validate_struct_fields(&pairs, &required)?;

    // Coerce field types
    let mut coerced: Vec<(String, Value)> = Vec::with_capacity(pairs.len());
    for (name, val) in pairs {
        let field_def = field_defs.iter().find(|f| f.name == name);
        match field_def {
            Some(def) if !def.ty.is_empty() => {
                let coerced_val = coerce_field(val, &def.ty)?;
                coerced.push((name, coerced_val));
            }
            _ => {
                coerced.push((name, val));
            }
        }
    }

    Ok(Value::Struct(coerced))
}
