use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use atman_rt::parse_file;
use atman_runtime::event::Observable;
use atman_runtime::model_registry::{
    AliasEntry, ModelEntry, ModelRegistry, ProviderConfig, ProviderEntry,
};
use atman_runtime::provider::{
    AssistantMessage, LlmRequest, OpenAiApi, Provider, ProviderRegistry,
};
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::tool::BoxFut;
use atman_runtime::{Executor, Message, RuntimeError, Session, Value};

#[derive(Clone)]
struct RecordingProvider {
    api: Option<OpenAiApi>,
    requests: Arc<Mutex<Vec<(OpenAiApi, LlmRequest)>>>,
    mock: Arc<MockProvider>,
}

impl RecordingProvider {
    fn record(&self, request: &LlmRequest) {
        self.requests
            .lock()
            .unwrap()
            .push((self.api.expect("protocol selection"), request.clone()));
    }
}

impl Provider for RecordingProvider {
    fn name(&self) -> &str {
        "gateway"
    }

    fn for_api(&self, api: OpenAiApi) -> Option<Arc<dyn Provider>> {
        Some(Arc::new(Self {
            api: Some(api),
            ..self.clone()
        }))
    }

    fn call<'a>(
        &'a self,
        request: LlmRequest,
    ) -> BoxFut<'a, Result<AssistantMessage, RuntimeError>> {
        self.record(&request);
        self.mock.call(request)
    }

    fn call_streaming(&self, request: LlmRequest) -> Observable<AssistantMessage> {
        self.record(&request);
        self.mock.call_streaming(request)
    }
}

fn config(label: &str, api: OpenAiApi, budget: u64) -> ProviderConfig {
    let entry = ModelEntry {
        model: format!("vendor/{label}"),
        provider: Some("gateway".into()),
        context_budget: Some(budget),
        ..Default::default()
    };
    ProviderConfig {
        providers: HashMap::from([(
            "gateway".into(),
            ProviderEntry {
                name: "gateway".into(),
                kind: "openai-compat".into(),
                api: Some(api),
                ..Default::default()
            },
        )]),
        models: HashMap::from([
            ("primary".into(), entry.clone()),
            (format!("only-{label}"), entry),
        ]),
        aliases: HashMap::from([(
            "smart".into(),
            AliasEntry {
                model: "primary".into(),
            },
        )]),
    }
}

fn executor(label: &str, api: OpenAiApi, budget: u64) -> (Executor, RecordingProvider) {
    let models = Arc::new(ModelRegistry::new());
    models.replace_config(config(label, api, budget)).unwrap();
    let provider = RecordingProvider {
        api: None,
        requests: Default::default(),
        mock: Arc::new(
            MockProvider::new("gateway").with_fallback(Value::Str(r#"{"value":"ok"}"#.into())),
        ),
    };
    let mut executor = Executor::new();
    executor.providers = ProviderRegistry::with_models(models);
    executor.providers.register(Arc::new(provider.clone()));
    (executor, provider)
}

#[tokio::test]
async fn runtime_builds_bind_requests_to_the_selected_config_source() {
    use atman_runtime::config_hub::ConfigHub;
    use atman_runtime::{AtmanRuntime, AtmanRuntimeOptions};

    let project = tempfile::tempdir().unwrap();
    let left_dir = tempfile::tempdir().unwrap();
    let right_dir = tempfile::tempdir().unwrap();
    let left_hub = ConfigHub::from_config_dir(left_dir.path());
    let right_hub = ConfigHub::from_config_dir(right_dir.path());
    for (hub, label, api, budget) in [
        (&left_hub, "left", "responses", 40_000),
        (&right_hub, "right", "chat-completions", 400_000),
    ] {
        std::fs::write(
            hub.config_toml_path(),
            format!(
                r#"
[providers.gateway]
kind = "openai-compat"
api_key = "test-key"
base_url = "https://gateway.example/v1"
api = "{api}"
[models.primary]
model = "vendor/{label}"
provider = "gateway"
context_budget = {budget}
[alias.smart]
model = "primary"
"#
            ),
        )
        .unwrap();
    }
    let options = |hub: &ConfigHub| AtmanRuntimeOptions {
        events: atman_runtime::event::EventSink::new(),
        mock: false,
        config_dir: Some(hub.config_dir().to_path_buf()),
        project_root: project.path().to_path_buf(),
        home_dir: None,
        workspace_generation: "scoped-runtime".into(),
    };
    let (left, right) = tokio::join!(
        AtmanRuntime::build(options(&left_hub)),
        AtmanRuntime::build(options(&right_hub))
    );
    let left = left.unwrap();
    let right = right.unwrap();
    assert!(Arc::ptr_eq(
        left.executor.providers.models(),
        left_hub.models()
    ));
    assert!(Arc::ptr_eq(
        right.executor.providers.models(),
        right_hub.models()
    ));
    assert!(!Arc::ptr_eq(
        left.executor.providers.models(),
        right.executor.providers.models()
    ));
    let source =
        parse_file(r#"flow main() { return llm.call(model: "smart", prompt: "hi") }"#).unwrap();
    for (runtime, model, api, budget) in [
        (&left, "vendor/left", OpenAiApi::Responses, 40_000),
        (&right, "vendor/right", OpenAiApi::ChatCompletions, 400_000),
    ] {
        assert!(
            runtime
                .executor
                .providers
                .try_resolve("smart")
                .unwrap()
                .is_some()
        );
        let recorder = RecordingProvider {
            api: None,
            requests: Default::default(),
            mock: Arc::new(MockProvider::new("gateway").with_fallback(Value::Str("ok".into()))),
        };
        runtime
            .executor
            .providers
            .register(Arc::new(recorder.clone()));
        let context = Arc::new(Session::open_ephemeral());
        runtime
            .executor
            .run_in_turn(&source, "main", vec![], None, Some(context.clone()))
            .await
            .unwrap();
        let requests = recorder.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!((requests[0].0, requests[0].1.model.as_str()), (api, model));
        context.set_current_model("smart");
        context.refresh_window_snapshot();
        assert_eq!(context.subscribe_context().borrow().window_budget, budget);
    }
    // Rebuilding one host preserves ownership; a different source cannot change its model or adapters.
    let peer = AtmanRuntime::build(options(&left_hub)).await.unwrap();
    assert!(Arc::ptr_eq(
        peer.executor.providers.models(),
        left.executor.providers.models()
    ));
    let text = right_hub.read_config_toml().unwrap().replace(
        "api = \"chat-completions\"",
        "api = \"responses\"\nenabled = false",
    );
    std::fs::write(right_hub.config_toml_path(), text).unwrap();
    right_hub.reload().unwrap();
    assert!(
        right
            .executor
            .providers
            .try_resolve("smart")
            .unwrap()
            .is_none()
    );
    assert!(!right.executor.providers.contains("config:gateway"));
    assert!(left.executor.providers.contains("config:gateway"));
    assert!(peer.executor.providers.contains("config:gateway"));
    assert_eq!(
        left_hub
            .models()
            .model_call_config("smart")
            .info
            .context_budget,
        40_000
    );
    // Mock mode adds its model without replacing this scope's configuration.
    let mut mock_options = options(&left_hub);
    mock_options.mock = true;
    let mock = AtmanRuntime::build(mock_options).await.unwrap();
    assert!(mock.executor.providers.contains("mock"));
    assert_eq!(left_hub.models().resolve_alias("smart"), "primary");
}

#[tokio::test]
async fn scoped_execution_keeps_aliases_protocols_context_and_compaction_together() {
    let (left, left_provider) = executor("left", OpenAiApi::Responses, 40_000);
    let (right, right_provider) = executor("right", OpenAiApi::ChatCompletions, 400_000);
    let left_context = Arc::new(Session::open_ephemeral());
    let right_context = Arc::new(Session::open_ephemeral());
    let file = parse_file(r#"
flow helper() -> string {
    value = llm.extract(model: "smart", prompt: "extract", context: "session", fields: { value: string -- "value" })
    return value.value
}
flow main() -> string {
    llm.call(model: "smart", prompt: "reply", context: "session")
    return helper().await
}
"#).unwrap();
    let (first, second) = tokio::join!(
        left.run_in_turn(&file, "main", vec![], None, Some(left_context.clone())),
        right.run_in_turn(&file, "main", vec![], None, Some(right_context.clone())),
    );
    assert!(matches!(first.unwrap(), Value::Str(value) if value == "ok"));
    assert!(matches!(second.unwrap(), Value::Str(value) if value == "ok"));
    for (context, owner, foreign, budget) in [
        (&left_context, "left", "right", 40_000),
        (&right_context, "right", "left", 400_000),
    ] {
        let history = serde_json::to_string(&context.messages().to_vec()).unwrap();
        assert!(history.contains(&format!("only-{owner}")), "{history}");
        assert!(!history.contains(&format!("only-{foreign}")), "{history}");
        context.set_current_model("smart");
        context.refresh_window_snapshot();
        assert_eq!(context.subscribe_context().borrow().window_budget, budget);
    }
    for (provider, model, protocol) in [
        (&left_provider, "vendor/left", OpenAiApi::Responses),
        (&right_provider, "vendor/right", OpenAiApi::ChatCompletions),
    ] {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|(api, request)| *api == protocol && request.model == model)
        );
    }

    // A request snapshot and a live context cannot silently change scope.
    let foreign = right.providers.models().model_call_config("smart");
    assert!(
        matches!(left.providers.resolve_call(&foreign), Err(error) if error.to_string().contains("scope"))
    );
    let rejected = right
        .run_in_turn(&file, "main", vec![], None, Some(left_context.clone()))
        .await;
    assert!(rejected.unwrap_err().to_string().contains("scope"));
    assert_eq!(right_provider.requests.lock().unwrap().len(), 2);

    // Refreshing one directory cannot alter another scope's next request or compaction target.
    right
        .providers
        .models()
        .replace_config(config("changed", OpenAiApi::Responses, 800_000))
        .unwrap();
    for index in 0..60 {
        let turn = atman_rt::TurnId::<atman_runtime::event::AtmanUuid>::now();
        let text = format!("{} {index}", "x".repeat(4000));
        let message = if index % 2 == 0 {
            Message::user_text(turn, text)
        } else {
            Message::assistant_text(turn, text)
        };
        left_context.append_message(message, None);
    }
    let before = left_context.message_count();
    atman_runtime::compaction::maybe_auto_compact(&left_context, "smart", &left.providers).await;
    assert!(left_context.message_count() < before);
    let requests = left_provider.requests.lock().unwrap();
    assert!(requests.len() > 2);
    assert!(
        requests
            .iter()
            .all(|(api, request)| *api == OpenAiApi::Responses && request.model == "vendor/left")
    );
    assert_eq!(
        left_context.subscribe_context().borrow().window_budget,
        40_000
    );
}

#[test]
fn disabled_config_and_call_ownership_apply_to_direct_and_prefixed_connections() {
    let models = Arc::new(ModelRegistry::new());
    let mut configuration = config("disabled", OpenAiApi::Responses, 40_000);
    configuration.providers.get_mut("gateway").unwrap().enabled = Some(false);
    models.replace_config(configuration).unwrap();
    let providers = ProviderRegistry::with_models(models.clone());
    providers.register(Arc::new(MockProvider::new("gateway")));
    providers.register(Arc::new(MockProvider::new("config:gateway")));
    assert!(providers.try_resolve("smart").unwrap().is_none());
    let unrelated = ModelRegistry::new().model_call_config("smart");
    assert!(providers.resolve_call(&unrelated).is_err());
    let clone = providers.clone();
    assert!(Arc::ptr_eq(clone.models(), &models));
}

#[tokio::test]
async fn mismatched_scope_does_not_consume_instance_admission() {
    let (owner, _) = executor("owner", OpenAiApi::Responses, 40_000);
    let (foreign, _) = executor("foreign", OpenAiApi::Responses, 40_000);
    let address = atman_runtime::flow_store::FlowRef {
        session_id: uuid::Uuid::new_v4().into(),
        instance_id: atman_runtime::flow_store::FlowInstanceId::now(),
    };
    let context = Arc::new(Session::open_instance_context(address.clone()));
    assert!(context.model_registry().is_none());
    context
        .bind_model_registry(owner.providers.models().clone())
        .unwrap();
    let mut state = atman_rt::FlowInstance::prepared(address);
    state.activate().unwrap();
    let lease = state
        .begin_turn(atman_rt::TurnId::now(), atman_rt::RunId::now())
        .unwrap();
    let invocation = atman_runtime::RootInvocation {
        session: Some(context),
        instance: Some(atman_runtime::InstanceInvocation::new(
            lease,
            atman_runtime::flow_authority::EffectiveAuthority::root(
                &Default::default(),
                false,
                None,
            ),
            Default::default(),
        )),
        ..Default::default()
    };
    let file = parse_file("flow main() -> int { return 42 }").unwrap();
    let error = foreign
        .run_with_invocation(&file, "main", vec![], invocation.clone())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("scope"));
    let value = owner
        .run_with_invocation(&file, "main", vec![], invocation)
        .await
        .unwrap();
    assert!(matches!(value, Value::Int(42)));
}
