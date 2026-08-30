mod common;

use std::sync::{Arc, Mutex};

use atman_dsl::parse::parse_file;
use atman_runtime::error::RuntimeError;
use atman_runtime::event::{NodeEvent, Observable, TurnId};
use atman_runtime::message::Message;
use atman_runtime::model_registry::{ModelEntry, ProviderEntry};
use atman_runtime::provider::{
    AssistantMessage, LlmRequest, Provider, ReasoningEffort, ReasoningSelection,
};
use atman_runtime::providers::openai::OpenAiReasoningFormat;
use atman_runtime::tool::BoxFut;
use atman_runtime::{Executor, InvocationEnv, Value};

struct ReasoningCaptureProvider {
    captured: Mutex<Vec<LlmRequest>>,
}

impl ReasoningCaptureProvider {
    fn new() -> Self {
        Self {
            captured: Mutex::new(Vec::new()),
        }
    }

    fn captured(&self) -> Vec<ReasoningSelection> {
        self.captured
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.reasoning.clone())
            .collect()
    }

    fn requests(&self) -> Vec<LlmRequest> {
        self.captured.lock().unwrap().clone()
    }

    fn response() -> AssistantMessage {
        AssistantMessage::text_only(Message::assistant_text(TurnId::now(), "ok"))
    }
}

impl Provider for ReasoningCaptureProvider {
    fn name(&self) -> &str {
        "reasoning-capture"
    }

    fn call<'a>(&'a self, req: LlmRequest) -> BoxFut<'a, Result<AssistantMessage, RuntimeError>> {
        self.captured.lock().unwrap().push(req);
        Box::pin(async { Ok(Self::response()) })
    }

    fn call_streaming(&self, req: LlmRequest) -> Observable<AssistantMessage> {
        use tokio::sync::broadcast;
        use tokio_util::sync::CancellationToken;

        self.captured.lock().unwrap().push(req);
        let (tx, events) = broadcast::channel(4);
        let output = Box::pin(async move {
            let _ = tx.send(NodeEvent::LlmDone { total_tokens: 0 });
            Ok(Self::response())
        });
        Observable {
            output,
            events,
            cancel: CancellationToken::new(),
        }
    }
}

async fn reasoning_capture_executor(
    default_reasoning: &str,
) -> (
    common::ModelRegistryGuard,
    Executor,
    Arc<ReasoningCaptureProvider>,
) {
    let mut config = common::config([(
        "reasoning-test".into(),
        ModelEntry {
            model: "reasoning-test".into(),
            provider: Some("reasoning-capture".into()),
            context_budget: Some(8_192),
            reasoning: Some(default_reasoning.into()),
            reasoning_efforts: vec![ReasoningEffort::High],
            ..Default::default()
        },
    )]);
    config.providers.insert(
        "reasoning-capture".into(),
        ProviderEntry {
            kind: "openai".into(),
            reasoning_format: Some(OpenAiReasoningFormat::Official),
            ..Default::default()
        },
    );
    let registry = common::ModelRegistryGuard::acquire(config).await;
    let provider = Arc::new(ReasoningCaptureProvider::new());
    let executor = Executor::new();
    executor.providers.register(provider.clone());
    (registry, executor, provider)
}

#[tokio::test]
async fn invocation_effort_only_affects_explicit_opt_in_calls() {
    let (_registry, executor, provider) = reasoning_capture_executor("off").await;
    let file = parse_file(
        r#"
flow mixed() -> string {
    implicit = llm.call(model: "reasoning-test", prompt: "implicit")
    return llm.call(
        model: "reasoning-test",
        prompt: "explicit",
        effort: env("effort"),
    )
}
"#,
    )
    .unwrap();
    let invocation_env = InvocationEnv::single("effort", Value::Str("high".into()));

    executor
        .run_in_turn_with_env(&file, "mixed", vec![], None, None, invocation_env)
        .await
        .unwrap();

    assert_eq!(
        provider.captured(),
        vec![
            ReasoningSelection::Disabled,
            ReasoningSelection::Effort {
                effort: ReasoningEffort::High,
                execution_mode: None,
            },
        ]
    );
}

#[tokio::test]
async fn consecutive_root_invocations_do_not_reuse_effort() {
    let (_registry, executor, provider) = reasoning_capture_executor("off").await;
    let file = parse_file(
        r#"flow explicit() -> string {
    return llm.call(
        model: "reasoning-test",
        prompt: "explicit",
        effort: env("effort"),
    )
}"#,
    )
    .unwrap();

    executor
        .run_in_turn_with_env(
            &file,
            "explicit",
            vec![],
            None,
            None,
            InvocationEnv::single("effort", Value::Str("high".into())),
        )
        .await
        .unwrap();
    executor
        .run_in_turn_with_env(
            &file,
            "explicit",
            vec![],
            None,
            None,
            InvocationEnv::default(),
        )
        .await
        .unwrap();

    assert_eq!(
        provider.captured(),
        vec![
            ReasoningSelection::Effort {
                effort: ReasoningEffort::High,
                execution_mode: None,
            },
            ReasoningSelection::Disabled,
        ]
    );
}

#[tokio::test]
async fn invocation_env_is_not_rendered_as_context_or_provider_tool() {
    let (_registry, executor, provider) = reasoning_capture_executor("off").await;
    let file = parse_file(
        r#"flow explicit() -> string {
    return llm.call(
        model: "reasoning-test",
        prompt: "hello",
        context: "session",
        effort: env("effort"),
    )
}"#,
    )
    .unwrap();
    let session = Arc::new(atman_runtime::Session::open_ephemeral());
    let turn_id = TurnId::now();
    session.begin_turn(Message::user_text(turn_id.clone(), "hello"));

    executor
        .run_in_turn_with_env(
            &file,
            "explicit",
            vec![],
            Some(turn_id),
            Some(session.clone()),
            InvocationEnv::single("effort", Value::Str("high".into())),
        )
        .await
        .unwrap();

    let request = provider.requests().into_iter().next().unwrap();
    assert!(!format!("{:?}", request.messages).contains("high"));
    assert!(!request.system.as_deref().is_some_and(|system| {
        system
            .split(|character: char| !character.is_alphanumeric())
            .any(|word| word == "high")
    }));
    assert!(matches!(request.input, Value::Unit));
    assert!(request.tools.iter().all(|tool| tool.name != "env"));
    assert!(!format!("{:?}", session.messages_full()).contains("high"));
}

#[tokio::test]
async fn missing_invocation_value_evaluates_to_unit() {
    let _registry = common::ModelRegistryGuard::mock("mock").await;
    let file = parse_file(
        r#"flow read_missing() {
    return env("effort")
}"#,
    )
    .unwrap();
    let output = Executor::new()
        .run_in_turn_with_env(
            &file,
            "read_missing",
            vec![],
            None,
            None,
            InvocationEnv::default(),
        )
        .await
        .unwrap();

    assert!(matches!(output, Value::Unit));
}

#[tokio::test]
async fn missing_invocation_effort_uses_model_default() {
    let (_registry, executor, provider) = reasoning_capture_executor("high").await;
    let file = parse_file(
        r#"flow explicit() -> string {
    return llm.call(
        model: "reasoning-test",
        prompt: "explicit",
        effort: env("effort"),
    )
}"#,
    )
    .unwrap();

    executor
        .run_in_turn_with_env(
            &file,
            "explicit",
            vec![],
            None,
            None,
            InvocationEnv::default(),
        )
        .await
        .unwrap();

    assert_eq!(
        provider.captured(),
        vec![ReasoningSelection::Effort {
            effort: ReasoningEffort::High,
            execution_mode: None,
        }]
    );
}

#[tokio::test]
async fn inline_subflow_inherits_invocation_snapshot() {
    let _registry = common::ModelRegistryGuard::mock("mock").await;
    let file = parse_file(
        r#"
flow root() -> string {
    return subflow(read_effort)
}

flow read_effort() -> string {
    return env("effort")
}
"#,
    )
    .unwrap();
    let output = Executor::new()
        .run_in_turn_with_env(
            &file,
            "root",
            vec![],
            None,
            None,
            InvocationEnv::single("effort", Value::Str("high".into())),
        )
        .await
        .unwrap();

    assert!(matches!(output, Value::Str(value) if value == "high"));
}
