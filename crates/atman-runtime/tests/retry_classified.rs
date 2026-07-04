use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use atman_dsl::parse::parse_file;
use atman_runtime::error::RuntimeError;
use atman_runtime::event::{NodeEvent, Observable};
use atman_runtime::provider::{AssistantMessage, LlmRequest, Provider, StopReason, TokenUsage};
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::tool::BoxFut;
use atman_runtime::{Executor, Value};

struct ScriptedProvider {
    name: String,
    outcomes: Vec<Result<String, RuntimeError>>,
    calls: AtomicUsize,
}

impl ScriptedProvider {
    fn new(name: &str, outcomes: Vec<Result<String, RuntimeError>>) -> Self {
        Self {
            name: name.to_string(),
            outcomes,
            calls: AtomicUsize::new(0),
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
