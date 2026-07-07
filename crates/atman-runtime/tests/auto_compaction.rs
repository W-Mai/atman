use atman_runtime::Session;
use atman_runtime::event::TurnId;
use atman_runtime::message::Message;

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
async fn compact_messages_replaces_middle_span() {
    let tmp = tempfile::tempdir().unwrap();
    let session = Session::open(tmp.path()).unwrap();
    session.record_llm_call("llama-3b", 0, 0);
    build_long_history(&session, 20);
    let before_len = session.message_count();
    let result = session
        .compact_messages("test summary".into())
        .expect("expected compaction");
    assert!(result.after_tokens < result.before_tokens);
    assert!(session.message_count() < before_len);
    let msgs = session.messages();
    let has_footer = msgs
        .iter()
        .any(|m| m.text_concat().contains("[atman:compact"));
    assert!(has_footer, "compaction should leave a summary marker");
}

#[tokio::test]
async fn compact_messages_returns_none_below_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let session = Session::open(tmp.path()).unwrap();
    session.record_llm_call("claude-opus-4.7", 0, 0);
    for i in 0..4 {
        let msg = Message::user_text(TurnId::now(), format!("hi {i}"));
        session.append_message(msg, None);
    }
    assert!(session.compact_messages("noop".into()).is_none());
}

#[tokio::test]
async fn cooldown_blocks_repeat_compaction_within_window() {
    let tmp = tempfile::tempdir().unwrap();
    let session = Session::open(tmp.path()).unwrap();
    session.record_llm_call("llama-3b", 0, 0);
    build_long_history(&session, 20);
    assert!(session.approval_cooldown_ok_for_compact());
    let _ = session.compact_messages("first".into()).unwrap();
    assert!(!session.approval_cooldown_ok_for_compact());
}
