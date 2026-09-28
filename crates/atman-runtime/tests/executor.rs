use atman_rt::parse_file;
type FlowRunId = atman_rt::RunId<atman_runtime::event::AtmanUuid>;
use atman_runtime::flow_authority::EffectiveAuthority;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::task_registry::{TaskFilter, TaskRegistry};
use atman_runtime::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use atman_runtime::tools::agent_ctrl::FlowRegistry;
use atman_runtime::tools::memory_stubs::RuleFetch;
use atman_runtime::{Event, Executor, FlowStatus, tools};
type Value = atman_rt::Value<atman_runtime::AtmanPayload, atman_runtime::RuntimeError>;

mod common;

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct ProbeTool {
    name: &'static str,
    tier: Tier,
    calls: Arc<AtomicUsize>,
}

impl Tool for ProbeTool {
    fn name(&self) -> &str {
        self.name
    }

    fn tier(&self) -> Tier {
        self.tier
    }

    fn requires_call_intent(&self) -> bool {
        false
    }

    fn call<'a>(&'a self, _args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(Value::Int(1)) })
    }
}

#[tokio::test]
async fn denied_or_missing_tool_does_not_run_argument_effects() {
    let ex = Executor::new();
    let calls = Arc::new(AtomicUsize::new(0));
    for (name, tier) in [("side.effect", Tier::Zero), ("shell.exec", Tier::Four)] {
        ex.tools.register(Arc::new(ProbeTool {
            name,
            tier,
            calls: Arc::clone(&calls),
        }));
    }
    for (outer, expected) in [("shell.exec", "Tier 4"), ("missing.tool", "undefined tool")] {
        let source = format!("flow t() -> int {{ return {outer}(side.effect()) }}");
        let file = parse_file(&source).unwrap();
        let error = ex.run(&file, "t", vec![]).await.unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}

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
async fn root_registration_failure_does_not_leave_running_task() {
    let file = parse_file("flow t() -> Int { return 1 }").unwrap();
    let run_id = FlowRunId::now();
    let flow_registry = Arc::new(FlowRegistry::new());
    flow_registry
        .register_root(
            "existing-session".into(),
            run_id.clone(),
            EffectiveAuthority::root(&Default::default(), false, None),
        )
        .unwrap();
    let tasks = TaskRegistry::new();
    let mut ex = Executor::new();
    ex.tool_ctx = ex
        .tool_ctx
        .clone()
        .with_flow_registry(flow_registry)
        .with_task_registry(tasks.clone());

    let error = ex
        .run_in_turn_with_run_id(&file, "t", vec![], None, None, Some(run_id))
        .await
        .unwrap_err();

    assert!(error.to_string().contains("already registered"));
    assert!(tasks.list(&TaskFilter::running()).is_empty());
    assert!(tasks.list(&TaskFilter::all()).is_empty());
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
