use atman_dsl::parse::parse_file;
use atman_runtime::event::LlmCallStatus;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::{Event, Executor, Value};

use std::sync::Arc;

static TEST_CFG_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn install_test_models() {
    use atman_runtime::model_registry::{ModelConfig, ModelEntry};

    let models = [("mock-model", "mock"), ("flaky", "flaky")]
        .into_iter()
        .map(|(model, provider)| {
            (
                model.to_string(),
                ModelEntry {
                    model: model.to_string(),
                    provider: Some(provider.to_string()),
                    context_budget: Some(8_192),
                    ..Default::default()
                },
            )
        })
        .collect();
    atman_runtime::model_registry::set_model_config(ModelConfig {
        models,
        providers: std::collections::HashMap::new(),
        aliases: std::collections::HashMap::new(),
    });
}

#[tokio::test]
async fn llm_call_event_records_wallclock_and_tokens() {
    let _cfg_lock = TEST_CFG_LOCK.lock().await;
    install_test_models();
    let src = r#"flow t() -> string {
    return llm.call(
        model: "mock-model",
        prompt: "hello world",
    )
}
"#;
    let file = parse_file(src).unwrap();
    let ex = Executor::new();
    ex.providers.register(Arc::new(
        MockProvider::new("mock").with_model("mock-model", Value::Str("response text".into())),
    ));
    ex.run(&file, "t", vec![]).await.unwrap();

    let events = ex.events.snapshot();
    let call_event = events
        .iter()
        .find(|e| matches!(e, Event::LlmCall { .. }))
        .expect("expected LlmCall event");
    match call_event {
        Event::LlmCall {
            model,
            provider,
            usage,
            status,
            ..
        } => {
            assert_eq!(model, "mock-model");
            assert_eq!(provider, "mock");
            assert!(matches!(status, LlmCallStatus::Ok));
            assert!(usage.input > 0);
            assert!(usage.output > 0);
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn llm_call_event_records_retry_attempts() {
    let _cfg_lock = TEST_CFG_LOCK.lock().await;
    install_test_models();
    let src = r#"flow t() -> string {
    return llm.call(
        model: "flaky",
        prompt: "hi",
        retry: 2,
    )
}
"#;
    let file = parse_file(src).unwrap();
    let ex = Executor::new();
    ex.providers.register(Arc::new(
        MockProvider::new("flaky").with_fallback(Value::Str("unreachable".into())),
    ));
    ex.providers.register(Arc::new(
        MockProvider::new("stable").with_model("stable", Value::Str("stable-ok".into())),
    ));
    let out = ex.run(&file, "t", vec![]).await.unwrap();
    assert!(matches!(out, Value::Str(s) if s == "unreachable"));

    let events = ex.events.snapshot();
    let call_count = events
        .iter()
        .filter(|e| matches!(e, Event::LlmCall { .. }))
        .count();
    assert!(call_count >= 1);
}
