mod common;

use atman_runtime::RuntimeError;
use atman_runtime::event::{Event, EventSink, Observable, TurnId};
use atman_runtime::flow_authority::FlowExecutionState;
use atman_runtime::message::{Message, MessageOrigin, MessagePart, MessageRole};
use atman_runtime::provider::{
    AssistantMessage, LlmRequest, Provider, ProviderRegistry, StopReason, TokenUsage,
    wrap_call_as_streaming,
};
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolRegistry, ToolResult};
use atman_runtime::tools::agent_ctrl::{AgentSpawn, FlowRegistry, FlowRunStatus};
use atman_runtime::value::Value;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

struct Probe;

impl Tool for Probe {
    fn name(&self) -> &str {
        "probe"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some("Return a stable probe result.")
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    fn call<'a>(&'a self, _args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async { Ok(Value::Str("probe complete".into())) })
    }
}

struct ToolLoopProvider {
    calls: Arc<Mutex<Vec<LlmRequest>>>,
    next: AtomicUsize,
}

impl ToolLoopProvider {
    fn respond(&self, request: LlmRequest) -> Result<AssistantMessage, RuntimeError> {
        let turn_id = request
            .messages
            .first()
            .map(|message| message.turn_id.clone())
            .unwrap_or_else(TurnId::now);
        let index = self.next.fetch_add(1, Ordering::Relaxed);
        self.calls.lock().unwrap().push(request);
        let (parts, stop_reason) = if index == 0 {
            (
                vec![MessagePart::ToolUse {
                    id: "probe-call".into(),
                    name: "probe".into(),
                    input: serde_json::json!({}),
                    intent: None,
                }],
                StopReason::ToolUse,
            )
        } else {
            (
                vec![MessagePart::Text {
                    text: "child finished".into(),
                }],
                StopReason::End,
            )
        };
        Ok(AssistantMessage {
            message: Message {
                role: MessageRole::Assistant,
                parts,
                turn_id,
                origin: MessageOrigin::User,
            },
            stop_reason,
            token_usage: TokenUsage::default(),
            timing: Default::default(),
            model: "tool-loop".into(),
            response_id: None,
        })
    }
}

impl Provider for ToolLoopProvider {
    fn name(&self) -> &str {
        "tool-loop"
    }

    fn call<'a>(
        &'a self,
        request: LlmRequest,
    ) -> BoxFut<'a, Result<AssistantMessage, RuntimeError>> {
        Box::pin(async move { self.respond(request) })
    }

    fn call_streaming(&self, request: LlmRequest) -> Observable<AssistantMessage> {
        let response = self.respond(request);
        wrap_call_as_streaming(Box::pin(async move { response }))
    }
}

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
    let broker = atman_runtime::permission::PermissionBroker::shared(Arc::clone(&registry));
    let mut ctx = ToolCtx::new()
        .with_registry(Arc::new(tools))
        .with_providers(Arc::new(providers))
        .with_flow_registry(Arc::clone(&registry))
        .with_permission_broker(broker)
        .with_approval(Arc::new(atman_runtime::session::ApprovalRegistry::new()))
        .with_trust(atman_runtime::trust::TrustConfig::default());
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

#[tokio::test]
async fn spawned_managed_context_persists_assistant_tool_transactions() {
    let tmp = tempfile::tempdir().unwrap();
    let flow_path = tmp.path().join("tool_loop.at");
    std::fs::write(
        &flow_path,
        r#"flow tool_loop(goal: string) -> string {
    contract {
        invocation { user_message: goal }
    }
    loop {
        reply = llm.call(
            model: "tool-loop",
            context: "session",
            cache: true,
            tools: ["probe"],
        )
        tool_uses = extract_tool_uses(reply)
        when is_empty(tool_uses) {
            return text_concat(reply)
        }
        tool_results = dispatch_all(tool_uses)
        session.push(tool_results)
    }
}
"#,
    )
    .unwrap();
    let _guard = common::ModelRegistryGuard::acquire(common::config([common::model_for_provider(
        "tool-loop",
        "tool-loop",
        8_192,
        None,
    )]))
    .await;

    let flow_registry = Arc::new(FlowRegistry::new());
    let root_run_id = atman_runtime::event::FlowRunId::now();
    let root_identity = flow_registry
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
    tools.register(Arc::new(atman_runtime::tools::llm_call::LlmCallTool));
    tools.register(Arc::new(atman_runtime::tools::stdlib::ExtractToolUses));
    tools.register(Arc::new(atman_runtime::tools::stdlib::DispatchAll));
    tools.register(Arc::new(atman_runtime::tools::stdlib::IsEmpty));
    tools.register(Arc::new(atman_runtime::tools::stdlib::TextConcat));
    tools.register(Arc::new(atman_runtime::tools::session::SessionPush));
    tools.register(Arc::new(Probe));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let providers = ProviderRegistry::new();
    providers.register(Arc::new(ToolLoopProvider {
        calls: Arc::clone(&calls),
        next: AtomicUsize::new(0),
    }));
    let events = EventSink::new();
    let broker = atman_runtime::permission::PermissionBroker::shared(Arc::clone(&flow_registry));
    let turn_id = atman_runtime::event::TurnId::now();
    let mut ctx = ToolCtx::new()
        .with_registry(Arc::new(tools))
        .with_providers(Arc::new(providers))
        .with_flow_registry(Arc::clone(&flow_registry))
        .with_permission_broker(broker)
        .with_approval(Arc::new(atman_runtime::session::ApprovalRegistry::new()))
        .with_trust(atman_runtime::trust::TrustConfig::default())
        .with_events(events.clone());
    ctx.turn_id = Some(turn_id.clone());
    ctx.flow_run_id = Some(root_run_id);
    ctx.flow_identity = Some(root_identity);

    let result = AgentSpawn
        .call(
            ToolArgs {
                positional: Vec::new(),
                named: vec![
                    (
                        "flow".into(),
                        Value::Str(format!("{}@tool_loop", flow_path.display())),
                    ),
                    ("async".into(), Value::Bool(false)),
                    (
                        "arguments".into(),
                        Value::Struct(vec![("goal".into(), Value::Str("run probe".into()))]),
                    ),
                ],
            },
            &ctx,
        )
        .await
        .unwrap();

    assert!(matches!(result, Value::Str(text) if text == "child finished"));
    let started_turns: Vec<_> = events
        .snapshot()
        .into_iter()
        .filter_map(|event| match event {
            Event::FlowStart { turn_id, .. } => Some(turn_id),
            _ => None,
        })
        .collect();
    assert_eq!(started_turns, vec![Some(turn_id)]);
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        &calls[1].messages[..calls[0].messages.len()],
        calls[0].messages.as_slice()
    );
    let second_parts: Vec<_> = calls[1]
        .messages
        .iter()
        .flat_map(|message| &message.parts)
        .collect();
    assert_eq!(
        second_parts
            .iter()
            .filter(|part| matches!(part, MessagePart::ToolUse { id, .. } if id == "probe-call"))
            .count(),
        1
    );
    assert_eq!(
        second_parts
            .iter()
            .filter(|part| matches!(part, MessagePart::ToolResult { tool_use_id, .. } if tool_use_id == "probe-call"))
            .count(),
        1
    );
    drop(calls);

    let child_events: Vec<_> = events
        .snapshot()
        .into_iter()
        .filter(|event| {
            matches!(
                event,
                Event::AssistantMsg {
                    flow_run_id: Some(_),
                    ..
                }
            )
        })
        .collect();
    assert_eq!(child_events.len(), 2);
    let observations: Vec<_> = events
        .snapshot()
        .into_iter()
        .filter_map(|event| match event {
            Event::LlmCall {
                provider,
                context_cache: Some(observation),
                ..
            } if provider == "tool-loop" => Some(observation),
            _ => None,
        })
        .collect();
    assert_eq!(observations.len(), 2);
    assert_eq!(observations[0].common_prefix_bytes, 0);
    assert!(observations[1].common_prefix_bytes > 0);
}
