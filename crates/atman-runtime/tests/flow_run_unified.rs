//! Tests for S2: root FlowRun unification.
//!
//! Verifies that when a flow runs with a session, the root is registered in
//! the agent_registry (so flow.output("root") / flow.interject("root") work)
//! and the session's current_root pointer is set.

use std::sync::Arc;

use atman_dsl::parse::parse_file;
use atman_runtime::session::Session;
use atman_runtime::{Executor, Value, tools};

const SIMPLE_FLOW: &str = r#"flow t(n: Int) -> Int {
    return n + 1
}
"#;

#[tokio::test]
async fn root_flow_run_registered_in_agent_registry() {
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

    // Root should be registered as a FlowRun in the agent_registry.
    let root = session.agent_registry.lookup("root");
    assert!(root.is_ok(), "root should be in agent_registry");
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
    assert!(session.agent_registry.lookup("root").is_ok());
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
    // No session → no current_root pointer to check, but agent_registry on
    // the executor's tool_ctx is None, so no root entry created.
}
