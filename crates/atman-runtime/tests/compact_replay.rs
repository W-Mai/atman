mod common;

use atman_runtime::Session;
use atman_runtime::event::TurnId;
use atman_runtime::message::Message;
use atman_runtime::provider::ProviderRegistry;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::value::Value;
use std::sync::Arc;

#[tokio::test]
async fn compact_commit_preserves_the_selected_window_in_live_and_restored_views() {
    use atman_runtime::compaction::{
        CompactRange, estimate_tokens_for_messages, replace_range_with_summary,
    };
    use atman_runtime::context_plan::{
        ContextRecord, ContextRecordAuthority, ContextRecordBody, ContextRecordRetention,
    };
    use atman_runtime::message::MessagePart;

    for start in [0, 2] {
        let tmp = tempfile::tempdir().unwrap();
        let session = Session::open(tmp.path()).unwrap();
        let turn = TurnId::now();
        let record = |revision, body| {
            Message::context_record(
                turn.clone(),
                ContextRecord::new(
                    "session.goal",
                    revision,
                    ContextRecordAuthority::Runtime,
                    ContextRecordRetention::Latest,
                    body,
                ),
            )
        };
        let latest = record(2, ContextRecordBody::tombstone());
        let original = vec![
            record(1, ContextRecordBody::text("superseded goal")),
            latest.clone(),
            Message::user_text(turn.clone(), "old task ".repeat(2_000)),
            Message::assistant_text(turn.clone(), "old output ".repeat(2_000)),
            Message::user_text(turn.clone(), "retained input"),
        ];
        for message in &original {
            session.append_message(message.clone(), None);
        }
        let tokens = estimate_tokens_for_messages(&original);
        let range = CompactRange {
            start,
            end: 4,
            tokens_saved_estimate: tokens,
        };
        let expected = replace_range_with_summary(&original, &range, "summary".into(), turn);
        assert_eq!(expected[1], latest);
        assert_eq!(expected[2], original[4]);

        let result = session
            .compact_messages("summary".into(), range, tokens)
            .unwrap();
        assert_eq!(session.messages().to_vec(), expected);
        assert_eq!(*session.messages_handle().lock().unwrap(), expected);
        assert_eq!(
            session.subscribe_context().borrow().window_tokens,
            result.after_tokens
        );
        let full = session.messages_full();
        assert_eq!(&full[..original.len()], original.as_slice());
        assert!(matches!(
            full.last().unwrap().parts[0],
            MessagePart::CompactSummary { .. }
        ));
        let id = session.id().to_string();
        session.shutdown().await;
        drop(session);

        let restored = Session::open_existing(tmp.path(), &id).unwrap();
        assert_eq!(restored.messages().to_vec(), expected);
        assert_eq!(*restored.messages_handle().lock().unwrap(), expected);
        assert_eq!(*restored.messages_full(), *full);
        assert_eq!(
            restored.subscribe_context().borrow().window_tokens,
            result.after_tokens
        );
        restored.shutdown().await;
    }
}

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
    let _registry = common::ModelRegistryGuard::acquire(common::config([(
        "mock-summary".into(),
        atman_runtime::model_registry::ModelEntry {
            model: "mock-summary".into(),
            context_budget: Some(40_000),
            compact_threshold_ratio: Some(0.8),
            ..Default::default()
        },
    )]))
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let sid = {
        let session = Session::open(tmp.path()).unwrap();
        build_long_history(&session, 60);
        session.record_llm_call("mock-summary", 0, 0, 0, 0, None, None);
        let providers = ProviderRegistry::new();
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
        let sid = session.id().to_string();
        session.shutdown().await;
        sid
    };
    let events_path = tmp.path().join("sessions").join(&sid).join("events.jsonl");
    let events = std::fs::read_to_string(&events_path).unwrap();
    assert!(
        events.contains("\"type\":\"checkpoint\""),
        "persistent compaction must write a checkpoint; tail: {}",
        events.lines().rev().take(5).collect::<Vec<_>>().join("\n")
    );
    let parsed = events
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    let checkpoint_index = parsed
        .iter()
        .position(|event| event["type"] == "checkpoint")
        .unwrap();
    assert_eq!(
        checkpoint_index,
        parsed.len() - 1,
        "checkpoint must be the final persisted event; trailing types: {:?}",
        parsed[checkpoint_index + 1..]
            .iter()
            .map(|event| event["type"].as_str().unwrap_or_default())
            .collect::<Vec<_>>()
    );
    assert!(
        parsed[checkpoint_index]["messages"]
            .as_array()
            .is_some_and(|messages| messages.len() < 60),
        "checkpoint must contain the compacted replacement window"
    );
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

/// Regression: after MULTIPLE runtime auto-compactions, `messages_full()`
/// must still contain pre-compact messages.
#[tokio::test]
async fn messages_full_retains_history_after_multiple_runtime_compacts() {
    let tmp = tempfile::tempdir().unwrap();
    let session = Session::open(tmp.path()).unwrap();

    let mut total_appended = 0usize;
    let mut first_msgs: Vec<String> = Vec::new();
    for phase in 0..3 {
        let base = "x".repeat(4000);
        for i in 0..40 {
            let turn = TurnId::now();
            let text = format!("{base} phase{phase} msg{i}");
            let msg = if i % 2 == 0 {
                Message::user_text(turn, text.clone())
            } else {
                Message::assistant_text(turn, text.clone())
            };
            if first_msgs.len() < 5 {
                first_msgs.push(format!("phase{phase} msg{i}"));
            }
            session.append_message(msg, None);
            total_appended += 1;
        }
        let summary = format!("Compacted summary for phase {phase}.");
        let result = session.compact_messages_auto(summary);
        assert!(
            result.is_some(),
            "phase {phase}: compaction must succeed, before={}",
            session.message_count()
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let full = session.messages_full();
    let texts: Vec<String> = full.iter().map(|m| m.text_concat()).collect();

    assert!(
        full.len() >= total_appended,
        "full must retain all {} pre-compact messages across 3 compactions, got {}. first 10 texts: {:?}",
        total_appended,
        full.len(),
        texts.iter().take(10).collect::<Vec<_>>()
    );

    assert!(
        texts.iter().any(|t| t.contains(&first_msgs[0])),
        "full must retain the first message '{}', got first 5: {:?}",
        first_msgs[0],
        texts.iter().take(5).collect::<Vec<_>>()
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
