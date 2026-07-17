use std::sync::Arc;
use std::time::Duration;

use atman_dsl::parse::parse_file;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::stream::StreamFrame;
use atman_runtime::{Executor, Session, Value, tools};

#[tokio::test]
async fn llm_chunks_flow_from_provider_to_session_stream() {
    let tmp = tempfile::tempdir().unwrap();
    let session = std::sync::Arc::new(Session::open(tmp.path()).unwrap());
    let mut rx = session.stream_subscribe();

    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    ex.providers.register(Arc::new(
        MockProvider::new("mock")
            .with_fallback(Value::Str("hi from mock".into()))
            .with_chunk_delay(Duration::from_millis(1)),
    ));

    let src = r#"flow t() -> string {
    return llm { model: "mock", prompt: "irrelevant", fallback: "ok" }
}
"#;
    let file = parse_file(src).unwrap();
    let user_msg =
        atman_runtime::message::Message::user_text(atman_runtime::event::TurnId::now(), "run");
    session.begin_turn(user_msg);
    ex.run_in_turn(&file, "t", vec![], None, Some(session.clone()))
        .await
        .expect("flow ok");
    session.end_turn();

    let mut got_chunk = false;
    let mut got_done = false;
    while let Ok(frame) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
        match frame {
            Ok(StreamFrame::LlmChunk { text, model }) => {
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
async fn tool_use_frames_wrap_dispatch() {
    let tmp = tempfile::tempdir().unwrap();
    let session = std::sync::Arc::new(Session::open(tmp.path()).unwrap());
    let mut rx = session.stream_subscribe();

    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);

    let src = r#"flow t() -> int {
    n = len([1, 2, 3])
    return n
}
"#;
    let file = parse_file(src).unwrap();
    let user_msg =
        atman_runtime::message::Message::user_text(atman_runtime::event::TurnId::now(), "run");
    session.begin_turn(user_msg);
    ex.run_in_turn(&file, "t", vec![], None, Some(session.clone()))
        .await
        .expect("flow ok");
    session.end_turn();

    let mut started = 0;
    let mut done = 0;
    while let Ok(frame) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
        match frame {
            Ok(StreamFrame::ToolUseStart { tool, .. }) if tool == "len" => started += 1,
            Ok(StreamFrame::ToolUseDone { tool, ok, .. }) if tool == "len" => {
                assert!(ok, "len should succeed");
                done += 1;
            }
            _ => {}
        }
    }
    assert_eq!(started, 1, "want one ToolUseStart for len");
    assert_eq!(done, 1, "want one ToolUseDone for len");
}

#[tokio::test]
async fn zero_subscribers_makes_stream_send_a_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let session = std::sync::Arc::new(Session::open(tmp.path()).unwrap());

    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);

    let src = r#"flow t() -> int {
    return len([1, 2, 3])
}
"#;
    let file = parse_file(src).unwrap();
    let user_msg =
        atman_runtime::message::Message::user_text(atman_runtime::event::TurnId::now(), "run");
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
