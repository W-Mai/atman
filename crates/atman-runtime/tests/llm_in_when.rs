use atman_dsl::parse::parse_file;
use atman_runtime::Executor;
use atman_runtime::model_registry::{MODEL_CONFIG_LOCK, ModelConfig, ModelEntry, set_model_config};
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::value::Value;
use std::sync::Arc;

fn register_model() {
    let _lock = MODEL_CONFIG_LOCK.lock().unwrap();
    set_model_config(ModelConfig {
        models: [(
            "m".into(),
            ModelEntry {
                model: "m".into(),
                context_budget: Some(8_192),
                ..Default::default()
            },
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    });
}

fn run(src: &str, provider: MockProvider) -> Value {
    register_model();
    let parsed = parse_file(src).expect("parse");
    let ex = Executor::new();
    ex.providers.register(Arc::new(provider));
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(ex.run(&parsed, "test", vec![]))
        .expect("flow failed")
}

#[test]
fn llm_call_in_when_condition() {
    let src = r#"
flow test() -> string {
    result = llm.call(model: "m", prompt: "is this safe?", context: "none")
    when result == "yes" {
        return "approved"
    }
    return "denied: " + result
}
"#;
    let provider = MockProvider::new("mock").with_model("m", Value::Str("yes".into()));
    let result = run(src, provider);
    match result {
        Value::Str(s) => assert_eq!(s, "approved"),
        other => panic!("expected string, got {other:?}"),
    }
}

#[test]
fn llm_call_inline_in_when_condition() {
    let src = r#"
flow test() -> string {
    when llm.call(model: "m", prompt: "check", context: "none") == "yes" {
        return "inline approved"
    }
    return "denied"
}
"#;
    let provider = MockProvider::new("mock").with_model("m", Value::Str("yes".into()));
    let result = run(src, provider);
    match result {
        Value::Str(s) => assert_eq!(s, "inline approved"),
        other => panic!("expected string, got {other:?}"),
    }
}
