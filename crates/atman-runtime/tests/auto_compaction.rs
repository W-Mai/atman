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
async fn compact_messages_refreshes_window_from_compacted_history() {
    let tmp = tempfile::tempdir().unwrap();
    let session = Session::open(tmp.path()).unwrap();
    build_long_history(&session, 20);
    session.record_llm_call("llama-3b", 50_000, 0, 0, 0, None, None);

    let result = session
        .compact_messages("test summary".into())
        .expect("expected compaction");

    assert_eq!(session.last_input_tokens(), 0);
    assert_eq!(
        session.subscribe_context().borrow().window_tokens,
        result.after_tokens
    );
    assert_ne!(result.after_tokens, 50_000);
}

#[tokio::test(flavor = "current_thread")]
async fn workflow_second_llm_waits_for_compacted_session_history() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use atman_dsl::parse::parse_file;
    use atman_runtime::error::RuntimeError;
    use atman_runtime::event::{NodeEvent, Observable};
    use atman_runtime::message::{MessagePart, MessageRole};
    use atman_runtime::provider::{
        AssistantMessage, CallTiming, LlmRequest, Provider, StopReason, TokenUsage,
    };
    use atman_runtime::tool::BoxFut;
    use atman_runtime::{Executor, Value, tools};

    struct CompactingProvider {
        calls: AtomicUsize,
        normal_calls: AtomicUsize,
        second_call_tokens: std::sync::Mutex<Option<u64>>,
    }

    impl Provider for CompactingProvider {
        fn name(&self) -> &str {
            "workflow-compact"
        }

        fn call<'a>(
            &'a self,
            req: LlmRequest,
        ) -> BoxFut<'a, Result<AssistantMessage, RuntimeError>> {
            Box::pin(async move { self.reply(req).await })
        }

        fn call_streaming(&self, req: LlmRequest) -> Observable<AssistantMessage> {
            let (tx, events) = tokio::sync::broadcast::channel(4);
            let result = self.reply_sync(req);
            let output: BoxFut<'static, Result<AssistantMessage, RuntimeError>> =
                Box::pin(async move {
                    let _ = tx.send(NodeEvent::LlmDone { total_tokens: 0 });
                    result
                });
            Observable {
                output,
                events,
                cancel: tokio_util::sync::CancellationToken::new(),
            }
        }
    }

    impl CompactingProvider {
        async fn reply(&self, req: LlmRequest) -> Result<AssistantMessage, RuntimeError> {
            self.reply_sync(req)
        }

        fn reply_sync(&self, req: LlmRequest) -> Result<AssistantMessage, RuntimeError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let normal_idx = req
                .system
                .is_none()
                .then(|| self.normal_calls.fetch_add(1, Ordering::SeqCst));
            let input_tokens =
                atman_runtime::compaction::estimate_tokens_for_messages(&req.messages);
            if normal_idx == Some(1) {
                *self.second_call_tokens.lock().unwrap() = Some(input_tokens);
                assert!(
                    input_tokens < 6_400,
                    "second workflow LLM saw uncompacted history: {input_tokens} tokens"
                );
            }
            let turn_id = req
                .messages
                .first()
                .map(|m| m.turn_id.clone())
                .unwrap_or_else(TurnId::now);
            Ok(AssistantMessage {
                message: Message {
                    role: MessageRole::Assistant,
                    parts: vec![MessagePart::Text {
                        text: normal_idx.map_or_else(|| "summary".into(), |i| format!("reply {i}")),
                    }],
                    turn_id,
                },
                stop_reason: StopReason::End,
                token_usage: TokenUsage {
                    input: if normal_idx == Some(0) {
                        50_000
                    } else {
                        input_tokens
                    },
                    output: 1,
                    ..Default::default()
                },
                timing: CallTiming::default(),
                model: String::new(),
                response_id: None,
            })
        }
    }

    let provider = Arc::new(CompactingProvider {
        calls: AtomicUsize::new(0),
        normal_calls: AtomicUsize::new(0),
        second_call_tokens: std::sync::Mutex::new(None),
    });
    let session = Session::open_ephemeral();
    build_long_history(&session, 20);

    let mut ex = Executor::with_events(session.sink().clone());
    tools::register_tier_zero(&mut ex.tools);
    ex.providers.register(provider.clone());
    let file = parse_file(
        r#"flow start() -> string {
    first = llm { model: "llama-workflow-compact" context: session }
    second = llm { model: "llama-workflow-compact" context: session }
    return text_concat(second)
}"#,
    )
    .unwrap();

    let turn_id = TurnId::now();
    session.begin_turn(Message::user_text(turn_id.clone(), "run"));
    let out = ex
        .run_in_turn(&file, "start", vec![], Some(turn_id), Some(&session))
        .await
        .unwrap();
    session.end_turn();

    assert!(matches!(out, Value::Str(s) if s == "reply 1"));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 4);
    assert_eq!(provider.normal_calls.load(Ordering::SeqCst), 2);
    assert!(provider.second_call_tokens.lock().unwrap().is_some());
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
