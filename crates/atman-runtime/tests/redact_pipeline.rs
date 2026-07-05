use std::sync::Arc;

use atman_runtime::event::{Event, EventSink, TurnId};
use atman_runtime::event_writer::EventWriter;
use atman_runtime::message::Message;
use atman_runtime::redact::{RedactMode, Redactor};

#[tokio::test]
async fn events_jsonl_contains_redacted_marker_for_openai_key_in_user_msg() {
    let dir = tempfile::tempdir().unwrap();
    let redactor = Arc::new(Redactor::builtin());
    let writer = EventWriter::spawn_with(dir.path(), Some(redactor.clone())).unwrap();
    let sink = EventSink::new()
        .with_forwarder(writer.sender())
        .with_redactor(redactor);

    let turn_id = TurnId::now();
    sink.emit(Event::UserMsg {
        seq: 0,
        turn_id: turn_id.clone(),
        message: Message::user_text(
            turn_id.clone(),
            "please try token sk-abcdefghijklmnop1234567890 now",
        ),
        ts: chrono::Utc::now(),
    });
    writer.shutdown().await;

    let jsonl = tokio::fs::read_to_string(dir.path().join("events.jsonl"))
        .await
        .unwrap();
    assert!(
        jsonl.contains("<REDACTED:openai_api_key>"),
        "expected redacted marker on disk: {jsonl}"
    );
    assert!(
        !jsonl.contains("sk-abcdefghijklmnop1234567890"),
        "raw key must not reach disk: {jsonl}"
    );
}

#[tokio::test]
async fn events_jsonl_partial_mode_keeps_prefix_and_suffix() {
    let dir = tempfile::tempdir().unwrap();
    let redactor = Arc::new(Redactor::builtin().with_mode(RedactMode::Partial));
    let writer = EventWriter::spawn_with(dir.path(), Some(redactor.clone())).unwrap();
    let sink = EventSink::new()
        .with_forwarder(writer.sender())
        .with_redactor(redactor);

    let turn_id = TurnId::now();
    sink.emit(Event::UserMsg {
        seq: 0,
        turn_id: turn_id.clone(),
        message: Message::user_text(turn_id, "call github with ghp_abcdefghij1234567890xyzXYZ11"),
        ts: chrono::Utc::now(),
    });
    writer.shutdown().await;

    let jsonl = tokio::fs::read_to_string(dir.path().join("events.jsonl"))
        .await
        .unwrap();
    assert!(jsonl.contains("ghp"), "partial keeps prefix: {jsonl}");
    assert!(
        !jsonl.contains("ghp_abcdefghij1234567890xyzXYZ11"),
        "full key must not appear: {jsonl}"
    );
    assert!(jsonl.contains("<REDACTED:github_token>"));
}

#[tokio::test]
async fn events_jsonl_without_redactor_keeps_secret_verbatim() {
    let dir = tempfile::tempdir().unwrap();
    let writer = EventWriter::spawn_with(dir.path(), None).unwrap();
    let sink = EventSink::new().with_forwarder(writer.sender());

    let turn_id = TurnId::now();
    sink.emit(Event::UserMsg {
        seq: 0,
        turn_id: turn_id.clone(),
        message: Message::user_text(turn_id, "leak sk-abcdefghijklmnop1234567890 baseline"),
        ts: chrono::Utc::now(),
    });
    writer.shutdown().await;

    let jsonl = tokio::fs::read_to_string(dir.path().join("events.jsonl"))
        .await
        .unwrap();
    assert!(
        jsonl.contains("sk-abcdefghijklmnop1234567890"),
        "no-redactor baseline should keep raw: {jsonl}"
    );
}
