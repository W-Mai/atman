use crate::error::RuntimeError;
use crate::form::{CompositeForm, FormAnswer, FormKind, FormQuestion, PendingForm};
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

    fn approval_level(&self, _args: &ToolArgs, _ctx: &ToolCtx) -> ApprovalLevel {
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
             For several independent answers, make one `form.ask` call with a `questions`
             list. Each question has an `id`, `kind`, and the fields for that kind.
             If `questions` is present, it takes precedence over the single-question fields.
             The UI keeps all answers as a draft and asks for one final Yes/No confirmation;
             do not make multiple calls expecting the UI to merge them.
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
                "multiline": {"type": "boolean"},
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string"},
                            "kind": {"type": "string"},
                            "prompt": {"type": "string"},
                            "options": {"type": "array", "items": {"type": "string"}},
                            "min": {"type": "integer"},
                            "max": {"type": "integer"},
                            "placeholder": {"type": "string"},
                            "multiline": {"type": "boolean"}
                        },
                        "required": ["id", "kind", "prompt"]
                    }
                }
            }
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let (form, kind, composite) = parse_form_request(&args)?;
            // Daemon clients drive the modal over RPC via the prompt
            // resolver; the in-process TUI subscribes to FormRegistry.
            // Pick whichever the runtime host wired up, prefer the
            // resolver so daemon overrides an accidental fallback.
            if let Some(resolver) = ctx.prompt_resolver.clone() {
                let id = crate::rendezvous::PromptId::now();
                let payload = if composite {
                    serde_json::to_value(&form).unwrap_or(serde_json::Value::Null)
                } else {
                    serde_json::to_value(&kind).unwrap_or(serde_json::Value::Null)
                };
                let timeout = std::time::Duration::from_secs(300);
                let Some(answer_json) =
                    crate::rendezvous::await_expirable_prompt_with_payload_cancel(
                        &resolver,
                        id,
                        "form_ask",
                        payload,
                        timeout,
                        &ctx.cancel,
                    )
                    .await?
                else {
                    return Ok(submission_to_value(
                        &crate::form::FormSubmission::Rejected,
                        composite,
                    ));
                };
                let submission = serde_json::from_value::<crate::form::FormSubmission>(answer_json)
                    .map_err(|error| {
                        RuntimeError::ToolFailed(format!(
                            "form.ask: invalid prompt submission: {error}"
                        ))
                    })?;
                return Ok(submission_to_value(&submission, composite));
            }
            let forms = ctx.forms.as_ref().ok_or_else(|| {
                RuntimeError::ToolFailed(
                    "form.ask: no FormRegistry or PromptResolver attached".into(),
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
                form,
                kind,
                emitted_at: chrono::Utc::now(),
            };
            let rx = forms.request(pending);
            let submission = await_local_submission(
                forms,
                form_id,
                rx,
                std::time::Duration::from_secs(300),
                &ctx.cancel,
            )
            .await;
            Ok(submission_to_value(&submission, composite))
        })
    }
}

async fn await_local_submission(
    forms: &crate::session::FormRegistry,
    form_id: String,
    mut rx: tokio::sync::oneshot::Receiver<crate::form::FormSubmission>,
    timeout: std::time::Duration,
    cancel: &tokio_util::sync::CancellationToken,
) -> crate::form::FormSubmission {
    tokio::select! {
        result = tokio::time::timeout(timeout, &mut rx) => match result {
            Ok(Ok(submission)) => submission,
            Ok(Err(_)) => {
                forms.cancel(&form_id);
                crate::form::FormSubmission::Rejected
            }
            Err(_) => {
                if forms.expire(&form_id) {
                    crate::form::FormSubmission::Rejected
                } else {
                    tokio::select! {
                        result = &mut rx => result.unwrap_or(crate::form::FormSubmission::Rejected),
                        _ = cancel.cancelled() => {
                            forms.cancel(&form_id);
                            crate::form::FormSubmission::Rejected
                        }
                    }
                }
            }
        },
        _ = cancel.cancelled() => {
            forms.cancel(&form_id);
            crate::form::FormSubmission::Rejected
        }
    }
}

fn submission_to_value(submission: &crate::form::FormSubmission, composite: bool) -> Value {
    match submission {
        crate::form::FormSubmission::Submitted { answers } if composite => Value::Struct(vec![
            ("kind".into(), Value::Str("submitted".into())),
            (
                "answers".into(),
                Value::List(answers.iter().map(answer_to_value).collect()),
            ),
        ]),
        crate::form::FormSubmission::Submitted { answers } => {
            answers.first().map(answer_to_value).unwrap_or_else(|| {
                Value::Struct(vec![("kind".into(), Value::Str("cancelled".into()))])
            })
        }
        crate::form::FormSubmission::Rejected => {
            Value::Struct(vec![("kind".into(), Value::Str("cancelled".into()))])
        }
    }
}

fn parse_form_request(args: &ToolArgs) -> Result<(CompositeForm, FormKind, bool), RuntimeError> {
    match (args.named("questions"), args.named("kind")) {
        (Some(Value::List(items)), _) => {
            if items.is_empty() {
                return Err(RuntimeError::ToolFailed(
                    "form.ask: `questions` must be non-empty".into(),
                ));
            }
            let mut questions = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                let Value::Struct(fields) = item else {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "struct {id, kind, prompt, ...}".into(),
                        actual: item.kind_name().into(),
                    });
                };
                let get = |name: &str| {
                    fields
                        .iter()
                        .find(|(key, _)| key == name)
                        .map(|(_, value)| value)
                };
                let id = match get("id") {
                    Some(Value::Str(value)) if !value.is_empty() => value.clone(),
                    Some(value) => {
                        return Err(RuntimeError::TypeMismatch {
                            expected: "string".into(),
                            actual: value.kind_name().into(),
                        });
                    }
                    None => return Err(RuntimeError::MissingArg(format!("questions[{index}].id"))),
                };
                if questions
                    .iter()
                    .any(|question: &FormQuestion| question.id == id)
                {
                    return Err(RuntimeError::ToolFailed(format!(
                        "form.ask: duplicate question id `{id}`"
                    )));
                }
                let named = ToolArgs {
                    positional: Vec::new(),
                    named: fields.clone(),
                };
                let kind = parse_form_kind(&named)?;
                questions.push(FormQuestion { id, kind });
            }
            let first = questions[0].kind.clone();
            Ok((CompositeForm { questions }, first, true))
        }
        (Some(value), _) => Err(RuntimeError::TypeMismatch {
            expected: "list<struct>".into(),
            actual: value.kind_name().into(),
        }),
        (None, _) => {
            let kind = parse_form_kind(args)?;
            Ok((
                CompositeForm {
                    questions: vec![FormQuestion {
                        id: "question".into(),
                        kind: kind.clone(),
                    }],
                },
                kind,
                false,
            ))
        }
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

    #[tokio::test]
    async fn local_form_timeout_keeps_pending_entry_answerable() {
        let forms = crate::session::FormRegistry::new();
        let _subscriber = forms.subscribe();
        let form_id = "timed-out".to_string();
        let form = crate::form::CompositeForm {
            questions: vec![crate::form::FormQuestion {
                id: "question".into(),
                kind: FormKind::Confirm { prompt: "?".into() },
            }],
        };
        let pending = PendingForm {
            form_id: form_id.clone(),
            run_id: crate::event::FlowRunId::now(),
            tool_use_id: "tool".into(),
            form,
            kind: FormKind::Confirm { prompt: "?".into() },
            emitted_at: chrono::Utc::now(),
        };
        let rx = forms.request(pending);
        assert_eq!(
            await_local_submission(
                &forms,
                form_id,
                rx,
                std::time::Duration::ZERO,
                &tokio_util::sync::CancellationToken::new(),
            )
            .await,
            crate::form::FormSubmission::Rejected
        );
        assert_eq!(forms.list_pending().len(), 1);
        assert!(forms.submit(
            "timed-out",
            crate::form::FormSubmission::Submitted {
                answers: vec![FormAnswer::Confirmed { value: true }],
            }
        ));
        assert!(forms.list_pending().is_empty());
    }

    #[tokio::test]
    async fn local_form_cancellation_removes_pending_entry() {
        let forms = crate::session::FormRegistry::new();
        let _subscriber = forms.subscribe();
        let form_id = "cancelled".to_string();
        let pending = PendingForm {
            form_id: form_id.clone(),
            run_id: crate::event::FlowRunId::now(),
            tool_use_id: "tool".into(),
            form: crate::form::CompositeForm {
                questions: vec![crate::form::FormQuestion {
                    id: "question".into(),
                    kind: FormKind::Confirm { prompt: "?".into() },
                }],
            },
            kind: FormKind::Confirm { prompt: "?".into() },
            emitted_at: chrono::Utc::now(),
        };
        let rx = forms.request(pending);
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();

        assert_eq!(
            await_local_submission(
                &forms,
                form_id,
                rx,
                std::time::Duration::from_secs(300),
                &cancel,
            )
            .await,
            crate::form::FormSubmission::Rejected
        );
        assert!(forms.list_pending().is_empty());
    }

    #[test]
    fn composite_questions_take_priority_over_single_question_fields() {
        let question = |id: &str, prompt: &str| {
            Value::Struct(vec![
                ("id".into(), Value::Str(id.into())),
                ("kind".into(), Value::Str("text".into())),
                ("prompt".into(), Value::Str(prompt.into())),
            ])
        };
        let args = ToolArgs {
            positional: vec![],
            named: vec![
                (
                    "questions".into(),
                    Value::List(vec![question("name", "Name?"), question("team", "Team?")]),
                ),
                named("kind", Value::Str("confirm".into())),
                named("prompt", Value::Str("Ignore this?".into())),
            ],
        };
        let (form, first, composite) = parse_form_request(&args).unwrap();
        assert!(composite);
        assert_eq!(form.questions.len(), 2);
        assert_eq!(form.questions[0].id, "name");
        assert_eq!(form.questions[1].id, "team");
        assert!(matches!(first, FormKind::Text { .. }));
    }

    #[test]
    fn schema_avoids_top_level_union_keywords() {
        let schema = FormAsk.input_schema();
        for keyword in ["oneOf", "allOf", "anyOf"] {
            assert!(schema.get(keyword).is_none());
        }
    }

    #[test]
    fn parse_composite_questions_rejects_duplicate_ids() {
        let question = |id: &str| {
            Value::Struct(vec![
                ("id".into(), Value::Str(id.into())),
                ("kind".into(), Value::Str("confirm".into())),
                ("prompt".into(), Value::Str("Continue?".into())),
            ])
        };
        let args = ToolArgs {
            positional: vec![],
            named: vec![(
                "questions".into(),
                Value::List(vec![question("same"), question("same")]),
            )],
        };
        let error = parse_form_request(&args).unwrap_err();
        assert!(error.to_string().contains("duplicate question id"));
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
