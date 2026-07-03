use std::sync::Arc;
use std::time::Duration;

use atman_dsl::parse::parse_file;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::{Executor, RuntimeError, Value};

#[tokio::test]
async fn watch_tokens_consumed_aborts_when_exceeded() {
    let src = r#"flow review() -> string {
    primary = llm {
        model: "mock-model"
        prompt: "review please"
    }
    watch primary {
        on tokens_consumed(> 5) {
            abort("token budget exceeded")
        }
    }
    return primary
}
"#;
    let file = parse_file(src).unwrap();
    let mut ex = Executor::new();
    ex.providers
        .register(Arc::new(MockProvider::new("mock").with_model(
            "mock-model",
            Value::Str(
                "this is a fairly long response that will exceed five tokens once split".into(),
            ),
        )));

    let err = ex.run(&file, "review", vec![]).await.unwrap_err();
    match err {
        RuntimeError::Aborted(msg) => assert!(msg.contains("tokens_consumed")),
        other => panic!("expected Aborted, got {other:?}"),
    }
}

#[tokio::test]
async fn watch_elapsed_aborts_slow_stream() {
    let src = r#"flow review() -> string {
    primary = llm {
        model: "mock-model"
        prompt: "review please"
    }
    watch primary {
        on elapsed(> 50 ms) {
            abort("too slow")
        }
    }
    return primary
}
"#;
    let file = parse_file(src).unwrap();
    let mut ex = Executor::new();
    ex.providers.register(Arc::new(
        MockProvider::new("mock")
            .with_model(
                "mock-model",
                Value::Str("this response streams slowly across many chunks".into()),
            )
            .with_chunk_delay(Duration::from_millis(80)),
    ));

    let err = ex.run(&file, "review", vec![]).await.unwrap_err();
    match err {
        RuntimeError::Aborted(msg) => assert!(msg.contains("elapsed")),
        other => panic!("expected Aborted, got {other:?}"),
    }
}

#[tokio::test]
async fn watch_tokens_consumed_does_not_fire_when_under_budget() {
    let src = r#"flow review() -> string {
    primary = llm {
        model: "mock-model"
        prompt: "review please"
    }
    watch primary {
        on tokens_consumed(> 10000) {
            abort("budget")
        }
    }
    return primary
}
"#;
    let file = parse_file(src).unwrap();
    let mut ex = Executor::new();
    ex.providers.register(Arc::new(
        MockProvider::new("mock").with_model("mock-model", Value::Str("short reply".into())),
    ));

    let out = ex.run(&file, "review", vec![]).await.unwrap();
    assert!(matches!(out, Value::Str(s) if s == "short reply"));
}
