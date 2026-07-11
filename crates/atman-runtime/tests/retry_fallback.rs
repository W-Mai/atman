use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use atman_dsl::parse::parse_file;
use atman_runtime::Executor;
use atman_runtime::error::RuntimeError;
use atman_runtime::event::{NodeEvent, Observable};
use atman_runtime::message::{Message, MessagePart, MessageRole};
use atman_runtime::provider::{
    AssistantMessage, DEFAULT_STREAM_BUFFER, LlmRequest, Provider, StopReason, TokenUsage,
};
use atman_runtime::tool::BoxFut;
use atman_runtime::value::Value;

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

struct FlakyProvider {
    name: String,
    fail_first_n: AtomicU32,
    good: String,
}

impl FlakyProvider {
    fn new(name: &str, fail_n: u32, good: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            fail_first_n: AtomicU32::new(fail_n),
            good: good.into(),
        }
    }

    fn assistant(&self) -> AssistantMessage {
        AssistantMessage {
            message: Message {
                role: MessageRole::Assistant,
                parts: vec![MessagePart::Text {
                    text: self.good.clone(),
                }],
                turn_id: atman_runtime::event::TurnId::now(),
            },
            stop_reason: StopReason::End,
            token_usage: TokenUsage::default(),
        }
    }
}

impl Provider for FlakyProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn call<'a>(&'a self, _req: LlmRequest) -> BoxFut<'a, Result<AssistantMessage, RuntimeError>> {
        Box::pin(async move {
            let remaining = self.fail_first_n.load(Ordering::SeqCst);
            if remaining > 0 {
                self.fail_first_n.fetch_sub(1, Ordering::SeqCst);
                return Err(RuntimeError::ToolFailed("connection reset by peer".into()));
            }
            Ok(self.assistant())
        })
    }

    fn call_streaming(&self, req: LlmRequest) -> Observable<AssistantMessage> {
        let (tx, events) = broadcast::channel(DEFAULT_STREAM_BUFFER);
        let cancel = CancellationToken::new();
        let remaining = self.fail_first_n.load(Ordering::SeqCst);
        if remaining > 0 {
            self.fail_first_n.fetch_sub(1, Ordering::SeqCst);
        }
        let should_fail = remaining > 0;
        let msg = self.assistant();
        let _ = req;
        let output: BoxFut<'static, Result<AssistantMessage, RuntimeError>> =
            Box::pin(async move {
                let _ = tx.send(NodeEvent::LlmDone { total_tokens: 0 });
                if should_fail {
                    Err(RuntimeError::ToolFailed("connection reset by peer".into()))
                } else {
                    Ok(msg)
                }
            });
        Observable {
            output,
            events,
            cancel,
        }
    }
}

#[tokio::test]
async fn retry_recovers_after_flakes() {
    let src = r#"flow t() -> string {
    primary = llm {
        model: "flaky"
        prompt: "hi"
        retry: 2
    }
    return primary
}
"#;
    let file = parse_file(src).unwrap();
    let mut ex = Executor::new();
    ex.providers
        .register(Arc::new(FlakyProvider::new("flaky", 2, "ok")));
    let out = ex.run(&file, "t", vec![]).await.unwrap();
    assert!(matches!(out, Value::Str(s) if s == "ok"));
}

#[tokio::test]
async fn retry_exhausted_falls_back_to_alternate_llm() {
    let src = r#"flow t() -> string {
    primary = llm {
        model: "flaky"
        prompt: "hi"
        retry: 1
        fallback: llm {
            model: "stable"
            prompt: "hi"
        }
    }
    return primary
}
"#;
    let file = parse_file(src).unwrap();
    let mut ex = Executor::new();
    ex.providers
        .register(Arc::new(FlakyProvider::new("flaky", 5, "unreachable")));
    ex.providers
        .register(Arc::new(FlakyProvider::new("stable", 0, "fallback-ok")));
    let out = ex.run(&file, "t", vec![]).await.unwrap();
    assert!(matches!(out, Value::Str(s) if s == "fallback-ok"));
}

#[tokio::test]
async fn retry_exhausted_without_fallback_returns_err() {
    let src = r#"flow t() -> string {
    primary = llm {
        model: "flaky"
        prompt: "hi"
        retry: 1
    }
    return primary
}
"#;
    let file = parse_file(src).unwrap();
    let mut ex = Executor::new();
    ex.providers
        .register(Arc::new(FlakyProvider::new("flaky", 5, "x")));
    let err = ex.run(&file, "t", vec![]).await.unwrap_err();
    assert!(matches!(err, RuntimeError::ToolFailed(msg) if msg.contains("connection reset")));
}
