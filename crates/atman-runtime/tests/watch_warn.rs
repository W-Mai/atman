use std::sync::Arc;

use atman_dsl::parse::parse_file;
use atman_runtime::event::{Event, EventSink};
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::{Executor, Value};

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
async fn watch_token_warn_emits_event_and_stream_completes() {
    let _cfg_lock = TEST_CFG_LOCK.lock().await;
    install_mock_model();
    let src = r#"flow review() -> string {
    primary = llm.call(
        model: "mock-model",
        prompt: "review please",
    )
    watch primary {
        on token(match: "warn me") {
            warn("HIT WARN")
        }
    }
    return primary
}
"#;
    let file = parse_file(src).unwrap();
    let sink = EventSink::new();
    let ex = Executor::with_events(sink.clone());
    ex.providers
        .register(Arc::new(MockProvider::new("mock").with_model(
            "mock-model",
            Value::Str("the answer contains warn me right here".into()),
        )));

    let out = ex.run(&file, "review", vec![]).await.unwrap();
    assert!(matches!(&out, Value::Str(s) if s.contains("warn me")));

    let events = sink.snapshot();
    let warn = events.iter().find_map(|e| match e {
        Event::WatchWarn {
            target,
            trigger,
            message,
            ..
        } => Some((target.clone(), trigger.clone(), message.clone())),
        _ => None,
    });
    let (target, trigger, message) = warn.expect("expected WatchWarn event");
    assert_eq!(target, "primary");
    assert!(trigger.starts_with("token("), "trigger: {trigger}");
    assert_eq!(message, "HIT WARN");
}

#[tokio::test]
async fn watch_warn_fires_once_per_pattern_even_on_repeats() {
    let _cfg_lock = TEST_CFG_LOCK.lock().await;
    install_mock_model();
    let src = r#"flow review() -> string {
    primary = llm.call(
        model: "mock-model",
        prompt: "review",
    )
    watch primary {
        on token(match: "warn me") {
            warn()
        }
    }
    return primary
}
"#;
    let file = parse_file(src).unwrap();
    let sink = EventSink::new();
    let ex = Executor::with_events(sink.clone());
    ex.providers.register(Arc::new(
        MockProvider::new("mock")
            .with_model("mock-model", Value::Str("warn me warn me warn me".into())),
    ));

    ex.run(&file, "review", vec![]).await.unwrap();

    let warns = sink
        .snapshot()
        .iter()
        .filter(|e| matches!(e, Event::WatchWarn { .. }))
        .count();
    assert_eq!(warns, 1, "expected exactly one warn, got {warns}");
}
