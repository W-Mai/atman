use std::sync::Arc;

use atman_dsl::parse::parse_file;
use atman_runtime::error::RuntimeError;
use atman_runtime::event::{Event, EventSink};
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::safety::{
    NoopClassifier, SafetyClassifier, SafetyConfig, SafetyMode, ScanVerdict,
};
use atman_runtime::tool::BoxFut;
use atman_runtime::{Executor, Value};

struct StaticClassifier {
    verdict: ScanVerdict,
    kind: &'static str,
}

impl StaticClassifier {
    fn new(kind: &'static str, verdict: ScanVerdict) -> Self {
        Self { verdict, kind }
    }
}

impl SafetyClassifier for StaticClassifier {
    fn scan<'a>(&'a self, _text: &'a str) -> BoxFut<'a, Result<ScanVerdict, RuntimeError>> {
        let v = self.verdict.clone();
        Box::pin(async move { Ok(v) })
    }
    fn kind(&self) -> &'static str {
        self.kind
    }
}

fn agent_source() -> &'static str {
    r#"
flow t(prompt: string) -> string {
    return llm { model: "mock", prompt: prompt }
}
"#
}

async fn run(safety: SafetyConfig, sink: EventSink) -> Result<Value, RuntimeError> {
    let mut ex = Executor::with_events(sink).with_safety(safety);
    ex.providers.register(Arc::new(
        MockProvider::new("mock").with_fallback(Value::Str("ok".into())),
    ));
    let file = parse_file(agent_source()).unwrap();
    ex.run(
        &file,
        "t",
        vec![("prompt".into(), Value::Str("hi there".into()))],
    )
    .await
}

#[tokio::test]
async fn safety_disabled_never_touches_classifier() {
    let cfg = SafetyConfig {
        enabled: false,
        mode: SafetyMode::Deny,
        auto_rewrite: false,
        classifier: Arc::new(StaticClassifier::new(
            "static-deny",
            ScanVerdict::Deny(vec!["should-not-fire".into()]),
        )),
    };
    let sink = EventSink::new();
    let out = run(cfg, sink.clone()).await.unwrap();
    match out {
        Value::Str(s) => assert_eq!(s, "ok"),
        other => panic!("expected ok string, got {other:?}"),
    }
    let hits: usize = sink
        .snapshot()
        .iter()
        .filter(|e| matches!(e, Event::ContentFilterHit { .. }))
        .count();
    assert_eq!(hits, 0, "disabled config must not emit content_filter_hit");
}

#[tokio::test]
async fn safety_warn_mode_emits_events_but_still_runs_the_call() {
    let cfg = SafetyConfig {
        enabled: true,
        mode: SafetyMode::Warn,
        auto_rewrite: false,
        classifier: Arc::new(StaticClassifier::new(
            "static-warn",
            ScanVerdict::Warn(vec!["hate".into(), "violence".into()]),
        )),
    };
    let sink = EventSink::new();
    let out = run(cfg, sink.clone()).await.unwrap();
    match out {
        Value::Str(s) => assert_eq!(s, "ok"),
        other => panic!("expected ok string, got {other:?}"),
    }
    let hits: Vec<(String, String)> = sink
        .snapshot()
        .into_iter()
        .filter_map(|e| match e {
            Event::ContentFilterHit {
                category, action, ..
            } => Some((category, action)),
            _ => None,
        })
        .collect();
    assert_eq!(hits.len(), 2, "one hit per flagged category: {hits:?}");
    for (_, action) in &hits {
        assert_eq!(action, "warned");
    }
}

#[tokio::test]
async fn safety_deny_mode_blocks_and_never_calls_provider() {
    let cfg = SafetyConfig {
        enabled: true,
        mode: SafetyMode::Deny,
        auto_rewrite: false,
        classifier: Arc::new(StaticClassifier::new(
            "static-deny",
            ScanVerdict::Deny(vec!["self-harm".into()]),
        )),
    };
    let sink = EventSink::new();
    let result = run(cfg, sink.clone()).await;
    match result {
        Err(RuntimeError::ToolFailed(msg)) => {
            assert!(msg.contains("self-harm"), "err: {msg}");
            assert!(msg.contains("content_filter"), "err: {msg}");
        }
        other => panic!("expected content_filter err, got {other:?}"),
    }
    let hits: Vec<String> = sink
        .snapshot()
        .into_iter()
        .filter_map(|e| match e {
            Event::ContentFilterHit { action, .. } => Some(action),
            _ => None,
        })
        .collect();
    assert!(hits.iter().all(|a| a == "blocked"), "hits: {hits:?}");
    let llm_calls = sink
        .snapshot()
        .into_iter()
        .filter(|e| matches!(e, Event::LlmCall { .. }))
        .count();
    assert_eq!(llm_calls, 0, "provider must not be called on deny");
}

#[tokio::test]
async fn safety_noop_classifier_passes_through() {
    let cfg = SafetyConfig {
        enabled: true,
        mode: SafetyMode::Deny,
        auto_rewrite: false,
        classifier: Arc::new(NoopClassifier),
    };
    let sink = EventSink::new();
    let out = run(cfg, sink.clone()).await.unwrap();
    assert!(matches!(out, Value::Str(s) if s == "ok"));
    let hits: usize = sink
        .snapshot()
        .iter()
        .filter(|e| matches!(e, Event::ContentFilterHit { .. }))
        .count();
    assert_eq!(hits, 0);
}
