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
    session.record_llm_call("llama-3b", 0, 0, 0, 0, None, None);
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
    session.record_llm_call("claude-opus-4.7", 0, 0, 0, 0, None, None);
    for i in 0..4 {
        let msg = Message::user_text(TurnId::now(), format!("hi {i}"));
        session.append_message(msg, None);
    }
    assert!(session.compact_messages("noop".into()).is_none());
}

#[tokio::test]
async fn maybe_auto_compact_emits_warning_when_no_range_found() {
    use atman_runtime::compaction::maybe_auto_compact;
    let tmp = tempfile::tempdir().unwrap();
    let session = Session::open(tmp.path()).unwrap();
    let big = "y".repeat(50_000);
    session.append_message(Message::user_text(TurnId::now(), big.clone()), None);
    session.append_message(Message::assistant_text(TurnId::now(), big), None);
    session.record_llm_call("llama-3b", 0, 0, 0, 0, None, None);
    let providers = atman_runtime::provider::ProviderRegistry::new();
    maybe_auto_compact(&session, "llama-3b", &providers).await;
    let warned = session
        .sink()
        .snapshot()
        .iter()
        .any(|e| matches!(e, atman_runtime::event::Event::WatchWarn { target, .. } if target == "context.compaction"));
    assert!(warned, "expected a WatchWarn for skipped compaction");
}

#[tokio::test]
async fn maybe_auto_compact_calls_llm_and_writes_summary_event() {
    use atman_runtime::compaction::maybe_auto_compact;
    use atman_runtime::event::Event;
    use atman_runtime::provider::ProviderRegistry;
    use atman_runtime::providers::mock::MockProvider;
    use atman_runtime::value::Value;
    use std::sync::Arc;
    let tmp = tempfile::tempdir().unwrap();
    let session = Session::open(tmp.path()).unwrap();
    build_long_history(&session, 60);
    session.record_llm_call("mock-summary", 0, 0, 0, 0, None, None);
    let mut providers = ProviderRegistry::new();
    providers.register(Arc::new(MockProvider::new("mock-summary").with_fallback(
        Value::Str("We investigated compaction and shipped the anchor-based fs.read tool.".into()),
    )));
    maybe_auto_compact(&session, "mock-summary", &providers).await;
    let events = session.sink().snapshot();
    let compact_event = events
        .iter()
        .find_map(|e| match e {
            Event::ContextCompact {
                summary_text,
                replacement_msg_seq,
                ..
            } => Some((summary_text.clone(), *replacement_msg_seq)),
            _ => None,
        })
        .expect("expected ContextCompact event");
    let (summary_text, replacement_seq) = compact_event;
    assert!(
        summary_text
            .as_deref()
            .unwrap_or_default()
            .contains("compaction"),
        "expected LLM summary text, got {summary_text:?}"
    );
    assert!(replacement_seq.is_some());
    let has_system_msg = events.iter().any(|e| matches!(e, Event::SystemMsg { .. }));
    assert!(has_system_msg, "expected a paired SystemMsg event");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::CompactionSummary { .. })),
        "expected a durable CompactionSummary event"
    );
}

async fn setup_review_env() -> (
    tempfile::TempDir,
    std::sync::Arc<Session>,
    atman_runtime::provider::ProviderRegistry,
) {
    use atman_runtime::provider::ProviderRegistry;
    use atman_runtime::providers::mock::MockProvider;
    use atman_runtime::value::Value;
    use std::sync::Arc;
    let tmp = tempfile::tempdir().unwrap();
    let session = Arc::new(Session::open(tmp.path()).unwrap());
    build_long_history(&session, 60);
    session.record_llm_call("mock-summary", 0, 0, 0, 0, None, None);
    let mut providers = ProviderRegistry::new();
    providers
        .register(Arc::new(MockProvider::new("mock-summary").with_fallback(
            Value::Str("original LLM summary about compaction".into()),
        )));
    (tmp, session, providers)
}

fn wait_for_pending_and_decide(
    session: std::sync::Arc<Session>,
    decision: atman_runtime::CompactReviewDecision,
) -> tokio::sync::watch::Receiver<Option<atman_runtime::PendingCompactReview>> {
    let sub = session.compact_reviews().subscribe();
    tokio::spawn(async move {
        let reviews = session.compact_reviews();
        for _ in 0..500 {
            if let Some(pending) = reviews.list_pending() {
                reviews.decide(&pending.review_id, decision.clone());
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        eprintln!("[test] wait_for_pending_and_decide timed out");
    });
    sub
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_accept_as_is_commits_llm_summary() {
    use atman_runtime::CompactReviewDecision;
    use atman_runtime::compaction::maybe_auto_compact;
    use atman_runtime::event::Event;
    let (_tmp, session, providers) = setup_review_env().await;
    session.set_compact_review_mode(atman_runtime::CompactReviewMode::Always);
    let _sub = wait_for_pending_and_decide(session.clone(), CompactReviewDecision::AcceptAsIs);
    maybe_auto_compact(&session, "mock-summary", &providers).await;
    let events = session.sink().snapshot();
    let (summary_text, _) = events
        .iter()
        .find_map(|e| match e {
            Event::ContextCompact {
                summary_text,
                replacement_msg_seq,
                ..
            } => Some((summary_text.clone(), *replacement_msg_seq)),
            _ => None,
        })
        .expect("expected ContextCompact event");
    assert!(
        summary_text
            .as_deref()
            .unwrap_or_default()
            .contains("original LLM summary")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_accept_edited_commits_user_summary() {
    use atman_runtime::CompactReviewDecision;
    use atman_runtime::compaction::maybe_auto_compact;
    use atman_runtime::event::Event;
    let (_tmp, session, providers) = setup_review_env().await;
    session.set_compact_review_mode(atman_runtime::CompactReviewMode::Always);
    let _sub = wait_for_pending_and_decide(
        session.clone(),
        CompactReviewDecision::AcceptEdited {
            summary: "user-crafted replacement summary".into(),
        },
    );
    maybe_auto_compact(&session, "mock-summary", &providers).await;
    let events = session.sink().snapshot();
    let (summary_text, _) = events
        .iter()
        .find_map(|e| match e {
            Event::ContextCompact {
                summary_text,
                replacement_msg_seq,
                ..
            } => Some((summary_text.clone(), *replacement_msg_seq)),
            _ => None,
        })
        .expect("expected ContextCompact event");
    assert!(
        summary_text
            .as_deref()
            .unwrap_or_default()
            .contains("user-crafted replacement"),
        "expected edited summary, got {summary_text:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_reject_skips_commit() {
    use atman_runtime::CompactReviewDecision;
    use atman_runtime::compaction::maybe_auto_compact;
    use atman_runtime::event::Event;
    let (_tmp, session, providers) = setup_review_env().await;
    session.set_compact_review_mode(atman_runtime::CompactReviewMode::Always);
    let before_count = session.message_count();
    let _sub = wait_for_pending_and_decide(session.clone(), CompactReviewDecision::Reject);
    maybe_auto_compact(&session, "mock-summary", &providers).await;
    let events = session.sink().snapshot();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::ContextCompact { .. })),
        "expected no ContextCompact event when rejected"
    );
    assert_eq!(
        session.message_count(),
        before_count,
        "transcript must be unchanged on reject"
    );
}

#[tokio::test]
async fn review_manual_only_skips_review_on_auto_path() {
    use atman_runtime::compaction::maybe_auto_compact;
    use atman_runtime::event::Event;
    let (_tmp, session, providers) = setup_review_env().await;
    session.set_compact_review_mode(atman_runtime::CompactReviewMode::ManualOnly);
    let _sub = session.compact_reviews().subscribe();
    maybe_auto_compact(&session, "mock-summary", &providers).await;
    let events = session.sink().snapshot();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::ContextCompact { .. })),
        "auto path with manual-only mode must commit without review"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_always_without_subscriber_auto_accepts_daemon_shape() {
    use atman_runtime::compaction::maybe_auto_compact;
    use atman_runtime::event::Event;
    let (_tmp, session, providers) = setup_review_env().await;
    session.set_compact_review_mode(atman_runtime::CompactReviewMode::Always);
    assert_eq!(session.compact_reviews().subscriber_count(), 0);
    maybe_auto_compact(&session, "mock-summary", &providers).await;
    let events = session.sink().snapshot();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::ContextCompact { .. })),
        "daemon-shape (always mode, no subscriber) must commit without hanging"
    );
}

#[tokio::test]
async fn cooldown_blocks_repeat_compaction_within_window() {
    let tmp = tempfile::tempdir().unwrap();
    let session = Session::open(tmp.path()).unwrap();
    session.record_llm_call("llama-3b", 0, 0, 0, 0, None, None);
    build_long_history(&session, 20);
    assert!(session.approval_cooldown_ok_for_compact());
    let _ = session.compact_messages("first".into()).unwrap();
    assert!(!session.approval_cooldown_ok_for_compact());
}
