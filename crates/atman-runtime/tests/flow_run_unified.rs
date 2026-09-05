//! Root FlowRun unification tests.
//!
//! Verifies that root controls are scoped by execution identity and completed
//! entries release their context while durable run identities remain.

mod common;

use std::sync::Arc;

use atman_dsl::parse::parse_file;
use atman_runtime::event::{FlowRunId, TurnId};
use atman_runtime::flow_authority::{
    ChildWorkspaceAuthority, EffectiveAuthority, FlowExecutionState, InvocationKind,
};
use atman_runtime::session::Session;
use atman_runtime::tool::{Tool, ToolArgs, ToolCtx};
use atman_runtime::tools::agent_ctrl::{FlowEntryOptions, FlowInterject, FlowRegistry};
use atman_runtime::{Executor, Value, tools};

static CONFIG_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct ConfigDirGuard(Option<std::ffi::OsString>);

impl ConfigDirGuard {
    fn set(config_dir: &std::path::Path) -> Self {
        let old = std::env::var_os("ATMAN_CONFIG_DIR");
        unsafe {
            std::env::set_var("ATMAN_CONFIG_DIR", config_dir);
        }
        Self(old)
    }
}

impl Drop for ConfigDirGuard {
    fn drop(&mut self) {
        unsafe {
            match self.0.take() {
                Some(old) => std::env::set_var("ATMAN_CONFIG_DIR", old),
                None => std::env::remove_var("ATMAN_CONFIG_DIR"),
            }
        }
    }
}

const SIMPLE_FLOW: &str = r#"flow t(n: Int) -> Int {
    return n + 1
}
"#;

#[tokio::test]
async fn completed_root_releases_entry_but_keeps_run_identity() {
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

    let run_id = session
        .sink()
        .snapshot()
        .into_iter()
        .find_map(|event| match event {
            atman_runtime::event::Event::FlowStart {
                run_id,
                parent_run_id: None,
                ..
            } => Some(run_id),
            _ => None,
        })
        .expect("root flow start event");
    assert_eq!(
        session.flow_registry.execution_state(&run_id),
        Some(FlowExecutionState::Terminal)
    );
    assert!(session.flow_registry.entry_for_run(&run_id).is_none());
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

    registry
        .create_entry(
            "child-handle".into(),
            "child".into(),
            "m".into(),
            child.clone(),
            Default::default(),
        )
        .unwrap();
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
async fn completed_root_runs_keep_distinct_identities() {
    let file = parse_file(SIMPLE_FLOW).unwrap();
    let session = Arc::new(Session::open_ephemeral());
    let ex = Executor::with_events(session.sink().clone());
    tools::register_tier_zero(&ex.tools);

    ex.run_in_turn(
        &file,
        "t",
        vec![("n".into(), Value::Int(1))],
        None,
        Some(session.clone()),
    )
    .await
    .unwrap();

    ex.run_in_turn(
        &file,
        "t",
        vec![("n".into(), Value::Int(2))],
        None,
        Some(session.clone()),
    )
    .await
    .unwrap();
    let run_ids = session
        .sink()
        .snapshot()
        .into_iter()
        .filter_map(|event| match event {
            atman_runtime::event::Event::FlowStart {
                run_id,
                parent_run_id: None,
                ..
            } => Some(run_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(run_ids.len(), 2);
    assert_ne!(run_ids[0], run_ids[1]);
    for run_id in run_ids {
        assert_eq!(
            session.flow_registry.execution_state(&run_id),
            Some(FlowExecutionState::Terminal)
        );
        assert!(session.flow_registry.entry_for_run(&run_id).is_none());
    }
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
    // The executor has no session registry in which to create a root entry.
}

#[tokio::test]
async fn flow_interject_delivers_to_target_entry_channel() {
    let registry = Arc::new(FlowRegistry::new());
    let run_id = FlowRunId::now();
    registry
        .register_root(
            "session".into(),
            run_id.clone(),
            EffectiveAuthority::root(&Default::default(), false, None),
        )
        .unwrap();
    let entry = registry
        .create_entry(
            "sub_1".into(),
            "g".into(),
            "m".into(),
            run_id,
            Default::default(),
        )
        .unwrap();
    let ctx = ToolCtx::new().with_flow_registry(registry);
    let args = ToolArgs {
        positional: vec![Value::Str("sub_1".into()), Value::Str("wake up".into())],
        named: vec![],
    };
    FlowInterject.call(args, &ctx).await.unwrap();
    let pending = entry.pending_injections();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].text, "wake up");
}

#[tokio::test]
async fn root_alias_resolves_within_the_callers_execution_tree() {
    let registry = Arc::new(FlowRegistry::new());
    let first_run = FlowRunId::now();
    let second_run = FlowRunId::now();
    let first_turn = TurnId::now();
    let second_turn = TurnId::now();
    let first_identity = registry
        .register_root(
            "session".into(),
            first_run.clone(),
            EffectiveAuthority::root(&Default::default(), false, None),
        )
        .unwrap();
    let second_identity = registry
        .register_root(
            "session".into(),
            second_run.clone(),
            EffectiveAuthority::root(&Default::default(), false, None),
        )
        .unwrap();
    let first_entry = registry
        .create_entry(
            first_run.to_string(),
            "first".into(),
            "model".into(),
            first_run,
            FlowEntryOptions {
                turn_id: Some(first_turn.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    let second_entry = registry
        .create_entry(
            second_run.to_string(),
            "second".into(),
            "model".into(),
            second_run,
            FlowEntryOptions {
                turn_id: Some(second_turn.clone()),
                ..Default::default()
            },
        )
        .unwrap();

    for (identity, expected, text) in [
        (first_identity, &first_entry, "first correction"),
        (second_identity, &second_entry, "second correction"),
    ] {
        let mut ctx = ToolCtx::new().with_flow_registry(registry.clone());
        ctx.flow_identity = Some(identity);
        FlowInterject
            .call(
                ToolArgs {
                    positional: vec![Value::Str("root".into()), Value::Str(text.into())],
                    named: vec![],
                },
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(expected.pending_injections().last().unwrap().text, text);
    }
    assert_eq!(first_entry.pending_injections().len(), 1);
    assert_eq!(second_entry.pending_injections().len(), 1);
    assert!(Arc::ptr_eq(
        &registry.root_entry_for_turn(&first_turn).unwrap(),
        &first_entry
    ));
    assert!(Arc::ptr_eq(
        &registry.root_entry_for_turn(&second_turn).unwrap(),
        &second_entry
    ));
}

#[test]
fn logical_root_target_follows_the_active_redirect_segment() {
    let session = Session::open_ephemeral();
    let turn = session.begin_turn(atman_runtime::message::Message::user_text(
        TurnId::now(),
        "task",
    ));
    let first_run = FlowRunId::now();
    session
        .flow_registry
        .register_root(
            session.id().to_string(),
            first_run.clone(),
            EffectiveAuthority::root(&Default::default(), false, None),
        )
        .unwrap();
    let first_entry = session
        .flow_registry
        .create_entry(
            first_run.to_string(),
            "first".into(),
            "model".into(),
            first_run.clone(),
            FlowEntryOptions {
                turn_id: Some(turn.clone()),
                events: Some(session.sink().clone()),
                ..Default::default()
            },
        )
        .unwrap();
    session.flow_registry.mark_terminal(&first_run);
    session.flow_registry.remove(&first_entry.handle);

    let redirected_run = FlowRunId::now();
    session
        .flow_registry
        .register_root(
            session.id().to_string(),
            redirected_run.clone(),
            EffectiveAuthority::root(&Default::default(), false, None),
        )
        .unwrap();
    let redirected_entry = session
        .flow_registry
        .create_entry(
            redirected_run.to_string(),
            "redirected".into(),
            "model".into(),
            redirected_run,
            FlowEntryOptions {
                turn_id: Some(turn.clone()),
                events: Some(session.sink().clone()),
                ..Default::default()
            },
        )
        .unwrap();

    session
        .enqueue_injection_for_run(
            "redirect correction",
            atman_runtime::injection::InjectionLevel::L2CourseCorrect,
            None,
            Some((&turn, first_run)),
        )
        .unwrap();
    assert_eq!(
        redirected_entry.pending_injections()[0].text,
        "redirect correction"
    );
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
    let _config_lock = CONFIG_TEST_LOCK.lock().await;
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
    let commands_dir = tmp.path().join("commands");
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

    let _config = ConfigDirGuard::set(tmp.path());

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
    let _config_lock = CONFIG_TEST_LOCK.lock().await;
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
    let commands_dir = tmp.path().join("commands");
    std::fs::create_dir_all(&commands_dir).unwrap();
    let flow_src = r#"flow describe() -> string { return "test" }
flow test_flow(goal: string) -> string {
    session.push(message.user(goal))
    reply = llm.call(model: "mock", context: "session")
    when has_pending_injections() {
        next = llm.call(model: "mock", context: "session")
        return text_concat(next)
    }
    return text_concat(reply)
}
"#;
    let mut f = std::fs::File::create(commands_dir.join("test_l1.at")).unwrap();
    f.write_all(flow_src.as_bytes()).unwrap();

    let _config = ConfigDirGuard::set(tmp.path());

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

    let entry = registry.lookup(&handle).unwrap();
    entry
        .interject(
            "NUDGE: check config",
            atman_runtime::injection::InjectionLevel::L1Nudge,
            None,
        )
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Check sub-agent status first.
    let entry = registry.lookup(&handle).unwrap();
    let status = entry.status.lock().unwrap().clone();
    let pending_count = entry.pending_injections().len();
    let msgs = entry.context.messages_handle().lock().unwrap();
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
    let pending = entry.pending_injections();
    assert!(
        pending.is_empty(),
        "pending_injections should be empty after drain"
    );
}
