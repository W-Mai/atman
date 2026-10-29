mod common;

use std::sync::Arc;
use std::time::Duration;

use atman_rt::parse_file;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::stream::StreamFrame;
use atman_runtime::{Executor, Session};
type Value = atman_rt::Value<atman_runtime::AtmanPayload, atman_runtime::RuntimeError>;
type TurnId = atman_rt::TurnId<atman_runtime::event::AtmanUuid>;

#[tokio::test]
async fn llm_chunks_flow_from_provider_to_session_stream() {
    let _registry = common::ModelRegistryGuard::mock("mock").await;
    let tmp = tempfile::tempdir().unwrap();
    let session = std::sync::Arc::new(Session::open(tmp.path()).unwrap());
    let mut rx = session.stream_subscribe();

    let ex = Executor::new();
    ex.providers.register(Arc::new(
        MockProvider::new("mock")
            .with_fallback(Value::Str("hi from mock".into()))
            .with_chunk_delay(Duration::from_millis(1)),
    ));

    let src = r#"flow t() -> message {
    return llm.call(model: "mock", prompt: "irrelevant", context: "session")
}
"#;
    let file = parse_file(src).unwrap();
    let user_msg = atman_runtime::message::Message::user_text(TurnId::now(), "run");
    session.begin_turn(user_msg);
    ex.run_in_turn(&file, "t", vec![], None, Some(session.clone()))
        .await
        .expect("flow ok");
    session.end_turn();

    let mut got_chunk = false;
    let mut got_done = false;
    while let Ok(frame) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
        match frame {
            Ok(StreamFrame::LlmChunk { text, model, .. }) => {
                assert!(!text.is_empty(), "chunk text should not be empty");
                assert_eq!(model, "mock");
                got_chunk = true;
            }
            Ok(StreamFrame::LlmDone { .. }) => {
                got_done = true;
                break;
            }
            _ => {}
        }
    }
    assert!(got_chunk, "want at least one LlmChunk frame");
    assert!(got_done, "want LlmDone frame after stream ends");
}

#[tokio::test]
async fn internal_llm_extract_does_not_publish_model_tokens_to_session_stream() {
    let _registry = common::ModelRegistryGuard::mock("mock").await;
    let tmp = tempfile::tempdir().unwrap();
    let session = std::sync::Arc::new(Session::open(tmp.path()).unwrap());
    let mut rx = session.stream_subscribe();

    let ex = Executor::new();
    let provider =
        MockProvider::new("mock").with_model("mock", Value::Str(r#"{"value":"selected"}"#.into()));
    ex.providers.register(Arc::new(provider));

    let src = r#"flow t() -> string {
    result = llm.extract(
        model: "mock",
        prompt: "Select a value",
        fields: { value: string -- "value" },
    )
    return result.value
}
"#;
    let file = parse_file(src).unwrap();
    let user_msg = atman_runtime::message::Message::user_text(TurnId::now(), "run");
    session.begin_turn(user_msg);
    ex.run_in_turn(&file, "t", vec![], None, Some(session.clone()))
        .await
        .expect("flow ok");
    session.end_turn();

    let mut assistant_messages = Vec::new();
    while let Ok(frame) = rx.try_recv() {
        match frame {
            StreamFrame::LlmChunk { .. } | StreamFrame::ThinkingChunk { .. } => {
                panic!("internal extraction must not enter the conversation stream")
            }
            StreamFrame::AssistantMsg { message, .. } => assistant_messages.push(message),
            _ => {}
        }
    }
    assert!(
        assistant_messages.is_empty(),
        "internal extraction must not publish its JSON response as an assistant message"
    );
    assert!(
        session
            .messages()
            .iter()
            .all(|message| message.role != atman_runtime::message::MessageRole::Assistant),
        "internal extraction must not record an assistant response in session history"
    );
}

#[tokio::test]
async fn tool_use_frames_wrap_dispatch() {
    let tmp = tempfile::tempdir().unwrap();
    let session = std::sync::Arc::new(Session::open(tmp.path()).unwrap());
    let mut rx = session.stream_subscribe();

    let ex = Executor::new();

    let src = r#"flow t() -> string {
    quoted = shell_quote("hello world")
    return quoted
}
"#;
    let file = parse_file(src).unwrap();
    let user_msg = atman_runtime::message::Message::user_text(TurnId::now(), "run");
    session.begin_turn(user_msg);
    ex.run_in_turn(&file, "t", vec![], None, Some(session.clone()))
        .await
        .expect("flow ok");
    session.end_turn();

    let mut started = 0;
    let mut done = 0;
    while let Ok(frame) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
        match frame {
            Ok(StreamFrame::ToolUseStart { tool, .. }) if tool == "shell_quote" => started += 1,
            Ok(StreamFrame::ToolUseDone { tool, ok, .. }) if tool == "shell_quote" => {
                assert!(ok, "shell_quote should succeed");
                done += 1;
            }
            _ => {}
        }
    }
    assert_eq!(started, 1, "want one ToolUseStart for shell_quote");
    assert_eq!(done, 1, "want one ToolUseDone for shell_quote");
}

#[tokio::test]
async fn zero_subscribers_makes_stream_send_a_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let session = std::sync::Arc::new(Session::open(tmp.path()).unwrap());

    let ex = Executor::new();

    let src = r#"flow t() -> int {
    return len([1, 2, 3])
}
"#;
    let file = parse_file(src).unwrap();
    let user_msg = atman_runtime::message::Message::user_text(TurnId::now(), "run");
    session.begin_turn(user_msg);
    let out = ex
        .run_in_turn(&file, "t", vec![], None, Some(session.clone()))
        .await
        .expect("flow ok");
    session.end_turn();

    match out {
        Value::Int(n) => assert_eq!(n, 3),
        other => panic!("want int, got {other:?}"),
    }
}
