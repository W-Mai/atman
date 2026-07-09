use crate::error::RuntimeError;
use crate::form::{FormAnswer, FormKind, PendingForm};
use crate::tool::{ApprovalLevel, BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

pub struct FormAsk;

impl Tool for FormAsk {
    fn name(&self) -> &str {
        "form.ask"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn approval_level(&self) -> ApprovalLevel {
        ApprovalLevel::Auto
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Ask the user a structured question through a form modal. Pass `kind`
             plus fields required for that kind:
             \
             confirm       { kind:\"confirm\", prompt }
             single_select { kind:\"single_select\", prompt, options[] }
             multi_select  { kind:\"multi_select\", prompt, options[], min?, max? }
             text          { kind:\"text\", prompt, placeholder?, multiline? }
             \
             Returns a struct { kind, ... } where kind is one of \
             confirmed | selected | multi_selected | text_entered | cancelled.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "kind": {"type": "string"},
                "prompt": {"type": "string"},
                "options": {"type": "array", "items": {"type": "string"}},
                "min": {"type": "integer"},
                "max": {"type": "integer"},
                "placeholder": {"type": "string"},
                "multiline": {"type": "boolean"}
            },
            "required": ["kind", "prompt"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let kind = parse_form_kind(&args)?;
            let forms = ctx.forms.as_ref().ok_or_else(|| {
                RuntimeError::ToolFailed(
                    "form.ask: no FormRegistry attached (headless run?)".into(),
                )
            })?;
            let run_id = ctx.flow_run_id.clone().ok_or_else(|| {
                RuntimeError::ToolFailed("form.ask: no flow_run_id in ctx".into())
            })?;
            let form_id = uuid::Uuid::now_v7().to_string();
            let pending = PendingForm {
                form_id: form_id.clone(),
                run_id,
                tool_use_id: ctx.current_node_id.clone().unwrap_or_default(),
                kind,
                emitted_at: chrono::Utc::now(),
            };
            let rx = forms.request(pending);
            let answer = rx.await.unwrap_or(FormAnswer::Cancelled);
            Ok(answer_to_value(&answer))
        })
    }
}

fn parse_form_kind(args: &ToolArgs) -> Result<FormKind, RuntimeError> {
    let kind = named_str(args, "kind")?;
    let prompt = named_str(args, "prompt")?;
    match kind.as_str() {
        "confirm" => Ok(FormKind::Confirm { prompt }),
        "single_select" => {
            let options = named_string_list(args, "options")?;
            if options.is_empty() {
                return Err(RuntimeError::ToolFailed(
                    "form.ask(single_select): options must be non-empty".into(),
                ));
            }
            Ok(FormKind::SingleSelect { prompt, options })
        }
        "multi_select" => {
            let options = named_string_list(args, "options")?;
            if options.is_empty() {
                return Err(RuntimeError::ToolFailed(
                    "form.ask(multi_select): options must be non-empty".into(),
                ));
            }
            let min = named_usize(args, "min")?;
            let max = named_usize(args, "max")?;
            if let (Some(m), Some(mx)) = (min, max)
                && mx < m
            {
                return Err(RuntimeError::ToolFailed(
                    "form.ask(multi_select): max must be >= min".into(),
                ));
            }
            Ok(FormKind::MultiSelect {
                prompt,
                options,
                min,
                max,
            })
        }
        "text" => {
            let placeholder = named_opt_str(args, "placeholder")?;
            let multiline = matches!(args.named("multiline"), Some(Value::Bool(true)));
            Ok(FormKind::Text {
                prompt,
                placeholder,
                multiline,
            })
        }
        other => Err(RuntimeError::ToolFailed(format!(
            "form.ask: unknown kind `{other}` (expected confirm | single_select | multi_select | text)"
        ))),
    }
}

fn named_str(args: &ToolArgs, name: &str) -> Result<String, RuntimeError> {
    match args.named(name) {
        Some(Value::Str(s)) => Ok(s.clone()),
        Some(v) => Err(RuntimeError::TypeMismatch {
            expected: "string".into(),
            actual: v.kind_name().into(),
        }),
        None => Err(RuntimeError::MissingArg(name.into())),
    }
}

fn named_opt_str(args: &ToolArgs, name: &str) -> Result<Option<String>, RuntimeError> {
    match args.named(name) {
        Some(Value::Str(s)) => Ok(Some(s.clone())),
        Some(Value::Unit) | None => Ok(None),
        Some(v) => Err(RuntimeError::TypeMismatch {
            expected: "string".into(),
            actual: v.kind_name().into(),
        }),
    }
}

fn named_string_list(args: &ToolArgs, name: &str) -> Result<Vec<String>, RuntimeError> {
    match args.named(name) {
        Some(Value::List(items)) => items
            .iter()
            .map(|v| match v {
                Value::Str(s) => Ok(s.clone()),
                other => Err(RuntimeError::TypeMismatch {
                    expected: "string".into(),
                    actual: other.kind_name().into(),
                }),
            })
            .collect(),
        Some(v) => Err(RuntimeError::TypeMismatch {
            expected: "list<string>".into(),
            actual: v.kind_name().into(),
        }),
        None => Err(RuntimeError::MissingArg(name.into())),
    }
}

fn named_usize(args: &ToolArgs, name: &str) -> Result<Option<usize>, RuntimeError> {
    match args.named(name) {
        Some(Value::Int(i)) if *i >= 0 => Ok(Some(*i as usize)),
        Some(Value::Int(_)) => Err(RuntimeError::ToolFailed(format!(
            "form.ask: `{name}` must be non-negative"
        ))),
        Some(Value::Unit) | None => Ok(None),
        Some(v) => Err(RuntimeError::TypeMismatch {
            expected: "int".into(),
            actual: v.kind_name().into(),
        }),
    }
}

fn answer_to_value(answer: &FormAnswer) -> Value {
    match answer {
        FormAnswer::Confirmed { value } => Value::Struct(vec![
            ("kind".into(), Value::Str("confirmed".into())),
            ("value".into(), Value::Bool(*value)),
        ]),
        FormAnswer::Selected { index, label } => Value::Struct(vec![
            ("kind".into(), Value::Str("selected".into())),
            ("index".into(), Value::Int(*index as i64)),
            ("label".into(), Value::Str(label.clone())),
        ]),
        FormAnswer::MultiSelected { indices, labels } => Value::Struct(vec![
            ("kind".into(), Value::Str("multi_selected".into())),
            (
                "indices".into(),
                Value::List(indices.iter().map(|i| Value::Int(*i as i64)).collect()),
            ),
            (
                "labels".into(),
                Value::List(labels.iter().map(|s| Value::Str(s.clone())).collect()),
            ),
        ]),
        FormAnswer::TextEntered { text } => Value::Struct(vec![
            ("kind".into(), Value::Str("text_entered".into())),
            ("text".into(), Value::Str(text.clone())),
        ]),
        FormAnswer::Cancelled => {
            Value::Struct(vec![("kind".into(), Value::Str("cancelled".into()))])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::form::FormKind;
    use crate::tool::ToolArgs;

    fn named(name: &str, v: Value) -> (String, Value) {
        (name.into(), v)
    }

    #[test]
    fn parse_confirm_kind() {
        let args = ToolArgs {
            positional: vec![],
            named: vec![
                named("kind", Value::Str("confirm".into())),
                named("prompt", Value::Str("sure?".into())),
            ],
        };
        assert!(matches!(
            parse_form_kind(&args).unwrap(),
            FormKind::Confirm { .. }
        ));
    }

    #[test]
    fn parse_single_select_rejects_empty_options() {
        let args = ToolArgs {
            positional: vec![],
            named: vec![
                named("kind", Value::Str("single_select".into())),
                named("prompt", Value::Str("pick".into())),
                named("options", Value::List(vec![])),
            ],
        };
        let err = parse_form_kind(&args).unwrap_err();
        assert!(err.to_string().contains("non-empty"));
    }

    #[test]
    fn parse_multi_select_validates_bounds() {
        let args = ToolArgs {
            positional: vec![],
            named: vec![
                named("kind", Value::Str("multi_select".into())),
                named("prompt", Value::Str("tags".into())),
                named(
                    "options",
                    Value::List(vec![Value::Str("a".into()), Value::Str("b".into())]),
                ),
                named("min", Value::Int(3)),
                named("max", Value::Int(1)),
            ],
        };
        let err = parse_form_kind(&args).unwrap_err();
        assert!(err.to_string().contains("max must be >= min"));
    }

    #[test]
    fn parse_text_defaults_multiline_to_false() {
        let args = ToolArgs {
            positional: vec![],
            named: vec![
                named("kind", Value::Str("text".into())),
                named("prompt", Value::Str("name?".into())),
            ],
        };
        match parse_form_kind(&args).unwrap() {
            FormKind::Text { multiline, .. } => assert!(!multiline),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn parse_unknown_kind_errors_with_hint() {
        let args = ToolArgs {
            positional: vec![],
            named: vec![
                named("kind", Value::Str("weird".into())),
                named("prompt", Value::Str("?".into())),
            ],
        };
        let err = parse_form_kind(&args).unwrap_err();
        assert!(err.to_string().contains("weird"));
        assert!(err.to_string().contains("confirm"));
    }

    #[test]
    fn answer_confirmed_becomes_struct() {
        let v = answer_to_value(&FormAnswer::Confirmed { value: true });
        assert_eq!(v.field("kind").unwrap().kind_name(), "string");
        assert!(matches!(v.field("value"), Some(Value::Bool(true))));
    }

    #[test]
    fn answer_multi_selected_carries_indices_and_labels() {
        let v = answer_to_value(&FormAnswer::MultiSelected {
            indices: vec![0, 2],
            labels: vec!["a".into(), "c".into()],
        });
        let indices = match v.field("indices").unwrap() {
            Value::List(l) => l,
            _ => panic!("expected list"),
        };
        assert_eq!(indices.len(), 2);
    }

    #[test]
    fn answer_cancelled_is_kind_only_struct() {
        let v = answer_to_value(&FormAnswer::Cancelled);
        assert!(matches!(v.field("kind"), Some(Value::Str(s)) if s == "cancelled"));
        assert!(v.field("value").is_none());
    }
}
