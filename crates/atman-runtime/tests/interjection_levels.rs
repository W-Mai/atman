use std::sync::{Arc, Mutex};

use atman_dsl::parse::parse_file;
use atman_runtime::event::{Observable, TurnId};
use atman_runtime::injection::InjectionLevel;
use atman_runtime::message::{Message, MessagePart, MessageRole};
use atman_runtime::provider::{
    AssistantMessage, LlmRequest, Provider, StopReason, TokenUsage, wrap_call_as_streaming,
};
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::session::Session;
use atman_runtime::tool::BoxFut;
use atman_runtime::{Executor, RuntimeError, Value};

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
}

impl Provider for RecordingProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn call<'a>(&'a self, req: LlmRequest) -> BoxFut<'a, Result<AssistantMessage, RuntimeError>> {
        let calls = self.calls.clone();
        Box::pin(async move {
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
        let msg = Message {
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Text { text: "ok".into() }],
            turn_id: req
                .messages
                .first()
                .map(|m| m.turn_id.clone())
                .unwrap_or_else(TurnId::now),
        };
        wrap_call_as_streaming(Box::pin(
            async move { Ok(AssistantMessage::text_only(msg)) },
        ))
    }
}

fn joined_messages(calls: &[Vec<Message>]) -> String {
    calls
        .iter()
        .flat_map(|c| c.iter())
        .map(|m| m.text_concat())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn l2_course_correct_renders_as_user_correction_tag() {
    let src = r#"flow ask() -> string {
    return llm { model: "rec", prompt: "hi" }
}
"#;
    let file = parse_file(src).unwrap();
    let session = Session::open_ephemeral();
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "start"));
    session
        .enqueue_injection_with_level("prefer smaller diff", InjectionLevel::L2CourseCorrect, None)
        .unwrap();

    let calls: Arc<Mutex<Vec<Vec<Message>>>> = Arc::new(Mutex::new(Vec::new()));
    let mut ex = Executor::new();
    ex.providers.register(Arc::new(RecordingProvider {
        name: "rec".into(),
        calls: calls.clone(),
    }));

    ex.run_in_turn(&file, "ask", vec![], Some(turn_id.clone()), Some(&session))
        .await
        .unwrap();

    let seen = joined_messages(&calls.lock().unwrap());
    assert!(
        seen.contains("<user_correction"),
        "L2 must render as <user_correction>, saw: {seen}"
    );
    assert!(seen.contains("prefer smaller diff"), "text missing: {seen}");
}

#[tokio::test]
async fn l3_redirect_switches_to_target_flow() {
    let src = r#"flow first() -> string {
    return llm { model: "mock", prompt: "won't reach here" }
}
flow second() -> string {
    return "redirected"
}
"#;
    let file = parse_file(src).unwrap();
    let session = Session::open_ephemeral();
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "start"));
    session
        .enqueue_injection_with_level("second", InjectionLevel::L3Redirect, Some("second".into()))
        .unwrap();

    let mut ex = Executor::new();
    ex.providers.register(Arc::new(
        MockProvider::new("mock").with_model("mock", Value::Str("nope".into())),
    ));

    let out = ex
        .run_in_turn(
            &file,
            "first",
            vec![],
            Some(turn_id.clone()),
            Some(&session),
        )
        .await
        .unwrap();
    assert!(matches!(&out, Value::Str(s) if s == "redirected"));
}

#[tokio::test]
async fn l3_redirect_chain_limit_returns_error() {
    let src = r#"flow a() -> string { return llm { model: "mock", prompt: "x" } }
flow b() -> string { return llm { model: "mock", prompt: "x" } }
flow c() -> string { return llm { model: "mock", prompt: "x" } }
flow d() -> string { return llm { model: "mock", prompt: "x" } }
flow e() -> string { return llm { model: "mock", prompt: "x" } }
flow f() -> string { return llm { model: "mock", prompt: "x" } }
flow g() -> string { return "reached" }
"#;
    let file = parse_file(src).unwrap();
    let session = Session::open_ephemeral();
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "start"));
    for target in ["b", "c", "d", "e", "f", "g"] {
        session
            .enqueue_injection_with_level(target, InjectionLevel::L3Redirect, Some(target.into()))
            .unwrap();
    }

    let mut ex = Executor::new();
    ex.providers.register(Arc::new(
        MockProvider::new("mock").with_model("mock", Value::Str("x".into())),
    ));

    let err = ex
        .run_in_turn(&file, "a", vec![], Some(turn_id), Some(&session))
        .await
        .unwrap_err();
    assert!(
        format!("{err}").contains("redirect chain exceeded"),
        "got: {err}"
    );
}

#[tokio::test]
async fn l1_and_l2_both_appear_in_next_llm_request() {
    let src = r#"flow ask() -> string {
    return llm { model: "rec", prompt: "hi" }
}
"#;
    let file = parse_file(src).unwrap();
    let session = Session::open_ephemeral();
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "start"));
    session.enqueue_injection("small hint").unwrap();
    session
        .enqueue_injection_with_level("bigger course fix", InjectionLevel::L2CourseCorrect, None)
        .unwrap();

    let calls: Arc<Mutex<Vec<Vec<Message>>>> = Arc::new(Mutex::new(Vec::new()));
    let mut ex = Executor::new();
    ex.providers.register(Arc::new(RecordingProvider {
        name: "rec".into(),
        calls: calls.clone(),
    }));

    ex.run_in_turn(&file, "ask", vec![], Some(turn_id), Some(&session))
        .await
        .unwrap();

    let seen = joined_messages(&calls.lock().unwrap());
    assert!(seen.contains("small hint"), "L1 missing: {seen}");
    assert!(seen.contains("bigger course fix"), "L2 missing: {seen}");
    assert!(
        seen.contains("<user_nudge") && seen.contains("<user_correction"),
        "both tags: {seen}"
    );
}
