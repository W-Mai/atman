//! Root FlowRun unification tests.
//!
//! Verifies that when a flow runs with a session, the root is registered in
//! the flow_registry (so flow.output("root") / flow.interject("root") work)
//! and the session's current_root pointer is set.

mod common;

use std::sync::Arc;

use atman_dsl::parse::parse_file;
use atman_runtime::event::FlowRunId;
use atman_runtime::flow_authority::{
    ChildWorkspaceAuthority, EffectiveAuthority, FlowExecutionState, InvocationKind,
};
use atman_runtime::session::Session;
use atman_runtime::tool::{Tool, ToolArgs, ToolCtx};
use atman_runtime::tools::agent_ctrl::{FlowInterject, FlowRegistry};
use atman_runtime::{Executor, Value, tools};

static HOME_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct HomeGuard(Option<std::ffi::OsString>);

impl HomeGuard {
    fn set(home: &std::path::Path) -> Self {
        let old = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", home);
        }
        Self(old)
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        unsafe {
            match self.0.take() {
                Some(old) => std::env::set_var("HOME", old),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}

const SIMPLE_FLOW: &str = r#"flow t(n: Int) -> Int {
    return n + 1
}
"#;

#[tokio::test]
async fn root_flow_run_registered_in_flow_registry() {
    let file = parse_file(SIMPLE_FLOW).unwrap();
    let session = Arc::new(Session::open_ephemeral());
    let ex = Executor::with_events(session.sink().clone());
    tools::register_tier_zero(&ex.tools);

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
    assert!(matches!(
        *root.status.lock().unwrap(),
        atman_runtime::tools::agent_ctrl::FlowRunStatus::Ok { .. }
    ));

    // current_root pointer should point at "root".
    assert_eq!(session.current_root(), Some("root".to_string()));
}

#[test]
fn lifecycle_guard_marks_registered_run_terminal_on_drop() {
    let registry = Arc::new(FlowRegistry::new());
    let run_id = FlowRunId::now();
    registry
        .register_root(
            "session".into(),
            run_id.clone(),
            EffectiveAuthority::root(&Default::default(), false, None),
        )
        .unwrap();

    {
        let _guard = registry.lifecycle_guard(&run_id);
        assert_eq!(
            registry.execution_state(&run_id),
            Some(FlowExecutionState::Running)
        );
    }

    assert_eq!(
        registry.execution_state(&run_id),
        Some(FlowExecutionState::Terminal)
    );
}

#[test]
fn ancestry_and_handle_removal_matrix_preserves_run_identities() {
    let registry = Arc::new(FlowRegistry::new());
    let root = FlowRunId::now();
    let child = FlowRunId::now();
    let grandchild = FlowRunId::now();
    let sibling = FlowRunId::now();
    let other_root = FlowRunId::now();
    registry
        .register_root(
            "session-a".into(),
            root.clone(),
            EffectiveAuthority::root(&Default::default(), false, None),
        )
        .unwrap();
    for (parent, run) in [
        (&root, child.clone()),
        (&child, grandchild.clone()),
        (&root, sibling.clone()),
    ] {
        registry
            .register_child(
                parent,
                run,
                InvocationKind::InlineSubflow,
                false,
                ChildWorkspaceAuthority::Inherit,
            )
            .unwrap();
    }
    registry
        .register_root(
            "session-b".into(),
            other_root.clone(),
            EffectiveAuthority::root(&Default::default(), false, None),
        )
        .unwrap();

    assert!(!registry.is_strict_ancestor(&root, &root));
    assert!(registry.is_strict_ancestor(&root, &child));
    assert!(registry.is_strict_ancestor(&root, &grandchild));
    assert!(registry.is_strict_ancestor(&child, &grandchild));
    assert!(!registry.is_strict_ancestor(&child, &sibling));
    assert!(!registry.is_strict_ancestor(&root, &other_root));
    assert!(!registry.is_strict_ancestor(&FlowRunId::now(), &grandchild));
    assert_eq!(
        registry
            .strict_ancestors(&grandchild)
            .iter()
            .map(|identity| identity.run_id.clone())
            .collect::<Vec<_>>(),
        vec![child.clone(), root.clone()]
    );

    registry.create_entry(
        "child-handle".into(),
        "child".into(),
        "m".into(),
        child.clone(),
        Default::default(),
    );
    registry.remove("child-handle");
    assert!(registry.lookup("child-handle").is_err());
    assert!(registry.lookup_run(&child).is_some());
    assert!(registry.is_strict_ancestor(&root, &grandchild));
}

#[test]
fn register_child_rejections_leave_the_identity_graph_unchanged() {
    let registry = Arc::new(FlowRegistry::new());
    let root = FlowRunId::now();
    let child = FlowRunId::now();
    registry
        .register_root(
            "session".into(),
            root.clone(),
            EffectiveAuthority::root(&Default::default(), false, None),
        )
        .unwrap();
    registry
        .register_child(
            &root,
            child.clone(),
            InvocationKind::InlineSubflow,
            false,
            ChildWorkspaceAuthority::Inherit,
        )
        .unwrap();

    let duplicate_error = registry
        .register_child(
            &root,
            child.clone(),
            InvocationKind::SpawnSync,
            false,
            ChildWorkspaceAuthority::Inherit,
        )
        .unwrap_err();
    assert!(duplicate_error.to_string().contains("already registered"));

    let missing_parent = FlowRunId::now();
    let missing_parent_child = FlowRunId::now();
    let missing_error = registry
        .register_child(
            &missing_parent,
            missing_parent_child.clone(),
            InvocationKind::SpawnAsync,
            false,
            ChildWorkspaceAuthority::Inherit,
        )
        .unwrap_err();
    assert!(missing_error.to_string().contains("is not registered"));

    let terminal_parent = FlowRunId::now();
    let terminal_parent_child = FlowRunId::now();
    registry
        .register_root(
            "session".into(),
            terminal_parent.clone(),
            EffectiveAuthority::root(&Default::default(), false, None),
        )
        .unwrap();
    registry.mark_terminal(&terminal_parent);
    let terminal_error = registry
        .register_child(
            &terminal_parent,
            terminal_parent_child.clone(),
            InvocationKind::InlineSubflow,
            false,
            ChildWorkspaceAuthority::Inherit,
        )
        .unwrap_err();
    assert!(terminal_error.to_string().contains("is terminal"));

    let registered_child = registry.lookup_run(&child).unwrap();
    assert_eq!(registered_child.parent_run_id.as_ref(), Some(&root));
    assert_eq!(registered_child.invocation, InvocationKind::InlineSubflow);
    assert!(registry.is_strict_ancestor(&root, &child));
    assert!(registry.lookup_run(&missing_parent_child).is_none());
    assert!(registry.lookup_run(&terminal_parent_child).is_none());
    assert_eq!(
        registry.execution_state(&root),
        Some(FlowExecutionState::Running)
    );
    assert_eq!(
        registry.execution_state(&terminal_parent),
        Some(FlowExecutionState::Terminal)
    );
}

#[test]
fn duplicate_descendant_guards_are_reference_counted() {
    let registry = Arc::new(FlowRegistry::new());
    let parent_run_id = FlowRunId::now();
    let child_run_id = FlowRunId::now();
    registry
        .register_root(
            "session".into(),
            parent_run_id.clone(),
            EffectiveAuthority::root(&Default::default(), false, None),
        )
        .unwrap();
    registry
        .register_child(
            &parent_run_id,
            child_run_id.clone(),
            InvocationKind::InlineSubflow,
            false,
            ChildWorkspaceAuthority::Inherit,
        )
        .unwrap();

    let first = registry
        .block_on_descendant(&parent_run_id, &child_run_id)
        .unwrap();
    let second = registry
        .block_on_descendant(&parent_run_id, &child_run_id)
        .unwrap();
    drop(first);
    assert!(matches!(
        registry.execution_state(&parent_run_id),
        Some(FlowExecutionState::BlockedOnDescendants { .. })
    ));

    drop(second);
    assert_eq!(
        registry.execution_state(&parent_run_id),
        Some(FlowExecutionState::Running)
    );
}

#[tokio::test]
async fn current_root_cleared_and_reset_across_turns() {
    let file = parse_file(SIMPLE_FLOW).unwrap();
    let session = Arc::new(Session::open_ephemeral());
    let ex = Executor::with_events(session.sink().clone());
    tools::register_tier_zero(&ex.tools);

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
    let ex = Executor::new();
    tools::register_tier_zero(&ex.tools);

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
    let entry = registry.create_entry(
        "sub_1".into(),
        "g".into(),
        "m".into(),
        FlowRunId::now(),
        Default::default(),
    );
    let ctx = ToolCtx::new().with_flow_registry(registry);
    let args = ToolArgs {
        positional: vec![Value::Str("sub_1".into()), Value::Str("wake up".into())],
        named: vec![],
    };
    FlowInterject.call(args, &ctx).await.unwrap();
    let pending = entry.pending_injections.lock().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].text, "wake up");
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

#[tokio::test]
async fn flow_interject_cancels_running_subagent_llm() {
    let _home_lock = HOME_TEST_LOCK.lock().await;
    let _registry =
        common::ModelRegistryGuard::acquire(common::config([common::model_for_provider(
            "mock", "mock", 100_000, None,
        )]))
        .await;
    use atman_runtime::providers::mock::MockProvider;
    use atman_runtime::tool::{Tool, ToolRegistry};
    use atman_runtime::tools::agent_ctrl::{
        AgentSpawn, FlowInterject, FlowRegistry, FlowRunStatus,
    };
    use std::io::Write;

    let tmp = tempfile::tempdir().unwrap();
    let commands_dir = tmp.path().join(".config").join("atman").join("commands");
    std::fs::create_dir_all(&commands_dir).unwrap();
    let flow_src = r#"flow describe() -> string { return "test" }
flow test_flow(goal: string) -> string {
    reply = llm.call(
        model: "mock",
        prompt: goal,
        context: "session",
    )
    return text_concat(reply)
}
"#;
    let mut f = std::fs::File::create(commands_dir.join("test_interject.at")).unwrap();
    f.write_all(flow_src.as_bytes()).unwrap();

    let _home = HomeGuard::set(tmp.path());

    let registry = Arc::new(FlowRegistry::new());
    let events = atman_runtime::event::EventSink::new();
    let tasks = atman_runtime::task_registry::TaskRegistry::new();
    let providers = atman_runtime::provider::ProviderRegistry::new();
    providers.register(Arc::new(
        MockProvider::new("mock")
            .with_fallback(atman_runtime::Value::Str("ok".into()))
            .with_chunk_delay(std::time::Duration::from_secs(1)),
    ));
    let tools = ToolRegistry::new();
    atman_runtime::tools::register_tier_zero(&tools);
    let (stream_tx, _) = tokio::sync::broadcast::channel::<atman_runtime::stream::StreamFrame>(256);

    let root_run_id = FlowRunId::now();
    let root_identity = registry
        .register_root(
            "test-session".into(),
            root_run_id.clone(),
            EffectiveAuthority::root(&Default::default(), false, None),
        )
        .unwrap();
    let broker = atman_runtime::permission::PermissionBroker::shared(registry.clone());
    let mut ctx = ToolCtx::new()
        .with_events(events.clone())
        .with_registry(Arc::new(tools))
        .with_providers(Arc::new(providers))
        .with_flow_registry(registry.clone())
        .with_permission_broker(broker)
        .with_approval(Arc::new(atman_runtime::session::ApprovalRegistry::new()))
        .with_trust(atman_runtime::trust::TrustConfig::default())
        .with_stream_tx(stream_tx);
    ctx.task_registry = Some(tasks.clone());
    ctx.flow_run_id = Some(root_run_id);
    ctx.flow_identity = Some(root_identity);

    let spawn_args = ToolArgs {
        positional: vec![],
        named: vec![
            (
                "flow".into(),
                Value::Str("test_interject.at@test_flow".into()),
            ),
            (
                "arguments".into(),
                Value::Struct(vec![("goal".into(), Value::Str("test goal".into()))]),
            ),
        ],
    };
    let result = AgentSpawn.call(spawn_args, &ctx).await.unwrap();
    let handle = match result {
        Value::Struct(fields) => fields
            .iter()
            .find(|(k, _)| k == "handle")
            .and_then(|(_, v)| {
                if let Value::Str(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .expect("expected handle"),
        _ => panic!("expected struct with handle"),
    };

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let interject_args = ToolArgs {
        positional: vec![Value::Str(handle.clone()), Value::Str("stop now".into())],
        named: vec![("level".into(), Value::Str("l4_hard_stop".into()))],
    };
    let _ = FlowInterject.call(interject_args, &ctx).await.unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let entry = registry.lookup(&handle).unwrap();
    let status = entry.status.lock().unwrap().clone();
    assert!(
        matches!(status, FlowRunStatus::Killed { .. }),
        "sub-agent should be killed after hard stop, got: {:?}",
        status
    );
    assert!(events.snapshot().iter().any(|event| matches!(event,
        atman_runtime::event::Event::FlowEnd { run_id, status: atman_runtime::event::FlowStatus::Cancelled, .. }
        if run_id == &entry.child_run_id
    )));
    assert!(matches!(
        tasks.lookup_by_handle(&handle).unwrap().status,
        atman_runtime::task_registry::TaskStatus::Killed
    ));
}

#[tokio::test]
async fn l1_nudge_text_appears_in_entry_messages() {
    let _home_lock = HOME_TEST_LOCK.lock().await;
    let _registry =
        common::ModelRegistryGuard::acquire(common::config([common::model_for_provider(
            "mock", "mock", 100_000, None,
        )]))
        .await;
    use atman_runtime::Value;
    use atman_runtime::providers::mock::MockProvider;
    use atman_runtime::tool::{Tool, ToolRegistry};
    use atman_runtime::tools::agent_ctrl::{AgentSpawn, FlowRegistry};
    use std::io::Write;

    let tmp = tempfile::tempdir().unwrap();
    let commands_dir = tmp.path().join(".config").join("atman").join("commands");
    std::fs::create_dir_all(&commands_dir).unwrap();
    let flow_src = r#"flow describe() -> string { return "test" }
flow test_flow(goal: string) -> string {
    session.push(message.user(goal))
    reply = llm.call(model: "mock", context: "session")
    return text_concat(reply)
}
"#;
    let mut f = std::fs::File::create(commands_dir.join("test_l1.at")).unwrap();
    f.write_all(flow_src.as_bytes()).unwrap();

    let _home = HomeGuard::set(tmp.path());

    let registry = Arc::new(FlowRegistry::new());
    let providers = atman_runtime::provider::ProviderRegistry::new();
    providers.register(Arc::new(
        MockProvider::new("mock")
            .with_fallback(Value::Str("done".into()))
            .with_chunk_delay(std::time::Duration::from_millis(50)),
    ));
    let tools = ToolRegistry::new();
    atman_runtime::tools::register_tier_zero(&tools);

    let (stream_tx, _) = tokio::sync::broadcast::channel::<atman_runtime::stream::StreamFrame>(256);
    let root_run_id = FlowRunId::now();
    let root_identity = registry
        .register_root(
            "test-session".into(),
            root_run_id.clone(),
            EffectiveAuthority::root(&Default::default(), false, None),
        )
        .unwrap();
    let broker = atman_runtime::permission::PermissionBroker::shared(registry.clone());
    let mut ctx = ToolCtx::new()
        .with_registry(Arc::new(tools))
        .with_providers(Arc::new(providers))
        .with_flow_registry(registry.clone())
        .with_permission_broker(broker)
        .with_approval(Arc::new(atman_runtime::session::ApprovalRegistry::new()))
        .with_trust(atman_runtime::trust::TrustConfig::default())
        .with_stream_tx(stream_tx);
    ctx.flow_run_id = Some(root_run_id);
    ctx.flow_identity = Some(root_identity);

    let spawn_args = ToolArgs {
        positional: vec![],
        named: vec![
            ("flow".into(), Value::Str("test_l1.at@test_flow".into())),
            (
                "arguments".into(),
                Value::Struct(vec![("goal".into(), Value::Str("hello".into()))]),
            ),
        ],
    };
    let result = AgentSpawn.call(spawn_args, &ctx).await.unwrap();
    let handle = match result {
        Value::Struct(fields) => fields
            .iter()
            .find(|(k, _)| k == "handle")
            .and_then(|(_, v)| {
                if let Value::Str(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .expect("expected handle"),
        _ => panic!("expected struct"),
    };

    // Push an L1 nudge.
    let inj = atman_runtime::injection::Injection::new_pending(
        atman_runtime::event::TurnId::now(),
        String::from("NUDGE: check config"),
    );
    let entry = registry.lookup(&handle).unwrap();
    entry.pending_injections.lock().unwrap().push(inj);
    entry.injection_notify.notify_one();

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Check sub-agent status first.
    let entry = registry.lookup(&handle).unwrap();
    let status = entry.status.lock().unwrap().clone();
    let pending_count = entry.pending_injections.lock().unwrap().len();
    let msgs = entry.messages.lock().unwrap();
    let has_nudge = msgs
        .iter()
        .any(|m| m.text_concat().contains("NUDGE: check config"));
    assert!(
        has_nudge,
        "L1 nudge text should be in entry.messages, got: {:?}, pending: {}, status: {:?}",
        msgs.iter().map(|m| m.text_concat()).collect::<Vec<_>>(),
        pending_count,
        status
    );

    // Verify pending_injections is empty (drained).
    let pending = entry.pending_injections.lock().unwrap();
    assert!(
        pending.is_empty(),
        "pending_injections should be empty after drain"
    );
}
