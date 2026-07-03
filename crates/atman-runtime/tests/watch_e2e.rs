use std::sync::Arc;

use atman_dsl::parse::parse_file;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::{Executor, RuntimeError, Value};

#[tokio::test]
async fn watch_token_abort_stops_flow_when_forbidden_pattern_appears() {
    let src = r#"flow review() -> string {
    primary = llm {
        model: "mock-model"
        prompt: "review please"
    }
    watch primary {
        on token(match: "as any" | "@ts-ignore") {
            abort("type-safety violation")
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
            Value::Str("Let's just cast this as any, quick fix".into()),
        )));

    let err = ex.run(&file, "review", vec![]).await.unwrap_err();
    match err {
        RuntimeError::Aborted(msg) => assert!(msg.contains("as any")),
        other => panic!("expected Aborted, got {other:?}"),
    }
}

#[tokio::test]
async fn watch_token_does_not_fire_on_clean_output() {
    let src = r#"flow review() -> string {
    primary = llm {
        model: "mock-model"
        prompt: "review please"
    }
    watch primary {
        on token(match: "as any") {
            abort("type-safety")
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
            Value::Str("Looks good, typed correctly throughout.".into()),
        )));

    let out = ex.run(&file, "review", vec![]).await.unwrap();
    assert!(matches!(out, Value::Str(s) if s == "Looks good, typed correctly throughout."));
}
