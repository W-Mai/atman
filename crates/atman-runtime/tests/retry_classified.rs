mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use atman_dsl::parse::parse_file;
use atman_runtime::error::RuntimeError;
use atman_runtime::event::{NodeEvent, Observable};
use atman_runtime::message::Message;
use atman_runtime::provider::{AssistantMessage, LlmRequest, Provider, StopReason, TokenUsage};
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::tool::BoxFut;
use atman_runtime::{Executor, Session, Value};

struct ScriptedProvider {
    name: String,
    outcomes: Vec<Result<String, RuntimeError>>,
    calls: AtomicUsize,
    request_tokens: std::sync::Mutex<Vec<u64>>,
    requests: std::sync::Mutex<Vec<Vec<Message>>>,
}

impl ScriptedProvider {
    fn new(name: &str, outcomes: Vec<Result<String, RuntimeError>>) -> Self {
        Self {
            name: name.to_string(),
            outcomes,
            calls: AtomicUsize::new(0),
            request_tokens: std::sync::Mutex::new(Vec::new()),
            requests: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Provider for ScriptedProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn call<'a>(&'a self, req: LlmRequest) -> BoxFut<'a, Result<AssistantMessage, RuntimeError>> {
        Box::pin(async move {
            let idx = self.calls.fetch_add(1, Ordering::SeqCst);
            self.requests.lock().unwrap().push(req.messages.clone());
            self.request_tokens.lock().unwrap().push(
                atman_runtime::compaction::estimate_tokens_for_messages(&req.messages),
            );
            let outcome = self
                .outcomes
                .get(idx)
                .cloned()
                .unwrap_or_else(|| Err(RuntimeError::ToolFailed("scripted: exhausted".into())));
            match outcome {
                Ok(text) => Ok(AssistantMessage {
                    message: atman_runtime::message::Message::assistant_text(
                        req.messages
                            .first()
                            .map(|m| m.turn_id.clone())
                            .unwrap_or_else(atman_runtime::event::TurnId::now),
                        text,
                    ),
                    stop_reason: StopReason::End,
                    token_usage: TokenUsage::default(),
                    timing: atman_runtime::provider::CallTiming::default(),
                    model: String::new(),
                    response_id: None,
                }),
                Err(e) => Err(e),
            }
        })
    }

    fn call_streaming(&self, req: LlmRequest) -> Observable<AssistantMessage> {
        use tokio::sync::broadcast;
        use tokio_util::sync::CancellationToken;
        let (tx, events) = broadcast::channel(16);
        let cancel = CancellationToken::new();
        let idx = self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests.lock().unwrap().push(req.messages.clone());
        self.request_tokens.lock().unwrap().push(
            atman_runtime::compaction::estimate_tokens_for_messages(&req.messages),
        );
        let outcome = self
            .outcomes
            .get(idx)
            .cloned()
            .unwrap_or_else(|| Err(RuntimeError::ToolFailed("scripted: exhausted".into())));
        let turn_id = req
            .messages
            .first()
            .map(|m| m.turn_id.clone())
            .unwrap_or_else(atman_runtime::event::TurnId::now);
        let output: BoxFut<'static, Result<AssistantMessage, RuntimeError>> =
            Box::pin(async move {
                let _ = tx.send(NodeEvent::LlmDone { total_tokens: 0 });
                match outcome {
                    Ok(text) => Ok(AssistantMessage {
                        message: atman_runtime::message::Message::assistant_text(turn_id, text),
                        stop_reason: StopReason::End,
                        token_usage: TokenUsage::default(),
                        timing: atman_runtime::provider::CallTiming::default(),
                        model: String::new(),
                        response_id: None,
                    }),
                    Err(e) => Err(e),
                }
            });
        Observable {
            output,
            events,
            cancel,
        }
    }
}

fn build_long_history(session: &Session, turn_count: usize) {
    let base = "x".repeat(4000);
    for i in 0..turn_count {
        let turn = atman_runtime::event::TurnId::now();
        session.append_message(
            Message::user_text(turn.clone(), format!("{base} user {i}")),
            None,
        );
        session.append_message(
            Message::assistant_text(turn, format!("{base} assistant {i}")),
            None,
        );
    }
}

#[test]
fn attachment_failures_are_bound_to_the_request_and_context_owner() {
    use atman_runtime::context_state::ContextState;
    use atman_runtime::event::{ContextBase, ContextId, Event, FlowRunId, TurnId};
    use atman_runtime::message::{ImageData, ImageSource, MessagePart, MessagePartId};
    use atman_runtime::tool::{HistorySegment, Tool, ToolArgs, ToolCtx, ToolRegistry};

    let _registry = common::SyncModelRegistryGuard::mock("m");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    for owner_kind in ["root", "root-traced", "spawned", "inline"] {
        let spawned_context = matches!(owner_kind, "spawned" | "inline");
        for (mode, image_count, location, outcome, changed) in [
            ("session", 2, "first", "error", true),
            ("session", 1, "remote", "error", true),
            ("session", 2, "remote", "error", false),
            ("session_recent(1)", 2, "first", "error", false),
            ("bare", 1, "first", "error", false),
            ("override", 1, "first", "error", false),
            ("session", 1, "foreign", "error", false),
            ("session", 1, "first", "retry", true),
            ("session", 1, "remote", "retry", true),
            ("session", 1, "first", "repeat", true),
            ("session", 1, "provider", "error", false),
        ] {
            let session = Arc::new(Session::open_ephemeral());
            let trace = atman_runtime::event::EventSink::new();
            let turn = TurnId::now();
            let root_run = FlowRunId::now();
            let child_run = FlowRunId::now();
            let inline_run = FlowRunId::now();
            for (run, parent, spawned) in [
                (&root_run, None, false),
                (&child_run, Some(root_run.clone()), true),
                (&inline_run, Some(child_run.clone()), false),
            ] {
                session.sink().emit(Event::FlowStart {
                    run_id: run.clone(),
                    turn_id: Some(turn.clone()),
                    flow_name: "test".into(),
                    parent_run_id: parent,
                    parent_node_id: None,
                    spawned,
                });
            }
            let original: Vec<_> = (0..image_count)
                .map(|index| {
                    let mut message = Message::user_text(turn.clone(), format!("image {index}"));
                    message.parts.push(MessagePart::Image {
                        id: Some(MessagePartId(uuid::Uuid::now_v7())),
                        source: ImageSource {
                            media_type: "image/png".into(),
                            data: ImageData::Path {
                                path: format!("/tmp/image-{index}.png").into(),
                            },
                            detail: Default::default(),
                        },
                    });
                    session.append_message(message.clone(), None);
                    message
                })
                .collect();
            let first_id = original[0].part_id(0, None, 1).unwrap();
            let error = if location == "provider" {
                RuntimeError::ToolFailed("provider request failed".into())
            } else {
                RuntimeError::AttachmentError {
                    reason: "invalid_image".into(),
                    part_id: match location {
                        "first" => Some(first_id),
                        "foreign" => Some(MessagePartId(uuid::Uuid::now_v7())),
                        "remote" => None,
                        _ => unreachable!(),
                    },
                }
            };
            let provider = Arc::new(ScriptedProvider::new(
                "m",
                vec![
                    Err(error.clone()),
                    if outcome == "retry" {
                        Ok("done".into())
                    } else {
                        Err(error)
                    },
                ],
            ));
            let providers = Arc::new(atman_runtime::provider::ProviderRegistry::default());
            providers.register(provider.clone());
            let run = match owner_kind {
                "root" | "root-traced" => &root_run,
                "spawned" => &child_run,
                "inline" => &inline_run,
                _ => unreachable!(),
            };
            let (tx, mut frames) = tokio::sync::broadcast::channel(128);
            let mut ctx = ToolCtx::new()
                .with_session_runtime(session.clone())
                .with_registry(Arc::new(ToolRegistry::default()))
                .with_providers(providers)
                .with_events(session.sink().clone())
                .with_stream_tx(tx)
                .with_anchors(Some(turn.clone()), Some(run.clone()), None);
            if owner_kind == "root-traced" {
                ctx = ctx.with_events(trace.clone());
            }
            let context_id = spawned_context.then(ContextId::now);
            if let Some(id) = &context_id {
                let sink = session.sink().clone().with_context(id.clone());
                sink.emit(Event::ContextCreated {
                    base: None,
                    inheritance: atman_runtime::event::ContextInheritance::Full,
                });
                ctx = ctx
                    .with_context(Arc::new(ContextState::new(Vec::new())))
                    .with_history_segment(HistorySegment::Spawned)
                    .with_events(sink);
                runtime
                    .block_on(atman_runtime::tools::session::SessionPush.call(
                        ToolArgs {
                            positional: vec![Value::List(
                                original.iter().cloned().map(Value::Message).collect(),
                            )],
                            named: vec![],
                        },
                        &ctx,
                    ))
                    .unwrap();
            }
            let mut args = ToolArgs {
                positional: vec![],
                named: vec![
                    ("model".into(), Value::Str("m".into())),
                    ("retry".into(), Value::Int(i64::from(outcome != "error"))),
                ],
            };
            args.named.push(match mode {
                "bare" => ("prompt".into(), Value::Str("unrelated helper".into())),
                "override" => (
                    "messages".into(),
                    Value::List(vec![Value::Message(original[0].clone())]),
                ),
                _ => ("context".into(), Value::Str(mode.into())),
            });
            let result = runtime.block_on(async {
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    atman_runtime::tools::llm_call::LlmCallTool.call(args, &ctx),
                )
                .await
                .expect("LLM request did not release its context lock")
            });
            assert_eq!(
                result.is_ok(),
                outcome == "retry",
                "{owner_kind}/{mode}/{location}/{outcome}: {result:?}"
            );
            assert_eq!(
                provider.call_count(),
                if outcome == "error" { 1 } else { 2 }
            );
            let owner_messages = ctx
                .context()
                .unwrap()
                .messages_handle()
                .lock()
                .unwrap()
                .clone();
            let remaining = owner_messages
                .iter()
                .flat_map(|m| &m.parts)
                .filter(|p| matches!(p, MessagePart::Image { .. }))
                .count();
            assert_eq!(
                remaining,
                image_count - usize::from(changed),
                "{owner_kind}/{mode}/{location}/{outcome}"
            );
            if changed {
                assert!(owner_messages.iter().any(|m| {
                    m.text_concat()
                        .contains("attachment unavailable: image-0.png")
                }));
            }
            if spawned_context {
                assert_eq!(*session.messages_handle().lock().unwrap(), original);
                assert_eq!(session.messages().as_ref(), original.as_slice());
            }
            let events = session.sink().snapshot_envelopes();
            assert!(trace.snapshot().iter().all(|event| {
                event.context_message().is_none()
                    && !matches!(event, Event::AttachmentDegraded { .. })
            }));
            if owner_kind == "root-traced" {
                assert!(
                    trace
                        .snapshot()
                        .iter()
                        .any(|event| matches!(event, Event::LlmCall { .. }))
                );
            }
            let patches: Vec<_> = events
                .iter()
                .filter(|e| matches!(e.event, Event::AttachmentDegraded { .. }))
                .collect();
            assert_eq!(patches.len(), usize::from(changed));
            if let Some(envelope) = patches.first() {
                assert_eq!(envelope.context_id, context_id);
                assert!(
                    matches!(&envelope.event, Event::AttachmentDegraded { flow_run_id: Some(actual), patch, .. }
                    if actual == run && patch.target == (atman_runtime::message::AttachmentTarget::Part { part_id: first_id }))
                );
            }
            let base = match context_id {
                Some(context_id) => ContextBase::Context {
                    context_id,
                    through_seq: session.sink().published_seq(),
                },
                None => ContextBase::LegacyRoot {
                    through_seq: session.sink().published_seq(),
                },
            };
            let selected =
                atman_runtime::projection::context::replay_context(&events, &base).unwrap();
            assert_eq!(
                selected
                    .window()
                    .iter()
                    .map(|(_, m)| m.clone())
                    .collect::<Vec<_>>(),
                owner_messages
            );
            if outcome != "error" {
                let requests = provider.requests.lock().unwrap();
                assert!(
                    !requests[1]
                        .iter()
                        .flat_map(|m| &m.parts)
                        .any(|p| matches!(p, MessagePart::Image { .. }))
                );
                assert!(requests[1].iter().any(|m| {
                    m.text_concat()
                        .contains("attachment unavailable: image-0.png")
                }));
            }
            let mut attachment_notes = 0;
            while let Ok(frame) = frames.try_recv() {
                if let atman_runtime::stream::StreamFrame::Notification(note) = frame
                    && note.message.starts_with("attachment rejected")
                {
                    assert_eq!(note.run_id.as_deref(), Some(run.to_string().as_str()));
                    attachment_notes += 1;
                }
            }
            assert_eq!(attachment_notes, usize::from(changed));
        }
    }
}

fn run_with(provider: Arc<ScriptedProvider>, src: &str) -> (Result<Value, RuntimeError>, usize) {
    let file = parse_file(src).unwrap();
    let ex = Executor::new();
    let counter = provider.clone();
    ex.providers.register(provider);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(ex.run(&file, "t", vec![]));
    (result, counter.call_count())
}

#[test]
fn retry_classified_only_retries_on_listed_kinds() {
    let provider = Arc::new(ScriptedProvider::new(
        "m",
        vec![
            Err(RuntimeError::ToolFailed("openai: request timed out".into())),
            Ok("recovered after timeout".into()),
        ],
    ));
    let src = r#"flow t() -> string {
    return llm.call(
        model: "m",
        prompt: "hi",
        retry: 3,
        retry_classified: ["timeout", "rate_limit"],
    )
}
"#;
    let (value, calls) = run_with(provider, src);
    match value.unwrap() {
        Value::Str(s) => assert!(s.contains("recovered")),
        other => panic!("expected str got {other:?}"),
    }
    assert_eq!(calls, 2, "should retry once then succeed");
}

#[test]
fn retry_classified_gives_up_immediately_on_kind_not_in_list() {
    let provider = Arc::new(ScriptedProvider::new(
        "m",
        vec![
            Err(RuntimeError::ToolFailed(
                "openai http 401: unauthorized".into(),
            )),
            Ok("should not reach here".into()),
        ],
    ));
    let src = r#"flow t() -> string {
    return llm.call(
        model: "m",
        prompt: "hi",
        retry: 3,
        retry_classified: ["timeout", "rate_limit"],
    )
}
"#;
    let (value, calls) = run_with(provider, src);
    match value {
        Err(RuntimeError::ToolFailed(msg)) => assert!(msg.contains("401"), "msg: {msg}"),
        other => panic!("expected auth_failed err, got {other:?}"),
    }
    assert_eq!(calls, 1, "auth_failed not in list — must not retry");
}

#[test]
fn retry_without_classified_retries_any_error() {
    let _registry = common::SyncModelRegistryGuard::mock("m");
    let provider = Arc::new(ScriptedProvider::new(
        "m",
        vec![
            Err(RuntimeError::ToolFailed(
                "openai http 401: unauthorized".into(),
            )),
            Ok("still tried again".into()),
        ],
    ));
    let src = r#"flow t() -> string {
    return llm.call(
        model: "m",
        context: "session",
        prompt: "hi",
        retry: 3,
    )
}
"#;
    let session = Arc::new(Session::open_ephemeral());
    let turn_id = session.begin_turn(Message::user_text(
        atman_runtime::event::TurnId::now(),
        "task",
    ));
    let steering_id = session.enqueue_injection("retry constraint").unwrap();
    let executor = Executor::with_events(session.sink().clone());
    executor.providers.register(provider.clone());
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let value = runtime.block_on(executor.run_in_turn(
        &parse_file(src).unwrap(),
        "t",
        vec![],
        Some(turn_id.clone()),
        Some(session.clone()),
    ));
    session.end_turn(&turn_id);
    match value.unwrap() {
        Value::Message(message) => assert!(message.text_concat().contains("still tried again")),
        other => panic!("expected message got {other:?}"),
    }
    assert_eq!(
        provider.call_count(),
        2,
        "without retry_classified, any err retries"
    );
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests[0], requests[1]);
    assert_eq!(
        requests[0]
            .iter()
            .filter(|message| message.text_concat().contains(&steering_id.to_string()))
            .count(),
        1
    );
    assert_eq!(
        session
            .messages()
            .iter()
            .filter(|message| message.text_concat().contains(&steering_id.to_string()))
            .count(),
        1
    );
}

#[test]
fn context_overflow_compacts_and_resends_without_normal_retries() {
    struct OverflowProvider {
        calls: std::sync::Arc<AtomicUsize>,
        summary_calls: std::sync::Arc<AtomicUsize>,
        request_tokens: std::sync::Arc<std::sync::Mutex<Vec<u64>>>,
        requests: std::sync::Arc<std::sync::Mutex<Vec<Vec<Message>>>>,
    }

    impl Provider for OverflowProvider {
        fn name(&self) -> &str {
            "m"
        }

        fn call<'a>(
            &'a self,
            req: LlmRequest,
        ) -> BoxFut<'a, Result<AssistantMessage, RuntimeError>> {
            Box::pin(async move {
                let is_summary = req.messages.len() == 1
                    && (req.system.is_some()
                        || req.messages[0].text_concat().starts_with(
                            "Summarize this prior turn's assistant/system/tool output",
                        ));
                if !is_summary {
                    self.request_tokens.lock().unwrap().push(
                        atman_runtime::compaction::estimate_tokens_for_messages(&req.messages),
                    );
                    self.requests.lock().unwrap().push(req.messages.clone());
                }
                if is_summary {
                    self.summary_calls.fetch_add(1, Ordering::SeqCst);
                    return Ok(AssistantMessage {
                        message: atman_runtime::message::Message::assistant_text(
                            req.messages
                                .first()
                                .map(|m| m.turn_id.clone())
                                .unwrap_or_else(atman_runtime::event::TurnId::now),
                            "summary after overflow",
                        ),
                        stop_reason: StopReason::End,
                        token_usage: TokenUsage::default(),
                        timing: atman_runtime::provider::CallTiming::default(),
                        model: String::new(),
                        response_id: None,
                    });
                }
                match self.calls.fetch_add(1, Ordering::SeqCst) {
                    0 => Err(RuntimeError::ToolFailed(
                        "openai http 400: maximum context length is 1048565 tokens".into(),
                    )),
                    1 => Ok(AssistantMessage {
                        message: atman_runtime::message::Message::assistant_text(
                            req.messages
                                .first()
                                .map(|m| m.turn_id.clone())
                                .unwrap_or_else(atman_runtime::event::TurnId::now),
                            "recovered with compacted history",
                        ),
                        stop_reason: StopReason::End,
                        token_usage: TokenUsage::default(),
                        timing: atman_runtime::provider::CallTiming::default(),
                        model: String::new(),
                        response_id: None,
                    }),
                    _ => Err(RuntimeError::ToolFailed("scripted: exhausted".into())),
                }
            })
        }

        fn call_streaming(&self, req: LlmRequest) -> Observable<AssistantMessage> {
            use tokio::sync::broadcast;
            use tokio_util::sync::CancellationToken;
            let (tx, events) = broadcast::channel(16);
            let cancel = CancellationToken::new();
            let calls = self.calls.clone();
            let summary_calls = self.summary_calls.clone();
            let request_tokens = self.request_tokens.clone();
            let requests = self.requests.clone();
            let output: BoxFut<'static, Result<AssistantMessage, RuntimeError>> =
                Box::pin(async move {
                    let is_summary = req.messages.len() == 1
                        && (req.system.is_some()
                            || req.messages[0].text_concat().starts_with(
                                "Summarize this prior turn's assistant/system/tool output",
                            ));
                    if !is_summary {
                        request_tokens.lock().unwrap().push(
                            atman_runtime::compaction::estimate_tokens_for_messages(&req.messages),
                        );
                        requests.lock().unwrap().push(req.messages.clone());
                    }
                    let result = if is_summary {
                        summary_calls.fetch_add(1, Ordering::SeqCst);
                        Ok(AssistantMessage {
                            message: atman_runtime::message::Message::assistant_text(
                                req.messages
                                    .first()
                                    .map(|m| m.turn_id.clone())
                                    .unwrap_or_else(atman_runtime::event::TurnId::now),
                                "summary after overflow",
                            ),
                            stop_reason: StopReason::End,
                            token_usage: TokenUsage::default(),
                            timing: atman_runtime::provider::CallTiming::default(),
                            model: String::new(),
                            response_id: None,
                        })
                    } else {
                        match calls.fetch_add(1, Ordering::SeqCst) {
                            0 => Err(RuntimeError::ToolFailed(
                                "openai http 400: maximum context length is 1048565 tokens".into(),
                            )),
                            1 => Ok(AssistantMessage {
                                message: atman_runtime::message::Message::assistant_text(
                                    req.messages
                                        .first()
                                        .map(|m| m.turn_id.clone())
                                        .unwrap_or_else(atman_runtime::event::TurnId::now),
                                    "recovered with compacted history",
                                ),
                                stop_reason: StopReason::End,
                                token_usage: TokenUsage::default(),
                                timing: atman_runtime::provider::CallTiming::default(),
                                model: String::new(),
                                response_id: None,
                            }),
                            _ => Err(RuntimeError::ToolFailed("scripted: exhausted".into())),
                        }
                    };
                    let _ = tx.send(NodeEvent::LlmDone { total_tokens: 0 });
                    result
                });
            Observable {
                output,
                events,
                cancel,
            }
        }
    }

    let _registry = common::SyncModelRegistryGuard::mock("m");
    let provider = Arc::new(OverflowProvider {
        calls: std::sync::Arc::new(AtomicUsize::new(0)),
        summary_calls: std::sync::Arc::new(AtomicUsize::new(0)),
        request_tokens: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        requests: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
    });
    let session = std::sync::Arc::new(Session::open_ephemeral());
    build_long_history(&session, 30);
    let file = parse_file(
        r#"flow t() -> string {
    return llm.call(
        model: "m",
        context: "session",
        prompt: "continue",
        retry: 10,
    )
}
"#,
    )
    .unwrap();
    let ex = Executor::with_events(session.sink().clone());
    ex.providers.register(provider.clone());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let turn_id = atman_runtime::event::TurnId::now();
    session.begin_turn(Message::user_text(turn_id.clone(), "run"));
    let steering_id = session
        .enqueue_injection("preserve the current constraint")
        .unwrap();
    let result = rt.block_on(ex.run_in_turn(
        &file,
        "t",
        vec![],
        Some(turn_id.clone()),
        Some(session.clone()),
    ));
    session.end_turn(&turn_id);

    match result.unwrap() {
        Value::Str(s) => assert!(s.contains("recovered"), "got {s}"),
        Value::Message(message) => {
            assert!(message.text_concat().contains("recovered"));
            assert_eq!(message.turn_id, turn_id);
        }
        other => panic!("expected LLM response got {other:?}"),
    }
    assert!(session.sink().snapshot().iter().any(|event| {
        matches!(
            event,
            atman_runtime::Event::AssistantMsg {
                turn_id: event_turn,
                flow_run_id: Some(_),
                message,
            } if *event_turn == turn_id && message.text_concat().contains("recovered")
        )
    }));
    assert!(provider.summary_calls.load(Ordering::SeqCst) >= 1);
    assert!(provider.calls.load(Ordering::SeqCst) >= 2);
    let tokens = provider.request_tokens.lock().unwrap().clone();
    assert!(tokens.len() >= 2);
    assert!(tokens.last().copied().unwrap_or(0) <= tokens[0]);
    let requests = provider.requests.lock().unwrap().clone();
    assert!(requests.len() >= 2);
    for request in &requests {
        assert_eq!(
            request
                .iter()
                .filter(|message| message.text_concat().contains(&steering_id.to_string()))
                .count(),
            1,
            "steering must survive overflow without a duplicate retry suffix"
        );
        assert_eq!(
            request
                .iter()
                .filter(|m| m.text_concat() == "continue")
                .count(),
            1,
            "current prompt must appear exactly once: {:?}",
            request.iter().map(|m| m.text_concat()).collect::<Vec<_>>()
        );
        let user_turns = request
            .iter()
            .filter(|m| m.role == atman_runtime::message::MessageRole::User)
            .map(|m| m.turn_id.clone())
            .collect::<std::collections::HashSet<_>>();
        assert!(user_turns.len() <= 5, "request inherited {user_turns:?}");
    }
}

#[test]
fn retry_classified_unknown_kind_fails_parse_time() {
    let file = parse_file(
        r#"flow t() -> string {
    return llm.call(
        model: "m",
        prompt: "hi",
        retry: 1,
        retry_classified: ["not_a_real_kind"],
    )
}
"#,
    )
    .unwrap();
    let ex = Executor::new();
    ex.providers.register(Arc::new(
        MockProvider::new("m").with_model("m", Value::Str("unused".into())),
    ));
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(ex.run(&file, "t", vec![]));
    match result {
        Err(RuntimeError::ToolFailed(msg)) => {
            assert!(msg.contains("not_a_real_kind"), "msg: {msg}");
        }
        other => panic!("expected ToolFailed err with kind name, got {other:?}"),
    }
}
