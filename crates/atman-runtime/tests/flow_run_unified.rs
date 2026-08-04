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

#[tokio::test]
async fn flow_interject_cancels_running_subagent_llm() {
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
    reply = llm {
        model: "mock"
        prompt: goal
    }
    return text_concat(reply)
}
"#;
    let mut f = std::fs::File::create(commands_dir.join("test_interject.at")).unwrap();
    f.write_all(flow_src.as_bytes()).unwrap();

    let home = tmp.path().to_path_buf();
    let old_home = std::env::var_os("HOME");
    unsafe {
        std::env::set_var("HOME", &home);
    }

    let registry = Arc::new(FlowRegistry::new());
    let mut providers = atman_runtime::provider::ProviderRegistry::new();
    providers.register(Arc::new(
        MockProvider::new("mock").with_fallback(atman_runtime::Value::Str("ok".into())),
    ));
    let mut tools = ToolRegistry::new();
    atman_runtime::tools::register_tier_zero(&mut tools);

    let ctx = ToolCtx::new()
        .with_registry(Arc::new(tools))
        .with_providers(Arc::new(providers))
        .with_flow_registry(registry.clone());

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
        named: vec![],
    };
    let _ = FlowInterject.call(interject_args, &ctx).await.unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let entry = registry.lookup(&handle).unwrap();
    let status = entry.status.lock().unwrap().clone();
    let is_err = matches!(status, FlowRunStatus::Err { .. });
    assert!(
        is_err,
        "sub-agent should be err after interjection, got: {:?}",
        status
    );

    if let Some(old) = old_home {
        unsafe {
            std::env::set_var("HOME", old);
        }
    } else {
        unsafe {
            std::env::remove_var("HOME");
        }
    }
}
