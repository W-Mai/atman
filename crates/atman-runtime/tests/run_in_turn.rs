use std::sync::Arc;

use atman_dsl::parse::parse_file;
use atman_runtime::event::TurnId;
use atman_runtime::message::{Message, MessagePart, MessageRole};
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::session::Session;
use atman_runtime::{Executor, Value};

fn user_msg(turn_id: TurnId, text: &str) -> Message {
    Message {
        role: MessageRole::User,
        parts: vec![MessagePart::Text { text: text.into() }],
        turn_id,
    }
}

#[tokio::test]
async fn run_in_turn_appends_assistant_message_to_session() {
    let src = r#"flow ask() -> string {
    return llm { model: "mock", prompt: "hi" }
}
"#;
    let file = parse_file(src).unwrap();
    let mut executor = Executor::new();
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

    assert!(matches!(&out, Value::Str(s) if s == "hello world"));

    let msgs = session.messages();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].role, MessageRole::User);
    assert_eq!(msgs[1].role, MessageRole::Assistant);
    assert_eq!(msgs[1].text_concat(), "hello world");
    assert_eq!(msgs[1].turn_id, turn_id);
}

#[tokio::test]
async fn run_without_turn_does_not_touch_session() {
    let src = r#"flow ask() -> string {
    return llm { model: "mock", prompt: "hi" }
}
"#;
    let file = parse_file(src).unwrap();
    let mut executor = Executor::new();
    executor.providers.register(Arc::new(
        MockProvider::new("mock").with_model("mock", Value::Str("no session".into())),
    ));

    let out = executor.run(&file, "ask", vec![]).await.unwrap();
    assert!(matches!(&out, Value::Str(s) if s == "no session"));
}

#[tokio::test]
async fn assistant_msg_event_carries_flow_run_id() {
    let src = r#"flow ask() -> string {
    return llm { model: "mock", prompt: "hi" }
}
"#;
    let file = parse_file(src).unwrap();
    let mut executor = Executor::new();
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
