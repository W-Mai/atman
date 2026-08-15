use std::sync::Arc;

use atman_dsl::parse::parse_file;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::{Executor, RuntimeError, Value};

static TEST_CFG_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn install_mock_model() {
    use atman_runtime::model_registry::{ModelConfig, ModelEntry};

    atman_runtime::model_registry::set_model_config(ModelConfig {
        models: [(
            "mock-model".into(),
            ModelEntry {
                model: "mock-model".into(),
                provider: Some("mock".into()),
                context_budget: Some(8_192),
                ..Default::default()
            },
        )]
        .into_iter()
        .collect(),
        providers: std::collections::HashMap::new(),
        aliases: std::collections::HashMap::new(),
    });
}

#[tokio::test]
async fn watch_token_abort_stops_flow_when_forbidden_pattern_appears() {
    let _cfg_lock = TEST_CFG_LOCK.lock().await;
    install_mock_model();
    let src = r#"flow review() -> string {
    primary = llm.call(
        model: "mock-model",
        prompt: "review please",
    )
    watch primary {
        on token(match: "as any" | "@ts-ignore") {
            abort("type-safety violation")
        }
    }
    return primary
}
"#;
    let file = parse_file(src).unwrap();
    let ex = Executor::new();
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
    let _cfg_lock = TEST_CFG_LOCK.lock().await;
    install_mock_model();
    let src = r#"flow review() -> string {
    primary = llm.call(
        model: "mock-model",
        prompt: "review please",
    )
    watch primary {
        on token(match: "as any") {
            abort("type-safety")
        }
    }
    return primary
}
"#;
    let file = parse_file(src).unwrap();
    let ex = Executor::new();
    ex.providers
        .register(Arc::new(MockProvider::new("mock").with_model(
            "mock-model",
            Value::Str("Looks good, typed correctly throughout.".into()),
        )));

    let out = ex.run(&file, "review", vec![]).await.unwrap();
    assert!(matches!(out, Value::Str(s) if s == "Looks good, typed correctly throughout."));
}
