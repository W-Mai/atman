use atman_runtime::Session;
use atman_runtime::event::TurnId;
use atman_runtime::message::Message;
use atman_runtime::provider::ProviderRegistry;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::value::Value;
use std::sync::Arc;

fn build_long_history(session: &Session, msg_count: usize) {
    let base = "x".repeat(4000);
    for i in 0..msg_count {
        let turn = TurnId::now();
        let msg = if i % 2 == 0 {
            Message::user_text(turn, format!("{base} user {i}"))
        } else {
            Message::assistant_text(turn, format!("{base} assistant {i}"))
        };
        session.append_message(msg, None);
    }
}

#[tokio::test]
async fn resume_shows_the_compacted_view_not_the_raw_history() {
    let tmp = tempfile::tempdir().unwrap();
    let sid = {
        let session = Session::open(tmp.path()).unwrap();
        build_long_history(&session, 60);
        session.record_llm_call("mock-summary", 0, 0, 0, 0, None, None);
        let mut providers = ProviderRegistry::new();
        providers.register(Arc::new(MockProvider::new("mock-summary").with_fallback(
            Value::Str("Compacted: we did stuff, decided things, moved on.".into()),
        )));
        let before_count = session.message_count();
        atman_runtime::compaction::maybe_auto_compact(&session, "mock-summary", &providers).await;
        let after_count = session.message_count();
        assert!(
            after_count < before_count,
            "expected compaction to shrink transcript, before={before_count} after={after_count}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        session.id().to_string()
    };
    let resumed = Session::open_existing(tmp.path(), &sid).unwrap();
    let messages = resumed.messages();
    let has_summary = messages
        .iter()
        .any(|m| m.text_concat().contains("Compacted: we did stuff"));
    assert!(
        has_summary,
        "resumed transcript should include the LLM summary system message; got {} messages: {:?}",
        messages.len(),
        messages
            .iter()
            .map(|m| m.text_concat().chars().take(40).collect::<String>())
            .collect::<Vec<_>>()
    );
    assert!(
        messages.len() < 60,
        "resumed transcript should be smaller than the raw 60 messages, got {}",
        messages.len()
    );
}

#[tokio::test]
async fn legacy_compact_event_without_summary_field_replays_original_history() {
    let tmp = tempfile::tempdir().unwrap();
    let session = Session::open(tmp.path()).unwrap();
    for i in 0..5 {
        session.append_message(Message::user_text(TurnId::now(), format!("msg {i}")), None);
    }
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let events_path = session.dir().join("events.jsonl");
    let mut contents = std::fs::read_to_string(&events_path).unwrap();
    contents.push_str(
        "{\"type\":\"context_compact\",\"seq\":100,\"session_id\":\"x\",\"before_tokens\":9999,\"after_tokens\":10,\"compacted_range_start\":0,\"compacted_range_end\":3,\"ts\":\"2026-07-08T00:00:00Z\"}\n",
    );
    std::fs::write(&events_path, contents).unwrap();
    let sid = session.id().to_string();
    drop(session);
    let resumed = Session::open_existing(tmp.path(), &sid).unwrap();
    let messages = resumed.messages();
    assert_eq!(
        messages.len(),
        5,
        "legacy compact event should not drop messages"
    );
}
