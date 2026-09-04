mod common;

use std::sync::Arc;
use std::sync::Mutex;

use atman_dsl::parse::parse_file;
use atman_runtime::event::{Observable, TurnId};
use atman_runtime::message::{Message, MessageOrigin, MessagePart, MessageRole};
use atman_runtime::provider::{
    AssistantMessage, LlmRequest, Provider, StopReason, TokenUsage, wrap_call_as_streaming,
};
use atman_runtime::session::Session;
use atman_runtime::tool::BoxFut;
use atman_runtime::{Executor, RuntimeError};

fn user_msg(turn_id: TurnId, text: &str) -> Message {
    Message {
        role: MessageRole::User,
        parts: vec![MessagePart::Text { text: text.into() }],
        turn_id,
        origin: MessageOrigin::User,
    }
}

struct RecordingProvider {
    name: String,
    calls: Arc<Mutex<Vec<Vec<Message>>>>,
    inject_before_call: Option<Arc<(Session, String)>>,
}

impl Provider for RecordingProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn call<'a>(&'a self, req: LlmRequest) -> BoxFut<'a, Result<AssistantMessage, RuntimeError>> {
        let calls = self.calls.clone();
        let inject = self.inject_before_call.clone();
        Box::pin(async move {
            if let Some(ctx) = inject {
                ctx.0.enqueue_injection(ctx.1.clone()).unwrap();
            }
            calls.lock().unwrap().push(req.messages.clone());
            let turn_id = req
                .messages
                .first()
                .map(|m| m.turn_id.clone())
                .unwrap_or_else(TurnId::now);
            Ok(AssistantMessage {
                message: Message {
                    role: MessageRole::Assistant,
                    parts: vec![MessagePart::Text { text: "ok".into() }],
                    turn_id,
                    origin: MessageOrigin::User,
                },
                stop_reason: StopReason::End,
                token_usage: TokenUsage::default(),
                timing: atman_runtime::provider::CallTiming::default(),
                model: String::new(),
                response_id: None,
            })
        })
    }

    fn call_streaming(&self, req: LlmRequest) -> Observable<AssistantMessage> {
        let calls = self.calls.clone();
        let inject = self.inject_before_call.clone();
        let turn_id = req
            .messages
            .first()
            .map(|m| m.turn_id.clone())
            .unwrap_or_else(TurnId::now);
        let messages = req.messages.clone();
        wrap_call_as_streaming(Box::pin(async move {
            if let Some(ctx) = inject {
                ctx.0.enqueue_injection(ctx.1.clone()).unwrap();
            }
            calls.lock().unwrap().push(messages);
            Ok(AssistantMessage::text_only(Message {
                role: MessageRole::Assistant,
                parts: vec![MessagePart::Text { text: "ok".into() }],
                turn_id,
                origin: MessageOrigin::User,
            }))
        }))
    }
}

#[tokio::test]
async fn consumed_steering_is_persistent_and_respects_request_context_selection() {
    let _registry =
        common::ModelRegistryGuard::acquire(common::config([common::model_for_provider(
            "prov", "prov", 8_192, None,
        )]))
        .await;
    for selection in [
        "context: \"session\"",
        "context: \"session_recent(1)\"",
        "prompt: \"explicit\"",
        "messages: [user_msg(\"explicit\")]",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let session = Arc::new(Session::open(tmp.path()).unwrap());
        let sid = session.id().to_string();
        let turn_id = session.begin_turn(Message::user_text(TurnId::now(), "task"));
        let ids = [
            session.enqueue_injection("same steering").unwrap(),
            session.enqueue_injection("same steering").unwrap(),
        ];
        let calls = Arc::new(Mutex::new(Vec::new()));
        let executor = Executor::with_events(session.sink().clone());
        executor.providers.register(Arc::new(RecordingProvider {
            name: "prov".into(),
            calls: calls.clone(),
            inject_before_call: None,
        }));
        let invalid =
            parse_file(r#"flow invalid() -> string { return llm.call(model: "prov") }"#).unwrap();
        let result = executor
            .run_in_turn(
                &invalid,
                "invalid",
                vec![],
                Some(turn_id.clone()),
                Some(session.clone()),
            )
            .await;
        assert!(matches!(
            result,
            Err(RuntimeError::MissingArg(_))
                | Ok(atman_runtime::Value::Err(RuntimeError::MissingArg(_)))
        ));
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(session.list_pending_injections().len(), ids.len());
        let file = parse_file(&format!(
            r#"flow ask() -> string {{
    llm.call(model: "prov", {selection})
    llm.call(model: "prov", {selection})
    return "done"
}}
"#
        ))
        .unwrap();
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
        session.end_turn(&turn_id);
        let calls = calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2);
        for id in &ids {
            assert_eq!(
                calls[0]
                    .iter()
                    .filter(|message| message.text_concat().contains(&id.to_string()))
                    .count(),
                1,
                "{selection}"
            );
        }
        if selection == "context: \"session\"" {
            assert_eq!(calls[0], calls[1][..calls[0].len()]);
        } else if selection.starts_with("prompt:") || selection.starts_with("messages:") {
            assert!(
                !calls[1]
                    .iter()
                    .any(|message| message.text_concat().contains("same steering"))
            );
        }
        let expected = session.messages().to_vec();
        for id in &ids {
            assert_eq!(
                expected
                    .iter()
                    .filter(|message| message.text_concat().contains(&id.to_string()))
                    .count(),
                1
            );
        }
        session.shutdown().await;
        let restored = Session::open_existing(tmp.path(), &sid).unwrap();
        assert_eq!(restored.messages().to_vec(), expected);
        assert!(restored.list_pending_injections().is_empty());
        restored.shutdown().await;
    }
}

#[tokio::test]
async fn pending_injection_appears_in_next_llm_request_messages() {
    let _registry =
        common::ModelRegistryGuard::acquire(common::config([common::model_for_provider(
            "prov", "prov", 8_192, None,
        )]))
        .await;
    let src = r#"flow ask() -> string {
    return llm.call(model: "prov", prompt: "hi")
}
"#;
    let file = parse_file(src).unwrap();

    let session = std::sync::Arc::new(Session::open_ephemeral());
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "start"));
    session
        .enqueue_injection("remember to check tests")
        .unwrap();

    let calls: Arc<Mutex<Vec<Vec<Message>>>> = Arc::new(Mutex::new(Vec::new()));
    let executor = Executor::new();
    executor.providers.register(Arc::new(RecordingProvider {
        name: "prov".into(),
        calls: calls.clone(),
        inject_before_call: None,
    }));

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

    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let msgs = &calls[0];
    assert_eq!(
        msgs.iter()
            .filter(|message| message.role == MessageRole::User)
            .count(),
        2,
        "user prompt + injection nudge = 2 user messages"
    );
    let nudge_text = msgs
        .iter()
        .map(Message::text_concat)
        .find(|text| text.contains("<user_nudge"))
        .expect("injection message");
    assert!(nudge_text.contains("<user_nudge"), "got: {nudge_text}");
    assert!(
        nudge_text.contains("remember to check tests"),
        "got: {nudge_text}"
    );
}

#[tokio::test]
async fn no_pending_injection_yields_bare_user_message() {
    let _registry =
        common::ModelRegistryGuard::acquire(common::config([common::model_for_provider(
            "prov", "prov", 8_192, None,
        )]))
        .await;
    let src = r#"flow ask() -> string {
    return llm.call(model: "prov", prompt: "hi")
}
"#;
    let file = parse_file(src).unwrap();

    let session = std::sync::Arc::new(Session::open_ephemeral());
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "start"));

    let calls: Arc<Mutex<Vec<Vec<Message>>>> = Arc::new(Mutex::new(Vec::new()));
    let executor = Executor::new();
    executor.providers.register(Arc::new(RecordingProvider {
        name: "prov".into(),
        calls: calls.clone(),
        inject_before_call: None,
    }));

    executor
        .run_in_turn(&file, "ask", vec![], Some(turn_id), Some(session.clone()))
        .await
        .unwrap();

    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0]
            .iter()
            .filter(|message| message.role == MessageRole::User)
            .count(),
        1,
        "no injection = only one user prompt"
    );
}

#[tokio::test]
async fn injection_drained_once_not_reused_by_next_node() {
    let _registry =
        common::ModelRegistryGuard::acquire(common::config([common::model_for_provider(
            "prov", "prov", 8_192, None,
        )]))
        .await;
    let src = r#"flow chained() -> string {
    a = llm.call(model: "prov", prompt: "first")
    b = llm.call(model: "prov", prompt: "second")
    return b
}
"#;
    let file = parse_file(src).unwrap();

    let session = std::sync::Arc::new(Session::open_ephemeral());
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "start"));
    session.enqueue_injection("one-shot nudge").unwrap();

    let calls: Arc<Mutex<Vec<Vec<Message>>>> = Arc::new(Mutex::new(Vec::new()));
    let executor = Executor::new();
    executor.providers.register(Arc::new(RecordingProvider {
        name: "prov".into(),
        calls: calls.clone(),
        inject_before_call: None,
    }));

    executor
        .run_in_turn(
            &file,
            "chained",
            vec![],
            Some(turn_id),
            Some(session.clone()),
        )
        .await
        .unwrap();

    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0]
            .iter()
            .filter(|message| message.role == MessageRole::User)
            .count(),
        2,
        "first call got prompt + injection"
    );
    assert_eq!(
        calls[1]
            .iter()
            .filter(|message| message.role == MessageRole::User)
            .count(),
        1,
        "second call got only the bare prompt"
    );
}
