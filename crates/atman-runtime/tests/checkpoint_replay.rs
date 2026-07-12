use atman_runtime::Session;
use atman_runtime::event::Event;
use atman_runtime::message::Message;

fn build_long_history(session: &Session, msg_count: usize) {
    let base = "x".repeat(4000);
    for i in 0..msg_count {
        let turn = atman_runtime::event::TurnId::now();
        let msg = if i % 2 == 0 {
            Message::user_text(turn, format!("{base} user {i}"))
        } else {
            Message::assistant_text(turn, format!("{base} assistant {i}"))
        };
        session.append_message(msg, None);
    }
}

#[tokio::test]
async fn checkpoint_event_written_after_compaction() {
    let tmp = tempfile::tempdir().unwrap();
    let session = Session::open(tmp.path()).unwrap();
    let sid = session.id().to_string();
    session.record_llm_call("llama-3b", 0, 0, 0, 0, None, None);
    build_long_history(&session, 20);
    session.compact_messages("test summary".into()).unwrap();
    session.shutdown().await;

    let events_path = tmp.path().join("sessions").join(&sid).join("events.jsonl");
    let text = std::fs::read_to_string(&events_path).unwrap();
    let has_checkpoint = text.contains("\"type\":\"checkpoint\"");
    assert!(
        has_checkpoint,
        "events.jsonl should contain a checkpoint event after compaction"
    );
}

#[tokio::test]
async fn checkpoint_skips_dead_history_on_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let sid;
    {
        let session = Session::open(tmp.path()).unwrap();
        sid = session.id().to_string();
        session.record_llm_call("llama-3b", 0, 0, 0, 0, None, None);
        build_long_history(&session, 20);
        session
            .compact_messages("compacted summary".into())
            .unwrap();
        session.append_message(
            Message::user_text(
                atman_runtime::event::TurnId::now(),
                "post-compact user".to_string(),
            ),
            None,
        );
        session.shutdown().await;
    }

    let session = Session::open_existing(tmp.path(), &sid).unwrap();
    let msgs = session.messages();

    assert!(!msgs.is_empty(), "reopened session should have messages");
    let has_summary = msgs
        .iter()
        .any(|m| m.text_concat().contains("compacted summary"));
    assert!(
        has_summary,
        "reopened session should contain the compaction summary"
    );
    let has_post_compact = msgs
        .iter()
        .any(|m| m.text_concat().contains("post-compact user"));
    assert!(
        has_post_compact,
        "reopened session should contain post-compact tail message"
    );
    let has_dead = msgs.iter().any(|m| m.text_concat().contains("user 0"));
    assert!(
        !has_dead,
        "dead pre-compaction message must NOT be loaded — checkpoint should skip it"
    );

    session.shutdown().await;
}

#[tokio::test]
async fn checkpoint_restores_seq_counter_on_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let sid;
    {
        let session = Session::open(tmp.path()).unwrap();
        sid = session.id().to_string();
        session.record_llm_call("llama-3b", 0, 0, 0, 0, None, None);
        build_long_history(&session, 20);
        session.compact_messages("summary".into()).unwrap();
        session.shutdown().await;
    }

    let session = Session::open_existing(tmp.path(), &sid).unwrap();
    let events = session.sink().snapshot();
    let max_existing_seq = events.iter().map(|e| e.seq()).max().unwrap_or(0);
    session.append_message(
        Message::user_text(atman_runtime::event::TurnId::now(), "new msg".to_string()),
        None,
    );
    let events_after = session.sink().snapshot();
    let new_seq = events_after
        .iter()
        .find(|e| matches!(e, Event::UserMsg { .. }))
        .map(|e| e.seq())
        .expect("new UserMsg event");
    assert!(
        new_seq > max_existing_seq,
        "new event seq {new_seq} should be > restored max {max_existing_seq}"
    );
    session.shutdown().await;
}

#[tokio::test]
async fn reopen_without_checkpoint_falls_back_to_full_replay() {
    let tmp = tempfile::tempdir().unwrap();
    let sid;
    {
        let session = Session::open(tmp.path()).unwrap();
        sid = session.id().to_string();
        session.append_message(
            Message::user_text(atman_runtime::event::TurnId::now(), "hello".to_string()),
            None,
        );
        session.append_message(
            Message::assistant_text(atman_runtime::event::TurnId::now(), "world".to_string()),
            None,
        );
        session.shutdown().await;
    }

    let session = Session::open_existing(tmp.path(), &sid).unwrap();
    let msgs = session.messages();
    assert_eq!(msgs.len(), 2, "full replay should load both messages");
    assert_eq!(msgs[0].text_concat(), "hello");
    assert_eq!(msgs[1].text_concat(), "world");
    session.shutdown().await;
}
