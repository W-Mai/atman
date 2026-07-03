use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use atman_dsl::parse::parse_file;
use atman_runtime::Executor;
use atman_runtime::error::RuntimeError;
use atman_runtime::event::{NodeEvent, Observable};
use atman_runtime::provider::{DEFAULT_STREAM_BUFFER, LlmRequest, Provider};
use atman_runtime::tool::BoxFut;
use atman_runtime::value::Value;

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

struct FlakyProvider {
    name: String,
    fail_first_n: AtomicU32,
    good: Value,
}

impl FlakyProvider {
    fn new(name: &str, fail_n: u32, good: Value) -> Self {
        Self {
            name: name.into(),
            fail_first_n: AtomicU32::new(fail_n),
            good,
        }
    }
}

impl Provider for FlakyProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn call<'a>(&'a self, _req: LlmRequest) -> BoxFut<'a, Result<Value, RuntimeError>> {
        Box::pin(async move {
            let remaining = self.fail_first_n.load(Ordering::SeqCst);
            if remaining > 0 {
                self.fail_first_n.fetch_sub(1, Ordering::SeqCst);
                return Err(RuntimeError::ToolFailed("simulated flake".into()));
            }
            Ok(self.good.clone())
        })
    }

    fn call_streaming(&self, req: LlmRequest) -> Observable<Value> {
        let (tx, events) = broadcast::channel(DEFAULT_STREAM_BUFFER);
        let cancel = CancellationToken::new();
        let good = self.good.clone();
        let remaining = self.fail_first_n.load(Ordering::SeqCst);
        if remaining > 0 {
            self.fail_first_n.fetch_sub(1, Ordering::SeqCst);
        }
        let should_fail = remaining > 0;
        let _ = req;
        let output: BoxFut<'static, Result<Value, RuntimeError>> = Box::pin(async move {
            let _ = tx.send(NodeEvent::LlmDone { total_tokens: 0 });
            if should_fail {
                Err(RuntimeError::ToolFailed("simulated flake".into()))
            } else {
                Ok(good)
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
    ex.providers.register(Arc::new(FlakyProvider::new(
        "flaky",
        2,
        Value::Str("ok".into()),
    )));
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
    ex.providers.register(Arc::new(FlakyProvider::new(
        "flaky",
        5,
        Value::Str("unreachable".into()),
    )));
    ex.providers.register(Arc::new(FlakyProvider::new(
        "stable",
        0,
        Value::Str("fallback-ok".into()),
    )));
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
    ex.providers.register(Arc::new(FlakyProvider::new(
        "flaky",
        5,
        Value::Str("x".into()),
    )));
    let err = ex.run(&file, "t", vec![]).await.unwrap_err();
    assert!(matches!(err, RuntimeError::ToolFailed(msg) if msg.contains("simulated flake")));
}
