mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use atman_dsl::parse::parse_file;
use atman_runtime::error::RuntimeError;
use atman_runtime::event::{LlmCallStatus, NodeEvent, Observable, TurnId};
use atman_runtime::message::{Message, MessageOrigin, MessagePart, MessageRole, ToolCallIntent};
use atman_runtime::provider::{
    AssistantMessage, CallTiming, DEFAULT_STREAM_BUFFER, LlmRequest, Provider, StopReason,
    TokenUsage,
};
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::session::Session;
use atman_runtime::tool::BoxFut;
use atman_runtime::{Executor, Value};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

struct EmptyThenGoodProvider {
    calls: AtomicU32,
}

struct MissingIntentThenGoodProvider {
    calls: AtomicU32,
    systems: std::sync::Mutex<Vec<Option<String>>>,
    supply_on_retry: bool,
}

impl MissingIntentThenGoodProvider {
    fn response(&self, req: LlmRequest) -> AssistantMessage {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        self.systems.lock().unwrap().push(req.system);
        AssistantMessage {
            message: Message {
                role: MessageRole::Assistant,
                parts: vec![MessagePart::ToolUse {
                    id: format!("call_{call}"),
                    name: "fs.read".into(),
                    input: serde_json::json!({"path": "README.md"}),
                    intent: (call > 0 && self.supply_on_retry)
                        .then(|| ToolCallIntent::new("Read project documentation").unwrap()),
                }],
                turn_id: TurnId::now(),
                origin: MessageOrigin::User,
            },
            stop_reason: StopReason::End,
            token_usage: TokenUsage::default(),
            timing: CallTiming::default(),
            model: "intent-model".into(),
            response_id: None,
        }
    }
}

impl Provider for MissingIntentThenGoodProvider {
    fn name(&self) -> &str {
        "intent-model"
    }

    fn call<'a>(&'a self, req: LlmRequest) -> BoxFut<'a, Result<AssistantMessage, RuntimeError>> {
        Box::pin(async move { Ok(self.response(req)) })
    }

    fn call_streaming(&self, req: LlmRequest) -> Observable<AssistantMessage> {
        let response = self.response(req);
        let (tx, events) = broadcast::channel(DEFAULT_STREAM_BUFFER);
        let cancel = CancellationToken::new();
        let output = Box::pin(async move {
            let _ = tx.send(NodeEvent::LlmDone { total_tokens: 0 });
            Ok(response)
        });
        Observable {
            output,
            events,
            cancel,
        }
    }
}

impl EmptyThenGoodProvider {
    fn response(&self) -> AssistantMessage {
        let parts = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Vec::new()
        } else {
            vec![MessagePart::Text { text: "ok".into() }]
        };
        AssistantMessage {
            message: Message {
                role: MessageRole::Assistant,
                parts,
                turn_id: TurnId::now(),
                origin: MessageOrigin::User,
            },
            stop_reason: StopReason::End,
            token_usage: TokenUsage::default(),
            timing: CallTiming::default(),
            model: "empty-then-good".into(),
            response_id: None,
        }
    }
}

impl Provider for EmptyThenGoodProvider {
    fn name(&self) -> &str {
        "empty-then-good"
    }

    fn call<'a>(&'a self, _req: LlmRequest) -> BoxFut<'a, Result<AssistantMessage, RuntimeError>> {
        Box::pin(async move { Ok(self.response()) })
    }

    fn call_streaming(&self, _req: LlmRequest) -> Observable<AssistantMessage> {
        let response = self.response();
        let (tx, events) = broadcast::channel(DEFAULT_STREAM_BUFFER);
        let cancel = CancellationToken::new();
        let output = Box::pin(async move {
            let _ = tx.send(NodeEvent::LlmDone { total_tokens: 0 });
            Ok(response)
        });
        Observable {
            output,
            events,
            cancel,
        }
    }
}

fn user_msg(turn_id: TurnId, text: &str) -> Message {
    Message {
        role: MessageRole::User,
        parts: vec![MessagePart::Text { text: text.into() }],
        turn_id,
        origin: MessageOrigin::User,
    }
}

#[tokio::test]
async fn run_in_turn_appends_assistant_message_to_session() {
    let _registry = common::ModelRegistryGuard::mock("mock").await;
    let src = r#"flow ask() -> string {
    return llm.call(model: "mock", prompt: "hi", context: "session")
}
"#;
    let file = parse_file(src).unwrap();
    let executor = Executor::new();
    executor.providers.register(Arc::new(
        MockProvider::new("mock").with_model("mock", Value::Str("hello world".into())),
    ));

    let session = std::sync::Arc::new(Session::open_ephemeral());
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "please respond"));

    let out = executor
        .run_in_turn(
            &file,
            "ask",
            vec![],
            Some(turn_id.clone()),
            Some(session.clone()),
        )
        .await
        .unwrap();
    session.end_turn();

    assert!(matches!(&out, Value::Message(message) if message.text_concat() == "hello world"));

    let msgs = session.messages();
    let ordinary: Vec<_> = msgs
        .iter()
        .filter(|message| {
            !message
                .parts
                .iter()
                .any(|part| matches!(part, MessagePart::ContextRecord(_)))
        })
        .collect();
    assert_eq!(
        ordinary.len(),
        2,
        "root assistant must stay in session context"
    );
    assert_eq!(ordinary[0].role, MessageRole::User);
    assert_eq!(ordinary[1].role, MessageRole::Assistant);
    assert_eq!(ordinary[1].text_concat(), "hello world");

    let has_correlated_assistant = session.sink().snapshot().iter().any(|event| {
        matches!(
            event,
            atman_runtime::Event::AssistantMsg {
                turn_id: t,
                flow_run_id: Some(_),
                message,
            } if *t == turn_id && message.text_concat() == "hello world"
        )
    });
    assert!(has_correlated_assistant);
}

#[tokio::test]
async fn run_without_turn_does_not_touch_session() {
    let _registry = common::ModelRegistryGuard::mock("mock").await;
    let src = r#"flow ask() -> string {
    return llm.call(model: "mock", prompt: "hi")
}
"#;
    let file = parse_file(src).unwrap();
    let executor = Executor::new();
    executor.providers.register(Arc::new(
        MockProvider::new("mock").with_model("mock", Value::Str("no session".into())),
    ));

    let out = executor.run(&file, "ask", vec![]).await.unwrap();
    assert!(matches!(&out, Value::Str(s) if s == "no session"));
}

#[tokio::test]
async fn assistant_msg_event_carries_flow_run_id() {
    let _registry = common::ModelRegistryGuard::mock("mock").await;
    let src = r#"flow ask() -> string {
    return llm.call(model: "mock", prompt: "hi", context: "session")
}
"#;
    let file = parse_file(src).unwrap();
    let executor = Executor::new();
    executor.providers.register(Arc::new(
        MockProvider::new("mock").with_model("mock", Value::Str("ok".into())),
    ));

    let session = std::sync::Arc::new(Session::open_ephemeral());
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "start"));

    executor
        .run_in_turn(
            &file,
            "ask",
            vec![],
            Some(turn_id.clone()),
            Some(session.clone()),
        )
        .await
        .unwrap();
    session.end_turn();

    let events = session.sink().snapshot();
    let has_correlated_assistant = events.iter().any(|e| {
        matches!(
            e,
            atman_runtime::Event::AssistantMsg {
                turn_id: t,
                flow_run_id: Some(_),
                ..
            } if *t == turn_id
        )
    });
    assert!(
        has_correlated_assistant,
        "assistant_msg must carry flow_run_id when run_in_turn"
    );
}

#[tokio::test]
async fn empty_assistant_message_retries_without_entering_session_history() {
    let _registry =
        common::ModelRegistryGuard::acquire(common::config([common::model_for_provider(
            "empty-model",
            "empty-then-good",
            8_192,
            None,
        )]))
        .await;
    let file = parse_file(
        r#"flow ask() -> string {
    return llm.call(model: "empty-model", prompt: "hi", context: "session", retry: 1)
}
"#,
    )
    .unwrap();
    let executor = Executor::new();
    let provider = Arc::new(EmptyThenGoodProvider {
        calls: AtomicU32::new(0),
    });
    executor.providers.register(provider.clone());
    let session = Arc::new(Session::open_ephemeral());
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "start"));

    let output = executor
        .run_in_turn(&file, "ask", vec![], Some(turn_id), Some(session.clone()))
        .await
        .unwrap();
    session.end_turn();

    assert!(matches!(&output, Value::Message(message) if message.text_concat() == "ok"));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    let messages = session.messages();
    let assistant_messages = messages
        .iter()
        .filter(|message| message.role == MessageRole::Assistant)
        .collect::<Vec<_>>();
    assert_eq!(assistant_messages.len(), 1);
    assert_eq!(assistant_messages[0].text_concat(), "ok");
    let statuses = executor
        .events
        .snapshot()
        .into_iter()
        .filter_map(|event| match event {
            atman_runtime::Event::LlmCall { status, .. } => Some(status),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(matches!(
        statuses.as_slice(),
        [LlmCallStatus::Errored { message }, LlmCallStatus::Ok]
            if message.contains("empty assistant message")
    ));
}

#[tokio::test]
async fn missing_tool_intent_retries_with_schema_correction() {
    let _registry = common::ModelRegistryGuard::mock("intent-model").await;
    let file = parse_file(
        r#"flow ask() -> any {
    return llm.call(
        model: "intent-model",
        prompt: "read the project documentation",
        context: "session",
        tools: ["fs.read"],
    )
}
"#,
    )
    .unwrap();
    let executor = Executor::new();
    atman_runtime::tools::register_tier_zero(&executor.tools);
    let provider = Arc::new(MissingIntentThenGoodProvider {
        calls: AtomicU32::new(0),
        systems: std::sync::Mutex::new(Vec::new()),
        supply_on_retry: true,
    });
    executor.providers.register(provider.clone());
    let session = Arc::new(Session::open_ephemeral());
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "start"));

    let output = executor
        .run_in_turn(&file, "ask", vec![], Some(turn_id), Some(session.clone()))
        .await
        .unwrap();
    session.end_turn();

    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert!(
        provider.systems.lock().unwrap()[1]
            .as_deref()
            .is_some_and(|system| system.contains("previous response omitted"))
    );
    assert!(matches!(
        output,
        Value::Message(Message { parts, .. })
            if parts.iter().any(|part| matches!(
                part,
                MessagePart::ToolUse { intent: Some(intent), .. }
                    if intent.as_str() == "Read project documentation"
            ))
    ));
    assert_eq!(
        session
            .messages()
            .iter()
            .filter(|message| message.role == MessageRole::Assistant)
            .count(),
        1
    );
    let statuses = executor
        .events
        .snapshot()
        .into_iter()
        .filter_map(|event| match event {
            atman_runtime::Event::LlmCall { status, .. } => Some(status),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(matches!(
        statuses.as_slice(),
        [LlmCallStatus::Errored { message }, LlmCallStatus::Ok]
            if message.contains("omitted required tool call intent")
    ));
}

#[tokio::test]
async fn repeated_missing_tool_intent_keeps_the_flow_usable() {
    let _registry = common::ModelRegistryGuard::mock("intent-model").await;
    let file = parse_file(
        r#"flow ask() -> any {
    return llm.call(
        model: "intent-model",
        prompt: "read the project documentation",
        context: "session",
        tools: ["fs.read"],
    )
}
"#,
    )
    .unwrap();
    let executor = Executor::new();
    atman_runtime::tools::register_tier_zero(&executor.tools);
    let provider = Arc::new(MissingIntentThenGoodProvider {
        calls: AtomicU32::new(0),
        systems: std::sync::Mutex::new(Vec::new()),
        supply_on_retry: false,
    });
    executor.providers.register(provider.clone());
    let session = Arc::new(Session::open_ephemeral());
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "start"));

    let output = executor
        .run_in_turn(&file, "ask", vec![], Some(turn_id), Some(session.clone()))
        .await
        .unwrap();
    session.end_turn();

    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert!(matches!(
        output,
        Value::Message(Message { parts, .. })
            if parts.iter().any(|part| matches!(
                part,
                MessagePart::ToolUse { intent: None, .. }
            ))
    ));
    assert_eq!(
        session
            .messages()
            .iter()
            .filter(|message| message.role == MessageRole::Assistant)
            .count(),
        1
    );
}
