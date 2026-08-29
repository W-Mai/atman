use atman_runtime::Session;

#[tokio::test]
async fn cumulative_input_tokens_accumulates_across_calls() {
    let tmp = tempfile::tempdir().unwrap();
    let session = Session::open(tmp.path()).unwrap();
    assert_eq!(session.cumulative_input_tokens(), 0);
    session.record_llm_call("claude-opus-4.7", 500, 100, 0, 0, None, None);
    assert_eq!(session.cumulative_input_tokens(), 500);
    session.record_llm_call("claude-opus-4.7", 250, 40, 0, 0, None, None);
    assert_eq!(session.cumulative_input_tokens(), 750);
    assert_eq!(session.last_model(), "claude-opus-4.7");
    session.reset_input_tokens_to(120);
    assert_eq!(session.cumulative_input_tokens(), 120);
}

#[test]
fn model_switch_clears_only_unrepresentable_session_reasoning() {
    let _lock = atman_runtime::model_registry::MODEL_CONFIG_LOCK
        .lock()
        .unwrap();
    let mut config = atman_runtime::model_registry::ProviderConfig::default();
    config.providers.insert(
        "toggle".into(),
        atman_runtime::model_registry::ProviderEntry {
            kind: "openai-compat".into(),
            reasoning_format: Some(
                atman_runtime::providers::openai::OpenAiReasoningFormat::CompatibleThinking,
            ),
            ..Default::default()
        },
    );
    config.providers.insert(
        "official".into(),
        atman_runtime::model_registry::ProviderEntry {
            kind: "openai".into(),
            reasoning_format: Some(
                atman_runtime::providers::openai::OpenAiReasoningFormat::Official,
            ),
            ..Default::default()
        },
    );
    for (model, provider) in [("toggle-model", "toggle"), ("official-model", "official")] {
        config.models.insert(
            model.into(),
            atman_runtime::model_registry::ModelEntry {
                model: model.into(),
                provider: Some(provider.into()),
                context_budget: Some(128_000),
                thinking: Some(true),
                ..Default::default()
            },
        );
    }
    atman_runtime::model_registry::set_provider_config(config);

    let session = Session::open_ephemeral();
    let high = atman_runtime::provider::ReasoningSelection::Effort {
        effort: atman_runtime::provider::ReasoningEffort::High,
        execution_mode: None,
    };
    session.set_reasoning_override(Some(high.clone()));
    assert!(session.set_current_model("official-model").is_none());
    assert_eq!(session.reasoning_override(), Some(high.clone()));

    let cleared = session.set_current_model("toggle-model").unwrap();
    assert_eq!(cleared.0, high);
    assert!(cleared.1.contains("cannot represent effort `high`"));
    assert_eq!(session.reasoning_override(), None);
    assert_eq!(session.last_model(), "toggle-model");
}
