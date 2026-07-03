use atman_dsl::parse::parse_file;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::tools::memory_stubs::FetchRule;
use atman_runtime::{Event, Executor, FlowStatus, Value, tools};

use std::sync::Arc;

#[tokio::test]
async fn executor_runs_flow_and_emits_start_end() {
    let src = r#"flow t(n: Int) -> Int {
    return n + 1
}
"#;
    let file = parse_file(src).unwrap();
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);

    let out = ex
        .run(&file, "t", vec![("n".into(), Value::Int(4))])
        .await
        .unwrap();
    assert!(matches!(out, Value::Int(5)));

    let events = ex.events.snapshot();
    assert_eq!(events.len(), 2);
    assert!(matches!(events[0], Event::FlowStart { .. }));
    match &events[1] {
        Event::FlowEnd { status, .. } => assert!(matches!(status, FlowStatus::Ok)),
        other => panic!("expected FlowEnd, got {other:?}"),
    }
}

#[tokio::test]
async fn executor_reports_err_status_on_failure() {
    let src = r#"flow t() -> Int {
    return missing
}
"#;
    let file = parse_file(src).unwrap();
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    let err = ex.run(&file, "t", vec![]).await.unwrap_err();
    assert!(matches!(
        err,
        atman_runtime::RuntimeError::UndefinedVar(name) if name == "missing"
    ));
    let events = ex.events.snapshot();
    assert!(matches!(
        events.last(),
        Some(Event::FlowEnd {
            status: FlowStatus::Errored(_),
            ..
        })
    ));
}

#[tokio::test]
async fn executor_fetch_rule_returns_content() {
    let src = r#"flow t() -> string {
    return fetch_rule("comment-discipline")
}
"#;
    let file = parse_file(src).unwrap();
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);

    let rule = FetchRule::new();
    rule.insert("comment-discipline", "only write why-comments")
        .await;
    ex.tools.register(Arc::new(rule));

    let out = ex.run(&file, "t", vec![]).await.unwrap();
    assert!(matches!(out, Value::Str(s) if s == "only write why-comments"));
}

#[tokio::test]
async fn executor_runs_review_flow_with_mock_provider() {
    let src = r#"flow review_code(file: path) -> Review {
    gather = fetch_confessions()
    primary = llm {
        model: "claude-opus-4.7"
        prompt: "review please"
        input: gather
    }
    return primary
}
"#;
    let file = parse_file(src).unwrap();
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    ex.providers
        .register(Arc::new(MockProvider::new("mock").with_model(
            "claude-opus-4.7",
            Value::Struct(vec![
                ("severity".into(), Value::Str("info".into())),
                ("issues".into(), Value::List(vec![])),
            ]),
        )));

    let out = ex
        .run(
            &file,
            "review_code",
            vec![("file".into(), Value::Str("src/main.rs".into()))],
        )
        .await
        .unwrap();
    if let Value::Struct(fields) = out {
        assert_eq!(fields[0].0, "severity");
        assert!(matches!(&fields[0].1, Value::Str(s) if s == "info"));
    } else {
        panic!("expected struct");
    }
}
