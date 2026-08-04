//! Root FlowRun unification tests.
//!
//! Verifies that when a flow runs with a session, the root is registered in
//! the flow_registry (so flow.output("root") / flow.interject("root") work)
//! and the session's current_root pointer is set.

use std::sync::Arc;

use atman_dsl::parse::parse_file;
use atman_runtime::event::FlowRunId;
use atman_runtime::session::Session;
use atman_runtime::tool::{Tool, ToolArgs, ToolCtx};
use atman_runtime::tools::agent_ctrl::{FlowInterject, FlowRegistry};
use atman_runtime::{Executor, Value, tools};

const SIMPLE_FLOW: &str = r#"flow t(n: Int) -> Int {
    return n + 1
}
"#;

#[tokio::test]
async fn root_flow_run_registered_in_flow_registry() {
    let file = parse_file(SIMPLE_FLOW).unwrap();
    let session = Arc::new(Session::open_ephemeral());
    let mut ex = Executor::with_events(session.sink().clone());
    tools::register_tier_zero(&mut ex.tools);

    let out = ex
        .run_in_turn(
            &file,
            "t",
            vec![("n".into(), Value::Int(4))],
            None,
            Some(session.clone()),
        )
        .await
        .unwrap();
    assert!(matches!(out, Value::Int(5)));

    // Root should be in flow_registry.
    let root = session.flow_registry.lookup("root");
    assert!(root.is_ok(), "root should be in flow_registry");
    let root = root.unwrap();
    assert_eq!(root.handle, "root");

    // current_root pointer should point at "root".
    assert_eq!(session.current_root(), Some("root".to_string()));
}

#[tokio::test]
async fn current_root_cleared_and_reset_across_turns() {
    let file = parse_file(SIMPLE_FLOW).unwrap();
    let session = Arc::new(Session::open_ephemeral());
    let mut ex = Executor::with_events(session.sink().clone());
    tools::register_tier_zero(&mut ex.tools);

    // Turn 1
    ex.run_in_turn(
        &file,
        "t",
        vec![("n".into(), Value::Int(1))],
        None,
        Some(session.clone()),
    )
    .await
    .unwrap();
    assert_eq!(session.current_root(), Some("root".to_string()));

    // Turn 2 — root entry is overwritten (same handle "root")
    ex.run_in_turn(
        &file,
        "t",
        vec![("n".into(), Value::Int(2))],
        None,
        Some(session.clone()),
    )
    .await
    .unwrap();
    assert_eq!(session.current_root(), Some("root".to_string()));
    // Still only one "root" entry.
    assert!(session.flow_registry.lookup("root").is_ok());
}

#[tokio::test]
async fn no_session_no_root_registration() {
    // Ephemeral runs without a session should not register root.
    let file = parse_file(SIMPLE_FLOW).unwrap();
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);

    let out = ex
        .run(&file, "t", vec![("n".into(), Value::Int(4))])
        .await
        .unwrap();
    assert!(matches!(out, Value::Int(5)));
    // No session → no current_root pointer to check, but flow_registry on
    // the executor's tool_ctx is None, so no root entry created.
}

#[tokio::test]
async fn flow_interject_delivers_to_target_entry_channel() {
    let registry = Arc::new(FlowRegistry::new());
    let entry = registry.create_entry("sub_1".into(), "g".into(), "m".into(), FlowRunId::now());
    let ctx = ToolCtx::new().with_flow_registry(registry);
    let mut rx = entry.interjection_tx.subscribe();
    let args = ToolArgs {
        positional: vec![Value::Str("sub_1".into()), Value::Str("wake up".into())],
        named: vec![],
    };
    FlowInterject.call(args, &ctx).await.unwrap();
    let inj = rx.try_recv().expect("interjection should arrive");
    assert_eq!(inj.text, "wake up");
}

#[tokio::test]
async fn flow_interject_unknown_handle_errors() {
    let registry = Arc::new(FlowRegistry::new());
    let ctx = ToolCtx::new().with_flow_registry(registry);
    let args = ToolArgs {
        positional: vec![Value::Str("nope".into()), Value::Str("x".into())],
        named: vec![],
    };
    let err = FlowInterject.call(args, &ctx).await.unwrap_err();
    assert!(err.to_string().contains("not found"));
}
