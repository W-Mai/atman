use std::sync::Arc;

use atman_dsl::parse::parse_file;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::{Executor, Value};

static TEST_CFG_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn install_mock_model() {
    use atman_runtime::model_registry::{ModelConfig, ModelEntry};

    atman_runtime::model_registry::set_model_config(ModelConfig {
        models: [(
            "mock".into(),
            ModelEntry {
                model: "mock".into(),
                provider: Some("mock".into()),
                context_budget: Some(8_192),
                ..Default::default()
            },
        )]
        .into_iter()
        .collect(),
        providers: std::collections::HashMap::new(),
        aliases: std::collections::HashMap::new(),
    });
}

#[tokio::test]
async fn llm_accepts_messages_kwarg_from_message_nodes() {
    let _cfg_lock = TEST_CFG_LOCK.lock().await;
    install_mock_model();
    let src = r#"flow ask() -> string {
    return llm.call(
        model: "mock",
        messages: [user_msg("hello via messages")],
    )
}
"#;
    let file = parse_file(src).unwrap();
    let ex = Executor::new();
    ex.providers.register(Arc::new(
        MockProvider::new("mock").with_model("mock", Value::Str("got it".into())),
    ));
    let out = ex.run(&file, "ask", vec![]).await.unwrap();
    assert!(matches!(&out, Value::Str(s) if s == "got it"));
}

#[tokio::test]
async fn llm_system_kwarg_flows_through_to_provider() {
    let _cfg_lock = TEST_CFG_LOCK.lock().await;
    install_mock_model();
    let src = r#"flow ask() -> string {
    return llm.call(
        model: "mock",
        system: "you are terse",
        messages: [user_msg("hi")],
    )
}
"#;
    let file = parse_file(src).unwrap();
    let ex = Executor::new();
    ex.providers.register(Arc::new(
        MockProvider::new("mock").with_model("mock", Value::Str("k".into())),
    ));
    let out = ex.run(&file, "ask", vec![]).await.unwrap();
    assert!(matches!(&out, Value::Str(s) if s == "k"));
}

#[tokio::test]
async fn llm_rejects_both_prompt_and_messages_together() {
    let src = r#"flow ask() -> string {
    return llm.call(
        model: "mock",
        prompt: "old",
        messages: [user_msg("new")],
    )
}
"#;
    let file = parse_file(src).unwrap();
    let ex = Executor::new();
    ex.providers.register(Arc::new(
        MockProvider::new("mock").with_model("mock", Value::Str("x".into())),
    ));
    let err = ex.run(&file, "ask", vec![]).await.unwrap_err();
    assert!(format!("{err}").contains("both `messages:` and `prompt:`"));
}

#[tokio::test]
async fn llm_requires_prompt_or_messages() {
    let src = r#"flow ask() -> string {
    return llm.call(model: "mock",)
}
"#;
    let file = parse_file(src).unwrap();
    let ex = Executor::new();
    ex.providers.register(Arc::new(
        MockProvider::new("mock").with_model("mock", Value::Str("x".into())),
    ));
    let err = ex.run(&file, "ask", vec![]).await.unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("`prompt:` or `messages:`"), "got: {msg}");
}
