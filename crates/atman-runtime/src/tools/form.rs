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
             The UI keeps all answers as a draft and asks for one final Yes/No confirmation;
             do not make multiple calls expecting the UI to merge them.
             \
             Returns a struct { kind, ... } where kind is one of \
             confirmed | selected | multi_selected | text_entered | cancelled. \
             Composite calls return { kind: \"submitted\", answers: [...] } or { kind: \"cancelled\" }.",
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
            },
            "oneOf": [
                {"required": ["kind", "prompt"], "not": {"required": ["questions"]}},
                {"required": ["questions"], "not": {"anyOf": [{"required": ["kind"]}, {"required": ["prompt"]}]}}
            ]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let (form, composite) = parse_form_request(&args)?;
            let submission =
                request_form(form, ctx, Some(std::time::Duration::from_secs(300))).await?;
            Ok(submission_to_value(&submission, composite))
        })
    }
}

pub(crate) async fn request_form(
    form: CompositeForm,
    ctx: &ToolCtx,
    local_timeout: Option<std::time::Duration>,
) -> Result<crate::form::FormSubmission, RuntimeError> {
    let submission = if let Some(forms) = ctx
        .forms
        .as_ref()
        .filter(|forms| forms.subscriber_count() > 0 || ctx.prompt_resolver.is_none())
    {
        let kind = form
            .questions
            .first()
            .ok_or_else(|| RuntimeError::ToolFailed("form.ask: empty form".into()))?
            .kind
            .clone();
        let run_id = ctx
            .flow_run_id
            .clone()
            .ok_or_else(|| RuntimeError::ToolFailed("form.ask: no flow_run_id in ctx".into()))?;
        let response = forms.request(PendingForm {
            form_id: uuid::Uuid::now_v7().to_string(),
            run_id,
            tool_use_id: ctx.current_node_id.clone().unwrap_or_default(),
            form: form.clone(),
            kind,
            emitted_at: chrono::Utc::now(),
        });
        match local_timeout {
            Some(timeout) => tokio::time::timeout(timeout, response)
                .await
                .ok()
                .and_then(Result::ok),
            None => response.await.ok(),
        }
        .unwrap_or(crate::form::FormSubmission::Rejected)
    } else {
        let resolver = ctx.prompt_resolver.as_ref().ok_or_else(|| {
            RuntimeError::ToolFailed("form.ask: no FormRegistry or PromptResolver attached".into())
        })?;
        let payload = serde_json::to_value(&form).map_err(|error| {
            RuntimeError::ToolFailed(format!("form.ask: cannot encode request: {error}"))
        })?;
        let answer = crate::rendezvous::await_prompt_with_payload(
            resolver,
            crate::rendezvous::PromptId::now(),
            "form_ask",
            payload,
            std::time::Duration::from_secs(300),
        )
        .await?;
        serde_json::from_value::<crate::form::FormSubmission>(answer).map_err(|error| {
            RuntimeError::ToolFailed(format!("form.ask: invalid submission: {error}"))
        })?
    };
    form.validate_submission(&submission).map_err(|error| {
        RuntimeError::ToolFailed(format!("form.ask: invalid submission: {error}"))
    })?;
    Ok(submission)
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

fn parse_form_request(args: &ToolArgs) -> Result<(CompositeForm, bool), RuntimeError> {
    match (args.named("questions"), args.named("kind")) {
        (Some(Value::List(items)), None) => {
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
            Ok((CompositeForm { questions }, true))
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
                        kind,
                    }],
                },
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

    #[tokio::test(start_paused = true)]
    async fn local_form_wait_cancellation_and_timeout_clear_pending_requests() {
        let session = std::sync::Arc::new(crate::session::Session::open_ephemeral());
        let forms = session.forms();
        let subscriber = forms.subscribe();
        let mut ctx = ToolCtx::new().with_session_runtime(session.clone());
        ctx.flow_run_id = Some(crate::event::FlowRunId::now());
        let args = ToolArgs {
            positional: vec![],
            named: vec![
                named("kind", Value::Str("confirm".into())),
                named("prompt", Value::Str("Continue?".into())),
            ],
        };
        let mut first = FormAsk.call(args.clone(), &ctx);
        let mut second = FormAsk.call(args.clone(), &ctx);
        assert!(futures::poll!(&mut first).is_pending());
        assert!(futures::poll!(&mut second).is_pending());
        assert_eq!(subscriber.borrow().len(), 2);
        drop(first);
        let second_id = forms.list_pending()[0].form_id.clone();
        assert_eq!(forms.list_pending().len(), 1);
        assert!(forms.submit(
            &second_id,
            crate::form::FormSubmission::Submitted {
                answers: vec![FormAnswer::Confirmed { value: true }],
            }
        ));
        assert!(matches!(
            second.await.unwrap().field("kind"),
            Some(Value::Str(kind)) if kind == "confirmed"
        ));
        let timed_out = FormAsk.call(args, &ctx).await.unwrap();
        assert!(matches!(
            timed_out.field("kind"),
            Some(Value::Str(kind)) if kind == "cancelled"
        ));
        assert!(subscriber.borrow().is_empty());
        let abandoned = session
            .sink()
            .snapshot()
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    crate::event::Event::FormResolved {
                        abandoned: true,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(abandoned, 2);
    }

    #[test]
    fn parse_composite_questions() {
        let question = |id: &str, prompt: &str| {
            Value::Struct(vec![
                ("id".into(), Value::Str(id.into())),
                ("kind".into(), Value::Str("text".into())),
                ("prompt".into(), Value::Str(prompt.into())),
            ])
        };
        let args = ToolArgs {
            positional: vec![],
            named: vec![(
                "questions".into(),
                Value::List(vec![question("name", "Name?"), question("team", "Team?")]),
            )],
        };
        let (form, composite) = parse_form_request(&args).unwrap();
        assert!(composite);
        assert_eq!(form.questions.len(), 2);
        assert_eq!(form.questions[0].id, "name");
        assert_eq!(form.questions[1].id, "team");
        assert!(matches!(form.questions[0].kind, FormKind::Text { .. }));
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
    #[tokio::test]
    async fn session_form_calls_keep_run_identity_and_do_not_use_fallback_resolvers() {
        for entry in ["ask", "parent"] {
            for expression in [
                r#"user_confirm("Continue?")"#,
                r#"form.ask(kind: "confirm", prompt: "Continue?")"#,
                r#"form.ask(questions: [{id: "confirm", kind: "confirm", prompt: "Continue?"}])"#,
            ] {
                for accepted in [false, true] {
                    let session = std::sync::Arc::new(crate::session::Session::open_ephemeral());
                    let forms = session.forms();
                    let subscriber = forms.subscribe();
                    let mut executor = crate::Executor::with_events(session.sink().clone());
                    executor.tool_ctx.prompt_resolver = Some(std::sync::Arc::new(
                        crate::rendezvous::AutoResolveResolver {
                            default: serde_json::Value::Null,
                        },
                    ));
                    let file = atman_dsl::parse::parse_file(&format!(
                        "flow ask() {{ return {expression} }}\nflow parent() {{ return subflow(ask) }}"
                    )).unwrap();
                    let mut run = Box::pin(executor.run_in_turn(
                        &file,
                        entry,
                        vec![],
                        None,
                        Some(session.clone()),
                    ));
                    assert!(futures::poll!(&mut run).is_pending());
                    let pending = forms.list_pending().pop().unwrap();
                    let started = session
                        .sink()
                        .snapshot()
                        .into_iter()
                        .find_map(|event| match event {
                            crate::event::Event::FlowStart {
                                run_id, flow_name, ..
                            } if flow_name == "ask" => Some(run_id),
                            _ => None,
                        })
                        .unwrap();
                    assert_eq!(pending.run_id, started);
                    assert!(!pending.tool_use_id.is_empty());
                    assert!(forms.submit(
                        &pending.form_id,
                        crate::form::FormSubmission::Submitted {
                            answers: vec![FormAnswer::Confirmed { value: accepted }],
                        }
                    ));
                    let value = run.await.unwrap().to_json();
                    if expression.starts_with("user_confirm") {
                        assert_eq!(value, accepted);
                    } else if expression.contains("questions:") {
                        assert_eq!(value["kind"], "submitted");
                        assert_eq!(value["answers"][0]["value"], accepted);
                    } else {
                        assert_eq!(value["kind"], "confirmed");
                        assert_eq!(value["value"], accepted);
                    }
                    assert!(subscriber.borrow().is_empty());
                }
            }
        }
    }

    #[tokio::test]
    async fn resolver_answers_are_validated_without_coercing_invalid_data_to_rejection() {
        let args = ToolArgs {
            positional: vec![],
            named: vec![
                named("kind", Value::Str("confirm".into())),
                named("prompt", Value::Str("Continue?".into())),
            ],
        };
        for answer in [
            serde_json::json!(true),
            serde_json::json!({"kind": "confirmed", "value": true}),
            serde_json::json!({"status": "submitted", "answers": [{"kind": "text_entered", "text": "wrong kind"}]}),
            serde_json::json!({"status": "submitted", "answers": []}),
        ] {
            let mut ctx = ToolCtx::new();
            ctx.prompt_resolver = Some(std::sync::Arc::new(
                crate::rendezvous::AutoResolveResolver { default: answer },
            ));
            let error = FormAsk.call(args.clone(), &ctx).await.unwrap_err();
            assert!(error.to_string().contains("invalid submission"));
        }
    }

    #[tokio::test]
    async fn user_confirm_resolver_receives_the_same_submission_contract_without_form_subscribers()
    {
        for attached in [false, true] {
            for answer in [false, true] {
                let session =
                    attached.then(|| std::sync::Arc::new(crate::Session::open_ephemeral()));
                let mut executor = crate::Executor::new();
                executor.tool_ctx.prompt_resolver = Some(std::sync::Arc::new(
                    crate::rendezvous::AutoResolveResolver {
                        default: serde_json::to_value(crate::form::FormSubmission::Submitted {
                            answers: vec![FormAnswer::Confirmed { value: answer }],
                        })
                        .unwrap(),
                    },
                ));
                let file = atman_dsl::parse::parse_file(
                    r#"flow confirm() { return user_confirm("Continue?") }"#,
                )
                .unwrap();
                assert!(matches!(
                    executor.run_in_turn(&file, "confirm", vec![], None, session.clone()).await.unwrap(),
                    Value::Bool(value) if value == answer
                ));
                if let Some(session) = session {
                    assert!(session.forms().list_pending().is_empty());
                }
            }
        }
    }
}
