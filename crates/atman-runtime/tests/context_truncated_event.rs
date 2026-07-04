use atman_dsl::parse::parse_file;
use atman_runtime::event::{Event, EventSink};
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::{Executor, Value};
use std::sync::Arc;

#[tokio::test]
async fn context_truncated_event_emitted_when_prompt_exceeds_budget() {
    let long_body = "x".repeat(20_000);
    let src = format!(
        r#"flow t() -> string {{
    reply = llm {{
        model: "mock"
        prompt: "{long_body}"
        context_budget: 100
    }}
    return reply
}}
"#
    );

    let sink = EventSink::new();
    let mut ex = Executor::with_events(sink.clone());
    ex.providers.register(Arc::new(
        MockProvider::new("mock").with_fallback(Value::Str("ok".into())),
    ));
    let file = parse_file(&src).unwrap();
    let result = ex.run(&file, "t", vec![]).await.expect("flow ok");
    match result {
        Value::Str(s) => assert_eq!(s, "ok"),
        other => panic!("unexpected result {other:?}"),
    }

    let events = sink.snapshot();
    let trunc = events
        .iter()
        .find(|e| matches!(e, Event::ContextTruncated { .. }))
        .expect("expected ContextTruncated event in sink");
    match trunc {
        Event::ContextTruncated {
            original_chars,
            result_chars,
            dropped_chars,
            budget_tokens,
            ..
        } => {
            assert_eq!(*original_chars, 20_000);
            assert!(*dropped_chars > 0);
            assert!(*result_chars < *original_chars);
            assert_eq!(*budget_tokens, 100);
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn context_truncated_event_not_emitted_when_prompt_under_budget() {
    let src = r#"flow t() -> string {
    reply = llm {
        model: "mock"
        prompt: "short"
        context_budget: 100
    }
    return reply
}
"#;
    let sink = EventSink::new();
    let mut ex = Executor::with_events(sink.clone());
    ex.providers.register(Arc::new(
        MockProvider::new("mock").with_fallback(Value::Str("ok".into())),
    ));
    let file = parse_file(src).unwrap();
    ex.run(&file, "t", vec![]).await.expect("flow ok");

    let events = sink.snapshot();
    let has_trunc = events
        .iter()
        .any(|e| matches!(e, Event::ContextTruncated { .. }));
    assert!(!has_trunc, "should NOT emit ContextTruncated below budget");
}
