mod common;

use atman_runtime::event::{Event, EventSink};
use atman_runtime::flow_authority::FlowExecutionState;
use atman_runtime::provider::ProviderRegistry;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::tool::{Tool, ToolArgs, ToolCtx, ToolRegistry};
use atman_runtime::tools::agent_ctrl::{AgentSpawn, FlowRegistry, FlowRunStatus};
use atman_runtime::value::Value;
use std::sync::Arc;

async fn spawn_test_setup(
    with_provider: bool,
) -> (
    tempfile::TempDir,
    ToolCtx,
    String,
    Arc<FlowRegistry>,
    common::ModelRegistryGuard,
) {
    let tmp = tempfile::tempdir().unwrap();
    let flow_path = tmp.path().join("spawn_test.at");
    std::fs::write(
        &flow_path,
        r#"flow test_flow(goal: string) -> string {
    reply = llm.call(
        model: "mock",
        prompt: goal,
    )
    return reply
}
"#,
    )
    .unwrap();

    let registry_guard = common::ModelRegistryGuard::mock("mock").await;

    let registry = Arc::new(FlowRegistry::new());
    let tools = ToolRegistry::new();
    tools.register(Arc::new(AgentSpawn));
    tools.register(Arc::new(atman_runtime::tools::llm_call::LlmCallTool));
    let providers = ProviderRegistry::new();
    if with_provider {
        providers.register(Arc::new(
            MockProvider::new("mock").with_fallback(Value::Str("ok — sub-agent completed".into())),
        ));
    }
    let root_run_id = atman_runtime::event::FlowRunId::now();
    let root_identity = registry
        .register_root(
            "test-session".into(),
            root_run_id.clone(),
            atman_runtime::flow_authority::EffectiveAuthority::root(
                &atman_runtime::trust::TrustConfig::default(),
                false,
                None,
            ),
        )
        .unwrap();
    let mut ctx = ToolCtx::new()
        .with_registry(Arc::new(tools))
        .with_providers(Arc::new(providers))
        .with_flow_registry(Arc::clone(&registry));
    ctx.flow_run_id = Some(root_run_id);
    ctx.flow_identity = Some(root_identity);

    (
        tmp,
        ctx,
        flow_path.display().to_string(),
        registry,
        registry_guard,
    )
}

async fn wait_for_status(registry: &FlowRegistry, handle: &str) -> FlowRunStatus {
    for _ in 0..50 {
        let entry = registry.lookup(handle).unwrap();
        let status = entry.status.lock().unwrap().clone();
        if !status.is_running() {
            return status;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    registry
        .lookup(handle)
        .unwrap()
        .status
        .lock()
        .unwrap()
        .clone()
}

#[tokio::test]
async fn sync_and_async_flow_end_observe_terminal_registry_state() {
    for is_async in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let flow_path = tmp.path().join("terminal_state.at");
        std::fs::write(
            &flow_path,
            "flow terminal_state() -> string { return \"done\" }",
        )
        .unwrap();
        let registry = Arc::new(FlowRegistry::new());
        let root_run_id = atman_runtime::event::FlowRunId::now();
        let root_identity = registry
            .register_root(
                "test-session".into(),
                root_run_id.clone(),
                atman_runtime::flow_authority::EffectiveAuthority::root(
                    &Default::default(),
                    false,
                    None,
                ),
            )
            .unwrap();
        let tools = ToolRegistry::new();
        tools.register(Arc::new(AgentSpawn));
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let events = EventSink::new().with_forwarder(event_tx);
        let mut ctx = ToolCtx::new()
            .with_registry(Arc::new(tools))
            .with_providers(Arc::new(ProviderRegistry::new()))
            .with_flow_registry(Arc::clone(&registry))
            .with_events(events);
        ctx.flow_run_id = Some(root_run_id);
        ctx.flow_identity = Some(root_identity);
        let args = ToolArgs {
            positional: Vec::new(),
            named: vec![
                (
                    "flow".into(),
                    Value::Str(format!("{}@terminal_state", flow_path.display())),
                ),
                ("async".into(), Value::Bool(is_async)),
            ],
        };

        AgentSpawn.call(args, &ctx).await.unwrap();
        let terminal_at_flow_end = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(envelope) = event_rx.recv().await {
                if let Event::FlowEnd { run_id, .. } = envelope.event {
                    return registry.execution_state(&run_id);
                }
            }
            None
        })
        .await
        .unwrap();

        assert_eq!(terminal_at_flow_end, Some(FlowExecutionState::Terminal));
    }
}

#[tokio::test]
async fn agent_spawn_returns_final_assistant_text_when_no_tools_used() {
    let (_tmp, ctx, flow_path, registry, _guard) = spawn_test_setup(true).await;
    let args = ToolArgs {
        positional: Vec::new(),
        named: vec![
            ("goal".into(), Value::Str("count to three".into())),
            (
                "flow".into(),
                Value::Str(format!("{}@test_flow", flow_path)),
            ),
            ("model".into(), Value::Str("mock".into())),
            ("max_iterations".into(), Value::Int(3)),
        ],
    };
    let result = AgentSpawn.call(args, &ctx).await.unwrap();
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
        other => panic!("expected handle struct, got {other:?}"),
    };

    let status = wait_for_status(&registry, &handle).await;
    match status {
        FlowRunStatus::Ok { final_text, .. } => {
            assert!(
                final_text.contains("sub-agent completed"),
                "got: {final_text}"
            );
        }
        other => panic!("expected completed sub-agent, got {other:?}"),
    }
}

#[tokio::test]
async fn agent_spawn_reports_missing_provider_gracefully() {
    let (_tmp, ctx, flow_path, registry, _guard) = spawn_test_setup(false).await;
    let args = ToolArgs {
        positional: Vec::new(),
        named: vec![
            ("goal".into(), Value::Str("anything".into())),
            (
                "flow".into(),
                Value::Str(format!("{}@test_flow", flow_path)),
            ),
            ("model".into(), Value::Str("mock".into())),
        ],
    };
    let result = AgentSpawn.call(args, &ctx).await.unwrap();
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
        other => panic!("expected handle struct, got {other:?}"),
    };

    let status = wait_for_status(&registry, &handle).await;
    match status {
        FlowRunStatus::Err { message, .. } => {
            assert!(
                message.contains("no provider registered for model"),
                "got: {message}"
            );
        }
        other => panic!("expected failed sub-agent, got {other:?}"),
    }
}
