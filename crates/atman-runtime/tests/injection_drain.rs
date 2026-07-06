use std::sync::Arc;
use std::sync::Mutex;

use atman_dsl::parse::parse_file;
use atman_runtime::event::{Observable, TurnId};
use atman_runtime::message::{Message, MessagePart, MessageRole};
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
                },
                stop_reason: StopReason::End,
                token_usage: TokenUsage::default(),
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
            }))
        }))
    }
}

#[tokio::test]
async fn pending_injection_appears_in_next_llm_request_messages() {
    let src = r#"flow ask() -> string {
    return llm { model: "prov", prompt: "hi" }
}
"#;
    let file = parse_file(src).unwrap();

    let session = Session::open_ephemeral();
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "start"));
    session
        .enqueue_injection("remember to check tests")
        .unwrap();

    let calls: Arc<Mutex<Vec<Vec<Message>>>> = Arc::new(Mutex::new(Vec::new()));
    let mut executor = Executor::new();
    executor.providers.register(Arc::new(RecordingProvider {
        name: "prov".into(),
        calls: calls.clone(),
        inject_before_call: None,
    }));

    executor
        .run_in_turn(&file, "ask", vec![], Some(turn_id.clone()), Some(&session))
        .await
        .unwrap();

    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let msgs = &calls[0];
    assert_eq!(msgs.len(), 2, "user prompt + injection nudge = 2 messages");
    let nudge_text = msgs[1].text_concat();
    assert!(nudge_text.contains("<user_nudge"), "got: {nudge_text}");
    assert!(
        nudge_text.contains("remember to check tests"),
        "got: {nudge_text}"
    );
}

#[tokio::test]
async fn no_pending_injection_yields_bare_user_message() {
    let src = r#"flow ask() -> string {
    return llm { model: "prov", prompt: "hi" }
}
"#;
    let file = parse_file(src).unwrap();

    let session = Session::open_ephemeral();
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "start"));

    let calls: Arc<Mutex<Vec<Vec<Message>>>> = Arc::new(Mutex::new(Vec::new()));
    let mut executor = Executor::new();
    executor.providers.register(Arc::new(RecordingProvider {
        name: "prov".into(),
        calls: calls.clone(),
        inject_before_call: None,
    }));

    executor
        .run_in_turn(&file, "ask", vec![], Some(turn_id), Some(&session))
        .await
        .unwrap();

    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].len(), 1, "no injection = only the user prompt");
}

#[tokio::test]
async fn injection_drained_once_not_reused_by_next_node() {
    let src = r#"flow chained() -> string {
    a = llm { model: "prov", prompt: "first" }
    b = llm { model: "prov", prompt: "second" }
    return b
}
"#;
    let file = parse_file(src).unwrap();

    let session = Session::open_ephemeral();
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "start"));
    session.enqueue_injection("one-shot nudge").unwrap();

    let calls: Arc<Mutex<Vec<Vec<Message>>>> = Arc::new(Mutex::new(Vec::new()));
    let mut executor = Executor::new();
    executor.providers.register(Arc::new(RecordingProvider {
        name: "prov".into(),
        calls: calls.clone(),
        inject_before_call: None,
    }));

    executor
        .run_in_turn(&file, "chained", vec![], Some(turn_id), Some(&session))
        .await
        .unwrap();

    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].len(), 2, "first call got prompt + injection");
    assert_eq!(calls[1].len(), 1, "second call got only the bare prompt");
}
