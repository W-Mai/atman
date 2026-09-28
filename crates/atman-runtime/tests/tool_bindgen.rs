use atman_runtime::tool::{ApprovalLevel, tool_spec};
use atman_runtime::{
    CancelBehavior, Executor, RuntimeError, Tier, ToolArgs, ToolCallMode, ToolCtx, ToolRegistry,
    Value,
};

#[atman_runtime::tools]
mod host_tools {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use atman_runtime::{RuntimeError, ToolCtx};

    pub static DEFERRED_CALLS: AtomicUsize = AtomicUsize::new(0);
    pub static IMMEDIATE_CALLS: AtomicUsize = AtomicUsize::new(0);

    /// Doubles an integer.
    #[tool(name = "math.double", tier = 0)]
    pub async fn double(value: i64) -> i64 {
        value * 2
    }

    #[tool(name = "probe.deferred", tier = 0)]
    pub async fn deferred(value: i64) -> i64 {
        DEFERRED_CALLS.fetch_add(1, Ordering::SeqCst);
        value + 1
    }

    #[tool(name = "probe.immediate", tier = 0)]
    pub fn immediate(value: i64) -> i64 {
        IMMEDIATE_CALLS.fetch_add(1, Ordering::SeqCst);
        value + 1
    }

    /// Formats a list with an optional label.
    #[tool(tier = 2, cancel = "atomic")]
    pub fn format_values(
        values: Vec<i64>,
        label: Option<String>,
        ctx: ToolCtx,
    ) -> Result<String, RuntimeError> {
        assert!(!ctx.cancel.is_cancelled());
        Ok(format!("{}: {values:?}", label.unwrap_or_default()))
    }

    pub fn helper() -> i64 {
        17
    }
}

#[tokio::test]
async fn generated_tools_use_registry_metadata_and_dispatch() {
    let registry = ToolRegistry::new();
    host_tools::register(&registry).unwrap();
    assert!(!registry.has("helper"));
    assert_eq!(host_tools::helper(), 17);

    let double = registry.get("math.double").unwrap();
    assert_eq!(double.tier(), Tier::Zero);
    assert_eq!(double.call_mode(), ToolCallMode::Deferred);
    assert_eq!(
        double.approval_level(&ToolArgs::default(), &ToolCtx::default()),
        ApprovalLevel::Auto
    );
    let spec = tool_spec(double.as_ref());
    assert_eq!(spec.description.as_deref(), Some("Doubles an integer."));
    assert_eq!(spec.input_schema["properties"]["value"]["type"], "integer");
    assert_eq!(
        spec.input_schema["required"],
        serde_json::json!(["value", "_atman_intent"])
    );

    let result = double
        .call(
            ToolArgs {
                positional: vec![Value::Int(2)],
                named: vec![("value".into(), Value::Int(21))],
            },
            &ToolCtx::default(),
        )
        .await
        .unwrap();
    assert!(matches!(result, Value::Int(42)));

    let error = double
        .call(ToolArgs::default(), &ToolCtx::default())
        .await
        .unwrap_err();
    assert!(matches!(error, RuntimeError::MissingArg(name) if name == "value"));

    let error = double
        .call(
            ToolArgs {
                named: vec![("value".into(), Value::Str("wrong".into()))],
                ..Default::default()
            },
            &ToolCtx::default(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, RuntimeError::TypeMismatch { expected, actual } if expected == "int" && actual == "string")
    );
}

#[tokio::test]
async fn generated_tools_keep_cancel_policy_and_optional_arguments() {
    let registry = ToolRegistry::new();
    host_tools::register(&registry).unwrap();
    let tool = registry.get("format_values").unwrap();
    assert_eq!(tool.tier(), Tier::Two);
    assert_eq!(tool.call_mode(), ToolCallMode::Immediate);
    assert_eq!(tool.cancel_behavior(), CancelBehavior::Atomic);
    assert_eq!(
        tool.approval_level(&ToolArgs::default(), &ToolCtx::default()),
        ApprovalLevel::Approve
    );
    let spec = tool_spec(tool.as_ref());
    assert_eq!(spec.input_schema["properties"]["values"]["type"], "array");
    assert_eq!(
        spec.input_schema["properties"]["values"]["items"]["type"],
        "integer"
    );
    assert_eq!(
        spec.input_schema["properties"]["label"]["anyOf"][1]["type"],
        "null"
    );
    assert_eq!(
        spec.input_schema["required"],
        serde_json::json!(["values", "_atman_intent"])
    );

    let result = tool
        .call(
            ToolArgs {
                positional: vec![Value::List(vec![Value::Int(3), Value::Int(5)])],
                ..Default::default()
            },
            &ToolCtx::default(),
        )
        .await
        .unwrap();
    assert!(matches!(result, Value::Str(text) if text == ": [3, 5]"));
}

#[tokio::test]
async fn generated_tools_respect_call_timing_in_atman_flows() {
    use std::sync::atomic::Ordering;

    let executor = Executor::new();
    host_tools::register(&executor.tools).unwrap();

    let file = atman_rt::parse_file(
        "flow start() -> int { pending = probe.deferred(value: 7)\n return 0 }",
    )
    .unwrap();
    let value = executor.run(&file, "start", vec![]).await.unwrap();
    assert!(matches!(value, Value::Int(0)));
    assert_eq!(host_tools::DEFERRED_CALLS.load(Ordering::SeqCst), 0);

    let file =
        atman_rt::parse_file("flow start() -> int { return probe.immediate(value: 7) }").unwrap();
    let value = executor.run(&file, "start", vec![]).await.unwrap();
    assert!(matches!(value, Value::Int(8)));
    assert_eq!(host_tools::IMMEDIATE_CALLS.load(Ordering::SeqCst), 1);

    let file =
        atman_rt::parse_file("flow start() -> int { return probe.deferred(value: 7).await }")
            .unwrap();
    let value = executor.run(&file, "start", vec![]).await.unwrap();
    assert!(matches!(value, Value::Int(8)));
    assert_eq!(host_tools::DEFERRED_CALLS.load(Ordering::SeqCst), 1);
}
