use atman_runtime::provider::ProviderRegistry;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::tool::{Tool, ToolArgs, ToolCtx, ToolRegistry};
use atman_runtime::tools::agent_ctrl::AgentSpawn;
use atman_runtime::value::Value;
use std::sync::Arc;

#[tokio::test]
async fn agent_spawn_returns_final_assistant_text_when_no_tools_used() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(AgentSpawn));
    let mut providers = ProviderRegistry::new();
    providers.register(Arc::new(
        MockProvider::new("mock").with_fallback(Value::Str("ok — sub-agent completed".into())),
    ));
    let ctx = ToolCtx::new()
        .with_registry(Arc::new(tools.clone()))
        .with_providers(Arc::new(providers));
    let args = ToolArgs {
        positional: Vec::new(),
        named: vec![
            ("goal".into(), Value::Str("count to three".into())),
            ("model".into(), Value::Str("mock".into())),
            ("max_iterations".into(), Value::Int(3)),
        ],
    };
    let result = AgentSpawn.call(args, &ctx).await.unwrap();
    match result {
        Value::Str(s) => assert!(
            s.contains("sub-agent completed"),
            "unexpected sub-agent result: {s}"
        ),
        other => panic!("expected Value::Str, got {other:?}"),
    }
}

#[tokio::test]
async fn agent_spawn_reports_missing_provider_gracefully() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(AgentSpawn));
    let providers = ProviderRegistry::new();
    let ctx = ToolCtx::new()
        .with_registry(Arc::new(tools.clone()))
        .with_providers(Arc::new(providers));
    let args = ToolArgs {
        positional: Vec::new(),
        named: vec![
            ("goal".into(), Value::Str("anything".into())),
            ("model".into(), Value::Str("does-not-exist".into())),
        ],
    };
    let result = AgentSpawn.call(args, &ctx).await.unwrap();
    match result {
        Value::Str(s) => assert!(s.contains("no provider for model"), "got: {s}"),
        other => panic!("expected Value::Str, got {other:?}"),
    }
}
