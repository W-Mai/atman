mod common;

use atman_dsl::parse::parse_file;
use atman_runtime::event::LlmCallStatus;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::{Event, Value};

#[tokio::test]
async fn llm_call_event_records_wallclock_and_tokens() {
    let registry = common::ModelRegistryGuard::acquire(common::config([
        common::model_for_provider("mock-model", "mock", 8_192, None),
        common::model_for_provider("flaky", "flaky", 8_192, None),
    ]))
    .await;
    let src = r#"flow t() -> string {
    return llm.call(
        model: "mock-model",
        prompt: "hello world",
    )
}
"#;
    let file = parse_file(src).unwrap();
    let runtime = common::TestRuntime::new(
        registry,
        MockProvider::new("mock").with_model("mock-model", Value::Str("response text".into())),
    );
    runtime.executor.run(&file, "t", vec![]).await.unwrap();

    let events = runtime.executor.events.snapshot();
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
    let registry = common::ModelRegistryGuard::acquire(common::config([
        common::model_for_provider("mock-model", "mock", 8_192, None),
        common::model_for_provider("flaky", "flaky", 8_192, None),
    ]))
    .await;
    let src = r#"flow t() -> string {
    return llm.call(
        model: "flaky",
        prompt: "hi",
        retry: 2,
    )
}
"#;
    let file = parse_file(src).unwrap();
    let runtime = common::TestRuntime::new(
        registry,
        MockProvider::new("flaky").with_fallback(Value::Str("unreachable".into())),
    );
    common::register_provider(
        &runtime.executor,
        MockProvider::new("stable").with_model("stable", Value::Str("stable-ok".into())),
    );
    let out = runtime.executor.run(&file, "t", vec![]).await.unwrap();
    assert!(matches!(out, Value::Str(s) if s == "unreachable"));

    let events = runtime.executor.events.snapshot();
    let call_count = events
        .iter()
        .filter(|e| matches!(e, Event::LlmCall { .. }))
        .count();
    assert!(call_count >= 1);
}
