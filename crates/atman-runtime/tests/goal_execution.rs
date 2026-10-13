mod common;

use std::sync::Arc;
use std::time::Duration;

use atman_runtime::memory::goal::{GoalActor, GoalPauseReason};
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::{Executor, Session, Value as AtmanValue};

#[tokio::test]
async fn user_pause_preserves_completed_reply_and_releases_context_lock() {
    let _models = common::ModelRegistryGuard::mock("mock").await;
    let session = Arc::new(Session::open_ephemeral());
    session
        .goal_control
        .set("finish the request", None)
        .unwrap();
    session.begin_turn(atman_runtime::message::Message::user_text(
        atman_rt::TurnId::now(),
        "work",
    ));
    let executor = Executor::new();
    executor.providers.register(Arc::new(
        MockProvider::new("mock")
            .with_fallback(AtmanValue::Str("complete response".into()))
            .with_chunk_delay(Duration::from_millis(20)),
    ));
    let flow = atman_rt::parse_file(
        r#"
flow main() -> string {
    contract { invocation { goal: true } }
    reply = llm.call(model: "mock", prompt: "work", context: "session")
    return text_concat(reply)
}
"#,
    )
    .unwrap();
    let mut changes = session.goal_control.subscribe();
    let mut frames = session.stream_subscribe();
    let running_session = session.clone();
    let task = tokio::spawn(async move {
        executor
            .run_in_turn(&flow, "main", vec![], None, Some(running_session))
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !matches!(
            frames.recv().await.unwrap(),
            atman_runtime::stream::StreamFrame::LlmChunk { .. }
        ) {}
        session
            .goal_control
            .pause(
                GoalActor::User,
                GoalPauseReason::UserRequested,
                "Pause".into(),
                None,
            )
            .unwrap();
        while session.goal_control.snapshot().running_operations != 0 {
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(!task.is_finished());
    assert!(session.current_turn().is_some());
    let guard = tokio::time::timeout(Duration::from_secs(1), session.acquire_compact_lock())
        .await
        .unwrap();
    drop(guard);
    let messages_before = session.messages_handle().lock().unwrap().len();
    session.goal_control.resume(GoalActor::User, None).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(result, AtmanValue::Str(text) if text == "complete response"));
    assert_eq!(
        session.messages_handle().lock().unwrap().len(),
        messages_before
    );
}
