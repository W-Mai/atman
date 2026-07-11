use atman_runtime::compaction::{CompactRange, find_compact_summaries, replace_range_with_summary};
use atman_runtime::event::TurnId;
use atman_runtime::message::{Message, MessagePart, MessageRole};

fn user(text: &str) -> Message {
    Message::user_text(TurnId::now(), text)
}

fn assistant(text: &str) -> Message {
    Message::assistant_text(TurnId::now(), text)
}

#[test]
fn replace_range_summary_contains_atman_compact_footer_markers() {
    let msgs = vec![user("m1"), assistant("m2"), user("m3"), user("tail")];
    let range = CompactRange {
        start: 0,
        end: 3,
        tokens_saved_estimate: 0,
    };
    let out = replace_range_with_summary(
        &msgs,
        &range,
        "gist: talked about m1..m3\n\n[atman:compact seq_start=0 seq_end=2 count=3]".into(),
        TurnId::now(),
    );
    let text = out[0].text_concat();
    assert!(text.contains("[atman:compact "), "footer missing: {text}");
    assert!(text.contains("count=3"));
}

#[test]
fn find_compact_summaries_extracts_footer_metadata_from_system_messages() {
    let footer = "[atman:compact seq_start=42 seq_end=87 count=45]";
    let msgs = vec![
        user("keep"),
        Message {
            role: MessageRole::System,
            parts: vec![MessagePart::Text {
                text: format!("summary body\n\n{footer}"),
            }],
            turn_id: TurnId::now(),
        },
        assistant("tail"),
    ];
    let summaries = find_compact_summaries(&msgs);
    assert_eq!(summaries.len(), 1);
    let s = &summaries[0];
    assert_eq!(s.message_index, 1);
    assert_eq!(s.seq_start, 42);
    assert_eq!(s.seq_end, 87);
    assert_eq!(s.count, 45);
}

#[test]
fn find_compact_summaries_ignores_plain_system_messages() {
    let msgs = vec![
        user("keep"),
        Message::system_text(TurnId::now(), "plain system without any footer"),
        assistant("tail"),
    ];
    let summaries = find_compact_summaries(&msgs);
    assert!(summaries.is_empty(), "unexpected hit: {summaries:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn replace_messages_range_emits_context_compact_event_and_marks_sink() {
    use atman_runtime::event::{Event, EventSink};
    use atman_runtime::tool::{Tool, ToolArgs, ToolCtx};
    use atman_runtime::tools::stdlib::ReplaceMessagesRange;
    use atman_runtime::value::Value;

    let sink = EventSink::new();
    let mut ctx = ToolCtx::new();
    ctx.events = Some(sink.clone());
    let msgs = vec![
        Value::Message(user(&"a".repeat(400))),
        Value::Message(assistant(&"b".repeat(400))),
        Value::Message(user(&"c".repeat(400))),
        Value::Message(user("tail")),
    ];
    let args = ToolArgs {
        positional: vec![
            Value::List(msgs),
            Value::Int(0),
            Value::Int(3),
            Value::Str("gist".into()),
        ],
        named: vec![],
    };
    let out = ReplaceMessagesRange.call(args, &ctx).await.unwrap();
    let Value::List(items) = out else {
        panic!("expected list");
    };
    assert_eq!(items.len(), 2, "3 replaced + 1 tail = 2 items");

    let hits: Vec<_> = sink
        .snapshot()
        .into_iter()
        .filter_map(|e| match e {
            Event::ContextCompact {
                before_tokens,
                after_tokens,
                compacted_range_start,
                compacted_range_end,
                ..
            } => Some((
                before_tokens,
                after_tokens,
                compacted_range_start,
                compacted_range_end,
            )),
            _ => None,
        })
        .collect();
    assert_eq!(hits.len(), 1, "one compact event expected");
    let (before, after, start, end) = hits[0];
    assert!(
        before >= after,
        "compact should not grow tokens: {before} -> {after}"
    );
    assert_eq!(start, 0);
    assert_eq!(end, 2);
    let ago = sink.last_compact_ago_seconds().expect("timestamp recorded");
    assert!(ago >= 0);
}

#[tokio::test(flavor = "current_thread")]
async fn replace_messages_range_sends_lifecycle_fire_signal() {
    use atman_runtime::event::EventSink;
    use atman_runtime::tool::{Tool, ToolArgs, ToolCtx};
    use atman_runtime::tools::stdlib::ReplaceMessagesRange;
    use atman_runtime::value::Value;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut ctx = ToolCtx::new();
    ctx.events = Some(EventSink::new());
    ctx.lifecycle_fire_tx = Some(tx);
    let msgs = vec![
        Value::Message(user(&"a".repeat(400))),
        Value::Message(assistant(&"b".repeat(400))),
        Value::Message(user(&"c".repeat(400))),
        Value::Message(user("tail")),
    ];
    let args = ToolArgs {
        positional: vec![
            Value::List(msgs),
            Value::Int(0),
            Value::Int(3),
            Value::Str("gist".into()),
        ],
        named: vec![],
    };
    let _ = ReplaceMessagesRange.call(args, &ctx).await.unwrap();
    let ev = rx.try_recv().expect("lifecycle fire signal expected");
    assert_eq!(ev, atman_dsl::ast::LifecycleEvent::ContextCompact);
}
