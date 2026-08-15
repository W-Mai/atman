use atman_dsl::parse::parse_file;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::tools::memory_stubs::RuleFetch;
use atman_runtime::{Event, Executor, FlowStatus, Value, tools};

mod common;

use std::sync::Arc;

#[tokio::test]
async fn executor_runs_flow_and_emits_start_end() {
    let src = r#"flow t(n: Int) -> Int {
    return n + 1
}
"#;
    let file = parse_file(src).unwrap();
    let ex = Executor::new();
    tools::register_tier_zero(&ex.tools);

    let out = ex
        .run(&file, "t", vec![("n".into(), Value::Int(4))])
        .await
        .unwrap();
    assert!(matches!(out, Value::Int(5)));

    let events = ex.events.snapshot();
    assert!(matches!(events[0], Event::FlowStart { .. }));
    assert!(matches!(events[1], Event::FlowGraph { .. }));
    match events.last() {
        Some(Event::FlowEnd { status, .. }) => assert!(matches!(status, FlowStatus::Ok)),
        other => panic!("expected FlowEnd last, got {other:?}"),
    }
}

#[tokio::test]
async fn executor_reports_err_status_on_failure() {
    let src = r#"flow t() -> Int {
    return missing
}
"#;
    let file = parse_file(src).unwrap();
    let ex = Executor::new();
    tools::register_tier_zero(&ex.tools);
    let err = ex.run(&file, "t", vec![]).await.unwrap_err();
    assert!(matches!(
        err,
        atman_runtime::RuntimeError::UndefinedVar(name) if name == "missing"
    ));
    let events = ex.events.snapshot();
    assert!(matches!(
        events.last(),
        Some(Event::FlowEnd {
            status: FlowStatus::Errored { .. },
            ..
        })
    ));
}

#[tokio::test]
async fn executor_rule_fetch_returns_content() {
    let src = r#"flow t() -> string {
    return rule.fetch("comment-discipline")
}
"#;
    let file = parse_file(src).unwrap();
    let ex = Executor::new();
    tools::register_tier_zero(&ex.tools);

    let rule = RuleFetch::new();
    rule.insert("comment-discipline", "only write why-comments")
        .await;
    ex.tools.register(Arc::new(rule));

    let out = ex.run(&file, "t", vec![]).await.unwrap();
    assert!(matches!(out, Value::Str(s) if s == "only write why-comments"));
}

#[tokio::test]
async fn executor_runs_review_flow_with_mock_provider() {
    let _registry =
        common::ModelRegistryGuard::acquire(common::config([common::model_for_provider(
            "claude-opus-4.7",
            "mock",
            8_192,
            None,
        )]))
        .await;
    let src = r#"flow review_code(file: path) -> Review {
    gather = rule.fetch(query: "none")
    primary = llm.call(
        model: "claude-opus-4.7",
        prompt: "review please",
        input: gather,
    )
    return primary
}
"#;
    let file = parse_file(src).unwrap();
    let ex = Executor::new();
    tools::register_tier_zero(&ex.tools);
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
