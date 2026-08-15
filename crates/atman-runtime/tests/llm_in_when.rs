use atman_dsl::parse::parse_file;
use atman_runtime::Executor;
mod common;

use atman_runtime::providers::mock::MockProvider;
use atman_runtime::value::Value;
use std::sync::Arc;

fn run(src: &str, provider: MockProvider) -> Value {
    let _registry =
        common::SyncModelRegistryGuard::acquire(common::config([common::model_for_provider(
            "m", "mock", 8_192, None,
        )]));
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
