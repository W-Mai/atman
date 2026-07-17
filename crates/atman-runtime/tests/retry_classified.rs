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
}

impl ScriptedProvider {
    fn new(name: &str, outcomes: Vec<Result<String, RuntimeError>>) -> Self {
        Self {
            name: name.to_string(),
            outcomes,
            calls: AtomicUsize::new(0),
            request_tokens: std::sync::Mutex::new(Vec::new()),
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

fn build_long_history(session: &Session, msg_count: usize) {
    let base = "x".repeat(4000);
    for i in 0..msg_count {
        let turn = atman_runtime::event::TurnId::now();
        let msg = if i % 2 == 0 {
            Message::user_text(turn, format!("{base} user {i}"))
        } else {
            Message::assistant_text(turn, format!("{base} assistant {i}"))
        };
        session.append_message(msg, None);
    }
}

fn run_with(provider: Arc<ScriptedProvider>, src: &str) -> (Result<Value, RuntimeError>, usize) {
    let file = parse_file(src).unwrap();
    let mut ex = Executor::new();
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
    return llm {
        model: "m"
        prompt: "hi"
        retry: 3
        retry_classified: [timeout, rate_limit]
    }
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
    return llm {
        model: "m"
        prompt: "hi"
        retry: 3
        retry_classified: [timeout, rate_limit]
    }
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
    return llm {
        model: "m"
        prompt: "hi"
        retry: 3
    }
}
"#;
    let (value, calls) = run_with(provider, src);
    match value.unwrap() {
        Value::Str(s) => assert!(s.contains("still tried again")),
        other => panic!("expected str got {other:?}"),
    }
    assert_eq!(calls, 2, "without retry_classified, any err retries");
}

#[test]
fn context_overflow_compacts_and_resends_without_normal_retries() {
    struct OverflowProvider {
        calls: std::sync::Arc<AtomicUsize>,
        summary_calls: std::sync::Arc<AtomicUsize>,
        request_tokens: std::sync::Arc<std::sync::Mutex<Vec<u64>>>,
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
                self.request_tokens.lock().unwrap().push(
                    atman_runtime::compaction::estimate_tokens_for_messages(&req.messages),
                );
                if req.system.is_some() && req.messages.len() == 1 {
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
            let output: BoxFut<'static, Result<AssistantMessage, RuntimeError>> =
                Box::pin(async move {
                    request_tokens.lock().unwrap().push(
                        atman_runtime::compaction::estimate_tokens_for_messages(&req.messages),
                    );
                    let result = if req.system.is_some() && req.messages.len() == 1 {
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

    let provider = Arc::new(OverflowProvider {
        calls: std::sync::Arc::new(AtomicUsize::new(0)),
        summary_calls: std::sync::Arc::new(AtomicUsize::new(0)),
        request_tokens: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
    });
    let session = std::sync::Arc::new(Session::open_ephemeral());
    build_long_history(&session, 30);
    let file = parse_file(
        r#"flow t() -> string {
    return llm {
        model: "llama-3b"
        context: session
        prompt: "continue"
        retry: 10
    }
}
"#,
    )
    .unwrap();
    let mut ex = Executor::with_events(session.sink().clone());
    ex.providers.register(provider.clone());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let turn_id = atman_runtime::event::TurnId::now();
    session.begin_turn(Message::user_text(turn_id.clone(), "run"));
    let result =
        rt.block_on(ex.run_in_turn(&file, "t", vec![], Some(turn_id), Some(session.clone())));
    session.end_turn();

    match result.unwrap() {
        Value::Str(s) => assert!(s.contains("recovered"), "got {s}"),
        other => panic!("expected str got {other:?}"),
    }
    assert!(provider.summary_calls.load(Ordering::SeqCst) >= 1);
    assert!(provider.calls.load(Ordering::SeqCst) >= 2);
    let tokens = provider.request_tokens.lock().unwrap().clone();
    assert!(tokens.len() >= 2);
    assert!(tokens.last().copied().unwrap_or(0) < tokens[0]);
}

#[test]
fn retry_classified_unknown_kind_fails_parse_time() {
    let file = parse_file(
        r#"flow t() -> string {
    return llm {
        model: "m"
        prompt: "hi"
        retry: 1
        retry_classified: [not_a_real_kind]
    }
}
"#,
    )
    .unwrap();
    let mut ex = Executor::new();
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
