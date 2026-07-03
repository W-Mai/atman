use atman_runtime::event::NodeEvent;
use atman_runtime::provider::{LlmRequest, Provider};
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::value::Value;

#[tokio::test]
async fn mock_streaming_emits_chunks_then_done() {
    let provider = MockProvider::new("mock")
        .with_model("test-model", Value::Str("hello streaming world".into()));
    let mut obs = provider.call_streaming(LlmRequest {
        model: "test-model".to_string(),
        messages: vec![atman_runtime::provider::user_text_message("hi")],
        system: None,
        input: Value::Unit,
        schema: None,
        cache_prompt: false,
    });

    let mut chunks = Vec::new();
    let mut done_seen = false;
    let value = obs.output.await.unwrap();
    while let Ok(ev) = obs.events.try_recv() {
        match ev {
            NodeEvent::LlmChunk { text, .. } => chunks.push(text),
            NodeEvent::LlmDone { .. } => done_seen = true,
            _ => {}
        }
    }
    assert!(!chunks.is_empty(), "expected at least one chunk");
    assert_eq!(chunks.concat(), "hello streaming world");
    assert!(done_seen, "expected LlmDone event");
    assert!(value.text_concat() == "hello streaming world");
}

#[tokio::test]
async fn mock_streaming_non_string_value_emits_single_done() {
    let provider = MockProvider::new("mock").with_model(
        "m",
        Value::Struct(vec![("severity".into(), Value::Str("info".into()))]),
    );
    let mut obs = provider.call_streaming(LlmRequest {
        model: "m".to_string(),
        messages: vec![atman_runtime::provider::user_text_message("")],
        system: None,
        input: Value::Unit,
        schema: None,
        cache_prompt: false,
    });
    let value = obs.output.await.unwrap();
    let mut events = Vec::new();
    while let Ok(ev) = obs.events.try_recv() {
        events.push(ev);
    }
    assert!(matches!(events.last(), Some(NodeEvent::LlmDone { .. })));
    assert!(matches!(
        value.message.parts.first(),
        Some(atman_runtime::message::MessagePart::Text { .. })
    ));
}

#[tokio::test]
async fn mock_streaming_cancel_before_await_yields_cancelled_err() {
    let provider =
        MockProvider::new("mock").with_model("m", Value::Str("some long text to chunk".into()));
    let obs = provider.call_streaming(LlmRequest {
        model: "m".to_string(),
        messages: vec![atman_runtime::provider::user_text_message("")],
        system: None,
        input: Value::Unit,
        schema: None,
        cache_prompt: false,
    });
    obs.cancel.cancel();
    let err = obs.output.await.unwrap_err();
    assert!(matches!(err, atman_runtime::RuntimeError::Cancelled(_)));
}
