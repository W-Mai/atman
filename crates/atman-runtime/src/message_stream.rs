//! Incrementally maintained message accumulator.  `window()` returns a
//! `MessageWindow` anchored at the last compaction summary (zero-copy);
//! `full_messages()` returns a shared `Arc<Vec<Message>>` of every message.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Deref;
use std::sync::{Arc, Mutex};

use crate::compaction::is_compaction_summary;
use crate::event::EventEnvelope;
use crate::message::Message;

#[derive(Clone)]
pub struct MessageWindow {
    messages: Arc<Vec<Message>>,
    start: usize,
}

impl MessageWindow {
    pub fn to_vec(&self) -> Vec<Message> {
        self.as_slice().to_vec()
    }

    fn as_slice(&self) -> &[Message] {
        &self.messages[self.start..]
    }
}

impl Deref for MessageWindow {
    type Target = [Message];

    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

struct Acc {
    compacted: Vec<(u64, Message)>,
    compacted_positions: HashMap<u64, usize>,
    full_raw: Vec<(u64, Message)>,
    full_positions: HashMap<u64, usize>,
    replayed: usize,
    projection_revision: u64,
    ownership: FlowOwnership,
    full_cache: Arc<Vec<Message>>,
    window_cache: MessageWindow,
}

#[derive(Default)]
struct FlowOwnership {
    children: HashMap<crate::event::FlowRunId, HashSet<crate::event::FlowRunId>>,
    spawned: HashSet<crate::event::FlowRunId>,
}

impl FlowOwnership {
    fn observe(&mut self, event: &crate::event::Event) {
        let crate::event::Event::FlowStart {
            run_id,
            parent_run_id,
            spawned,
            ..
        } = event
        else {
            return;
        };
        if let Some(parent) = parent_run_id {
            self.children
                .entry(parent.clone())
                .or_default()
                .insert(run_id.clone());
        }
        if !*spawned
            && !parent_run_id
                .as_ref()
                .is_some_and(|parent| self.spawned.contains(parent))
        {
            return;
        }
        let mut queue = VecDeque::from([run_id.clone()]);
        while let Some(parent) = queue.pop_front() {
            if !self.spawned.insert(parent.clone()) {
                continue;
            }
            if let Some(children) = self.children.get(&parent) {
                queue.extend(children.iter().cloned());
            }
        }
    }
}

pub struct MessageStream {
    events: Arc<Mutex<Vec<EventEnvelope>>>,
    acc: Mutex<Acc>,
}

impl MessageStream {
    pub fn new(events: Arc<Mutex<Vec<EventEnvelope>>>) -> Self {
        let empty = Arc::new(Vec::new());
        Self {
            events,
            acc: Mutex::new(Acc {
                compacted: Vec::new(),
                compacted_positions: HashMap::new(),
                full_raw: Vec::new(),
                full_positions: HashMap::new(),
                replayed: 0,
                projection_revision: 0,
                ownership: FlowOwnership::default(),
                full_cache: Arc::clone(&empty),
                window_cache: MessageWindow {
                    messages: empty,
                    start: 0,
                },
            }),
        }
    }

    pub fn with_initial(
        events: Arc<Mutex<Vec<EventEnvelope>>>,
        compacted: Vec<(u64, Message)>,
        raw: Vec<(u64, Message)>,
    ) -> Self {
        let full: Arc<Vec<Message>> = Arc::new(raw.iter().map(|(_, msg)| msg.clone()).collect());
        let window_messages: Arc<Vec<Message>> =
            Arc::new(compacted.iter().map(|(_, msg)| msg.clone()).collect());
        let start = window_messages
            .iter()
            .rposition(is_compaction_summary)
            .unwrap_or(0);
        let window = MessageWindow {
            messages: window_messages,
            start,
        };
        let projection_revision = u64::from(!compacted.is_empty() || !raw.is_empty());
        let compacted_positions = crate::projection::message_window::message_positions(&compacted);
        let full_positions = crate::projection::message_window::message_positions(&raw);
        Self {
            events,
            acc: Mutex::new(Acc {
                compacted,
                compacted_positions,
                full_raw: raw,
                full_positions,
                replayed: 0,
                projection_revision,
                ownership: FlowOwnership::default(),
                full_cache: full,
                window_cache: window,
            }),
        }
    }

    pub fn full_messages(&self) -> Arc<Vec<Message>> {
        let events = self.events.lock().expect("events poisoned");
        let mut acc = self.acc.lock().expect("acc poisoned");
        self.ensure_fresh_locked(&events, &mut acc);
        Arc::clone(&acc.full_cache)
    }

    pub fn window(&self) -> MessageWindow {
        let events = self.events.lock().expect("events poisoned");
        let mut acc = self.acc.lock().expect("acc poisoned");
        self.ensure_fresh_locked(&events, &mut acc);
        acc.window_cache.clone()
    }

    fn ensure_fresh_locked(&self, events: &[EventEnvelope], acc: &mut Acc) {
        if acc.replayed >= events.len() {
            return;
        }
        let mut compacted_changed = false;
        let mut full_changed = false;
        for event in &events[acc.replayed..] {
            acc.ownership.observe(&event.event);
        }
        for ev in &events[acc.replayed..] {
            compacted_changed |= crate::projection::message_window::apply_envelope_to_messages(
                ev,
                &acc.ownership.spawned,
                &mut acc.compacted,
                &mut acc.compacted_positions,
            );
            match &ev.event {
                crate::event::Event::UserMsg {
                    message,
                    flow_run_id,
                    ..
                }
                | crate::event::Event::AssistantMsg {
                    message,
                    flow_run_id,
                    ..
                }
                | crate::event::Event::ToolResultMsg {
                    message,
                    flow_run_id,
                    ..
                }
                | crate::event::Event::SystemMsg {
                    message,
                    flow_run_id,
                    ..
                }
                | crate::event::Event::DeferredFormApplied {
                    message,
                    flow_run_id,
                    ..
                } if crate::projection::message_window::message_belongs_to_root(
                    flow_run_id.as_ref(),
                    &acc.ownership.spawned,
                ) =>
                {
                    acc.full_positions.insert(ev.seq, acc.full_raw.len());
                    acc.full_raw.push((ev.seq, message.clone()));
                    full_changed = true;
                }
                crate::event::Event::AttachmentDegraded { .. } => {
                    full_changed |= crate::projection::message_window::apply_envelope_to_messages(
                        ev,
                        &acc.ownership.spawned,
                        &mut acc.full_raw,
                        &mut acc.full_positions,
                    );
                }
                _ => {}
            }
        }
        acc.replayed = events.len();

        if compacted_changed {
            let compacted = acc
                .compacted
                .iter()
                .map(|(_, message)| message.clone())
                .collect::<Vec<_>>();
            let start = compacted
                .iter()
                .rposition(is_compaction_summary)
                .unwrap_or(0);
            acc.window_cache = MessageWindow {
                messages: Arc::new(compacted),
                start,
            };
        }
        if full_changed {
            acc.full_cache = Arc::new(
                acc.full_raw
                    .iter()
                    .map(|(_, message)| message.clone())
                    .collect(),
            );
        }
        if compacted_changed || full_changed {
            acc.projection_revision = acc.projection_revision.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::TurnId;
    use crate::event::{Event, EventEnvelope};
    use crate::message::{MessageOrigin, MessagePart, MessageRole};

    fn user(text: &str) -> Message {
        Message {
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                text: text.to_string(),
            }],
            turn_id: TurnId::now(),
            origin: MessageOrigin::User,
        }
    }

    fn assistant(text: &str) -> Message {
        Message {
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Text {
                text: text.to_string(),
            }],
            turn_id: TurnId::now(),
            origin: MessageOrigin::User,
        }
    }

    fn compact_summary(text: &str) -> Message {
        Message::system_compact_summary(TurnId::now(), text, 0, 1, 2)
    }

    fn make_msg_event(ty: &str, msg: &Message, _seq: u64) -> Event {
        match ty {
            "user_msg" => Event::UserMsg {
                turn_id: msg.turn_id.clone(),
                flow_run_id: None,
                message: msg.clone(),
                presentation: None,
                injection_id: None,
            },
            "assistant_msg" => Event::AssistantMsg {
                turn_id: msg.turn_id.clone(),
                flow_run_id: None,
                message: msg.clone(),
            },
            "system_msg" => Event::SystemMsg {
                turn_id: msg.turn_id.clone(),
                flow_run_id: None,
                message: msg.clone(),
            },
            _ => unreachable!(),
        }
    }

    fn make_context_compact(
        range_start: u64,
        range_end: u64,
        before_tokens: u64,
        after_tokens: u64,
        summary_text: &str,
        replacement_msg_seq: u64,
    ) -> Event {
        Event::ContextCompact {
            session_id: "test".into(),
            flow_run_id: None,
            before_tokens,
            after_tokens,
            compacted_range_start: range_start,
            compacted_range_end: range_end,
            summary_text: Some(summary_text.into()),
            replacement_msg_seq: Some(replacement_msg_seq),
        }
    }

    fn event_envelopes(events: Vec<Event>) -> Arc<Mutex<Vec<EventEnvelope>>> {
        Arc::new(Mutex::new(
            events
                .into_iter()
                .enumerate()
                .map(|(i, event)| EventEnvelope::new((i + 1) as u64, event))
                .collect(),
        ))
    }

    #[test]
    fn full_messages_filters_only_message_events() {
        let u1 = user("hello");
        let a1 = assistant("hi there");
        let events = event_envelopes(vec![
            make_msg_event("user_msg", &u1, 1),
            Event::TurnStart {
                turn_id: TurnId::now(),
            },
            make_msg_event("assistant_msg", &a1, 2),
            Event::LlmCall {
                model: "m".into(),
                provider: "p".into(),
                context_plan_id: None,
                context_epoch: None,
                context_tokens: None,
                usage_source: None,
                context_call_purpose: None,
                context_call_identity: None,
                context_cache: None,
                assistant_tool_batch_width: None,
                usage: crate::provider::TokenUsage::default(),
                wallclock_ms: 0,
                ttft_ms: None,
                tokens_per_second: None,
                status: crate::event::LlmCallStatus::Ok,
                run_id: None,
                node_id: None,
            },
        ]);
        let ms = MessageStream::new(events);
        let msgs = ms.full_messages();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].text_concat(), "hello");
        assert_eq!(msgs[1].text_concat(), "hi there");
    }

    #[test]
    fn non_message_events_preserve_message_cache_identity() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let stream = MessageStream::with_initial(
            Arc::clone(&events),
            vec![(1, user("window"))],
            vec![(1, user("full"))],
        );
        let full_before = stream.full_messages();
        let window_before = stream.window();
        let revision_before = stream.acc.lock().unwrap().projection_revision;
        events.lock().unwrap().extend([
            EventEnvelope::new(
                2,
                Event::TurnStart {
                    turn_id: TurnId::now(),
                },
            ),
            EventEnvelope::new(
                3,
                Event::FlowStart {
                    run_id: crate::event::FlowRunId::now(),
                    flow_name: "root".into(),
                    parent_run_id: None,
                    parent_node_id: None,
                    spawned: false,
                },
            ),
            EventEnvelope::new(
                4,
                Event::LlmCall {
                    model: "model".into(),
                    provider: "provider".into(),
                    context_plan_id: None,
                    context_epoch: None,
                    context_tokens: None,
                    usage_source: None,
                    context_call_purpose: None,
                    context_call_identity: None,
                    context_cache: None,
                    assistant_tool_batch_width: None,
                    usage: crate::provider::TokenUsage::default(),
                    wallclock_ms: 0,
                    ttft_ms: None,
                    tokens_per_second: None,
                    status: crate::event::LlmCallStatus::Ok,
                    run_id: None,
                    node_id: None,
                },
            ),
        ]);

        let full_after = stream.full_messages();
        let window_after = stream.window();
        let acc = stream.acc.lock().unwrap();

        assert!(Arc::ptr_eq(&full_before, &full_after));
        assert!(Arc::ptr_eq(&window_before.messages, &window_after.messages));
        assert_eq!(acc.projection_revision, revision_before);
        assert_eq!(acc.replayed, 3);
    }

    #[test]
    fn compaction_rebuilds_window_without_cloning_full_history() {
        let summary = compact_summary("summary");
        let compacted = vec![
            (1, user("old")),
            (2, assistant("old")),
            (3, summary.clone()),
        ];
        let raw = compacted.clone();
        let events = Arc::new(Mutex::new(Vec::new()));
        let stream = MessageStream::with_initial(Arc::clone(&events), compacted, raw);
        let full_before = stream.full_messages();
        let window_before = stream.window();
        events.lock().unwrap().push(EventEnvelope::new(
            4,
            make_context_compact(0, 1, 100, 10, "summary", 3),
        ));

        let full_after = stream.full_messages();
        let window_after = stream.window();

        assert!(Arc::ptr_eq(&full_before, &full_after));
        assert!(!Arc::ptr_eq(
            &window_before.messages,
            &window_after.messages
        ));
        assert_eq!(window_after.len(), 1);
        assert!(matches!(
            window_after[0].parts[0],
            MessagePart::CompactSummary { .. }
        ));
    }

    #[test]
    fn attachment_degradation_updates_both_message_views() {
        let message = user("attachment");
        let events = Arc::new(Mutex::new(Vec::new()));
        let stream = MessageStream::with_initial(
            Arc::clone(&events),
            vec![(5, message.clone())],
            vec![(5, message)],
        );
        let full_before = stream.full_messages();
        let window_before = stream.window();
        events.lock().unwrap().push(EventEnvelope::new(
            6,
            Event::AttachmentDegraded {
                turn_id: None,
                flow_run_id: None,
                message_seq: 5,
                part_index: 0,
                file_basename: "image.png".into(),
                reason: "unreadable".into(),
            },
        ));

        let full_after = stream.full_messages();
        let window_after = stream.window();

        assert!(!Arc::ptr_eq(&full_before, &full_after));
        assert!(!Arc::ptr_eq(
            &window_before.messages,
            &window_after.messages
        ));
        assert_eq!(full_after[0].text_concat(), window_after[0].text_concat());
        assert!(full_after[0].text_concat().contains("image.png"));
    }

    #[test]
    fn window_no_summary_returns_all() {
        let events = vec![
            make_msg_event("user_msg", &user("a"), 1),
            make_msg_event("assistant_msg", &assistant("b"), 2),
            make_msg_event("user_msg", &user("c"), 3),
        ];
        let ms = MessageStream::new(event_envelopes(events));
        assert_eq!(ms.window().len(), 3);
    }

    #[test]
    fn window_single_summary_starts_from_it() {
        let s1 = compact_summary("summary 1");
        let events = vec![
            make_msg_event("user_msg", &user("old"), 1),
            make_msg_event("assistant_msg", &assistant("old"), 2),
            make_msg_event("system_msg", &s1, 3),
            make_msg_event("user_msg", &user("new"), 4),
            make_msg_event("assistant_msg", &assistant("new"), 5),
        ];
        let ms = MessageStream::new(event_envelopes(events));
        let w = ms.window();
        assert_eq!(w.len(), 3);
        assert!(matches!(w[0].parts[0], MessagePart::CompactSummary { .. }));
    }

    #[test]
    fn window_multiple_summaries_uses_last() {
        let s1 = compact_summary("summary 1");
        let s2 = compact_summary("summary 2");
        let events = vec![
            make_msg_event("system_msg", &s1, 1),
            make_msg_event("user_msg", &user("m1"), 2),
            make_msg_event("system_msg", &s2, 3),
            make_msg_event("user_msg", &user("m2"), 4),
        ];
        let ms = MessageStream::new(event_envelopes(events));
        let w = ms.window();
        assert_eq!(w.len(), 2);
        assert!(matches!(w[0].parts[0], MessagePart::CompactSummary { .. }));
        if let MessagePart::CompactSummary { summary, .. } = &w[0].parts[0] {
            assert_eq!(summary, "summary 2");
        }
    }

    #[test]
    fn window_no_prefix_before_summary() {
        let s1 = compact_summary("summary");
        let events = vec![
            make_msg_event("user_msg", &user("very old"), 1),
            make_msg_event("assistant_msg", &assistant("very old"), 2),
            make_msg_event("system_msg", &s1, 3),
            make_msg_event("user_msg", &user("new"), 4),
        ];
        let ms = MessageStream::new(event_envelopes(events));
        let w = ms.window();
        assert_eq!(w.len(), 2);
        assert!(matches!(w[0].parts[0], MessagePart::CompactSummary { .. }));
        assert_eq!(w[1].text_concat(), "new");
    }

    #[test]
    fn window_empty_stream_returns_empty() {
        let ms = MessageStream::new(event_envelopes(Vec::new()));
        assert!(ms.window().is_empty());
    }

    #[test]
    fn context_compact_replaces_range_with_summary() {
        let events = vec![
            make_msg_event("user_msg", &user("old u1"), 1),
            make_msg_event("assistant_msg", &assistant("old a1"), 2),
            make_msg_event("user_msg", &user("old u2"), 3),
            make_msg_event("system_msg", &compact_summary("summary"), 4),
            make_context_compact(0, 2, 100, 50, "compaction summary text", 4),
            make_msg_event("user_msg", &user("after compact"), 5),
        ];
        let ms = MessageStream::new(event_envelopes(events));
        let w = ms.window();
        assert_eq!(w.len(), 2);
        assert!(matches!(w[0].parts[0], MessagePart::CompactSummary { .. }));
    }

    #[test]
    fn multiple_compactions_applied_in_order() {
        let events = vec![
            make_msg_event("user_msg", &user("a"), 1),
            make_msg_event("assistant_msg", &assistant("b"), 2),
            make_msg_event("system_msg", &compact_summary("s1"), 3),
            make_context_compact(0, 1, 200, 100, "first summary", 3),
            make_msg_event("user_msg", &user("c"), 4),
            make_msg_event("assistant_msg", &assistant("d"), 5),
            make_msg_event("system_msg", &compact_summary("s2"), 6),
            make_context_compact(1, 2, 150, 80, "second summary", 7),
            make_msg_event("user_msg", &user("e"), 7),
        ];
        let ms = MessageStream::new(event_envelopes(events));
        let w = ms.window();
        assert_eq!(w.len(), 2);
        assert!(matches!(w[0].parts[0], MessagePart::CompactSummary { .. }));
        if let MessagePart::CompactSummary { summary, .. } = &w[0].parts[0] {
            assert_eq!(summary, "second summary");
        }
    }

    #[test]
    fn compact_then_user_message_produces_summary_plus_user() {
        let events = vec![
            make_msg_event("user_msg", &user("old u1"), 1),
            make_msg_event("assistant_msg", &assistant("old a1"), 2),
            make_msg_event("user_msg", &user("old u2"), 3),
            make_msg_event("system_msg", &compact_summary("compact summary"), 4),
            make_context_compact(0, 2, 200, 100, "compact summary", 4),
            make_msg_event("user_msg", &user("new message after compact"), 5),
        ];
        let ms = MessageStream::new(event_envelopes(events));
        let w = ms.window();
        assert_eq!(w.len(), 2);
        assert!(matches!(w[0].parts[0], MessagePart::CompactSummary { .. }));
        assert_eq!(w[1].text_concat(), "new message after compact");
    }

    #[test]
    fn no_compaction_window_equals_full_messages() {
        let events = vec![
            make_msg_event("user_msg", &user("first"), 1),
            make_msg_event("assistant_msg", &assistant("second"), 2),
            make_msg_event("user_msg", &user("third"), 3),
        ];
        let ms = MessageStream::new(event_envelopes(events));
        assert_eq!(ms.full_messages().len(), 3);
        assert_eq!(ms.window().len(), 3);
    }

    #[test]
    fn full_messages_retains_compacted_history() {
        let events = vec![
            make_msg_event("user_msg", &user("old u1"), 1),
            make_msg_event("assistant_msg", &assistant("old a1"), 2),
            make_msg_event("user_msg", &user("old u2"), 3),
            make_msg_event("system_msg", &compact_summary("summary"), 4),
            make_context_compact(0, 2, 200, 100, "summary", 4),
            make_msg_event("user_msg", &user("after compact"), 5),
        ];
        let ms = MessageStream::new(event_envelopes(events));
        // Window: only compact summary + messages after it
        let w = ms.window();
        assert_eq!(w.len(), 2);
        // Full: all messages including compacted ones
        let f = ms.full_messages();
        assert_eq!(f.len(), 5, "full must retain compacted messages");
        assert_eq!(f[0].text_concat(), "old u1");
        assert_eq!(f[1].text_concat(), "old a1");
        assert_eq!(f[2].text_concat(), "old u2");
    }

    #[test]
    fn third_compaction_replaces_second_summary() {
        let events = vec![
            make_msg_event("user_msg", &user("a"), 1),
            make_msg_event("assistant_msg", &assistant("b"), 2),
            make_msg_event("system_msg", &compact_summary("s1"), 3),
            make_context_compact(0, 1, 100, 50, "s1 text", 3),
            make_msg_event("user_msg", &user("c"), 4),
            make_msg_event("system_msg", &compact_summary("s2"), 5),
            make_context_compact(0, 1, 80, 40, "s2 text", 6),
            make_msg_event("user_msg", &user("d"), 6),
            make_msg_event("system_msg", &compact_summary("s3"), 7),
            make_context_compact(0, 1, 70, 30, "s3 text", 9),
            make_msg_event("user_msg", &user("final"), 8),
        ];
        let ms = MessageStream::new(event_envelopes(events));
        let w = ms.window();
        assert_eq!(w.len(), 2);
        if let MessagePart::CompactSummary { summary, .. } = &w[0].parts[0] {
            assert_eq!(summary, "s3 text");
        }
        assert_eq!(w[1].text_concat(), "final");
    }

    #[test]
    fn checkpoint_replaces_live_window_and_accepts_following_messages() {
        let old = user("old");
        let summary = compact_summary("checkpoint summary");
        let retained = user("retained current user");
        let events = event_envelopes(vec![
            make_msg_event("user_msg", &old, 1),
            Event::Checkpoint {
                session_id: "test".into(),
                flow_run_id: None,
                messages: vec![summary.clone(), retained.clone()],
                window_tokens: 10,
            },
            make_msg_event("assistant_msg", &assistant("next provider output"), 3),
        ]);
        let ms = MessageStream::new(events);

        let window = ms.window();
        assert_eq!(window.len(), 3);
        assert!(matches!(
            window[0].parts[0],
            MessagePart::CompactSummary { .. }
        ));
        assert_eq!(window[1].text_concat(), "retained current user");
        assert_eq!(window[2].text_concat(), "next provider output");
        assert!(!window.iter().any(|message| message.text_concat() == "old"));
    }

    #[test]
    fn reopened_session_uses_compacted_window_before_new_events() {
        let initial_compacted = vec![
            (10, compact_summary("checkpoint summary")),
            (11, assistant("retained tail")),
        ];
        let initial_raw = vec![(1, user("dead user")), (2, assistant("dead assistant"))];
        let ms = MessageStream::with_initial(
            Arc::new(Mutex::new(Vec::new())),
            initial_compacted,
            initial_raw,
        );

        let window = ms.window();
        assert_eq!(window.len(), 2);
        assert!(matches!(
            window[0].parts[0],
            MessagePart::CompactSummary { .. }
        ));
        assert_eq!(window[1].text_concat(), "retained tail");

        let full = ms.full_messages();
        assert_eq!(full.len(), 2);
        assert_eq!(full[0].text_concat(), "dead user");
        assert_eq!(full[1].text_concat(), "dead assistant");
    }

    #[test]
    fn reopened_session_keeps_initial_messages_after_new_events() {
        let initial_compacted = vec![
            (1, compact_summary("compaction summary")),
            (2, assistant("tail assistant")),
        ];
        let initial_raw = vec![
            (1, compact_summary("compaction summary")),
            (2, assistant("tail assistant")),
        ];
        let events = Arc::new(Mutex::new(Vec::new()));
        let ms = MessageStream::with_initial(events.clone(), initial_compacted, initial_raw);

        events.lock().unwrap().push(EventEnvelope::new(
            1,
            Event::TurnStart {
                turn_id: TurnId::now(),
            },
        ));
        events.lock().unwrap().push(EventEnvelope::new(
            2,
            Event::UserMsg {
                turn_id: TurnId::now(),
                flow_run_id: None,
                message: user("latest user"),
                presentation: None,
                injection_id: None,
            },
        ));

        let w = ms.window();
        assert_eq!(w.len(), 3);
        assert!(matches!(w[0].parts[0], MessagePart::CompactSummary { .. }));
        assert_eq!(w[1].text_concat(), "tail assistant");
        assert_eq!(w[2].text_concat(), "latest user");
    }

    /// Regression: after a runtime ContextCompact, full_messages() must still
    /// contain the pre-compact messages.
    #[test]
    fn full_messages_retains_pre_compact_history_after_runtime_compact() {
        let initial_compacted = vec![
            (1, compact_summary("prior summary")),
            (2, user("old user")),
            (3, assistant("old assistant")),
        ];
        let initial_raw = vec![
            (1, compact_summary("prior summary")),
            (2, user("old user")),
            (3, assistant("old assistant")),
        ];
        let events = Arc::new(Mutex::new(Vec::new()));
        let ms = MessageStream::with_initial(events.clone(), initial_compacted, initial_raw);

        events.lock().unwrap().push(EventEnvelope::new(
            10,
            Event::UserMsg {
                turn_id: TurnId::now(),
                flow_run_id: None,
                message: user("new user before compact"),
                presentation: None,
                injection_id: None,
            },
        ));
        events.lock().unwrap().push(EventEnvelope::new(
            11,
            Event::AssistantMsg {
                turn_id: TurnId::now(),
                flow_run_id: None,
                message: assistant("new assistant before compact"),
            },
        ));

        let before = ms.full_messages();
        assert_eq!(before.len(), 5, "pre-compact full should have all 5 msgs");

        events.lock().unwrap().push(EventEnvelope::new(
            12,
            Event::SystemMsg {
                turn_id: TurnId::now(),
                flow_run_id: None,
                message: compact_summary("runtime summary"),
            },
        ));
        events.lock().unwrap().push(EventEnvelope::new(
            13,
            Event::ContextCompact {
                session_id: "test".into(),
                flow_run_id: None,
                before_tokens: 1000,
                after_tokens: 100,
                compacted_range_start: 1,
                compacted_range_end: 2,
                summary_text: Some("runtime summary".into()),
                replacement_msg_seq: Some(12),
            },
        ));

        events.lock().unwrap().push(EventEnvelope::new(
            14,
            Event::UserMsg {
                turn_id: TurnId::now(),
                flow_run_id: None,
                message: user("after compact user"),
                presentation: None,
                injection_id: None,
            },
        ));

        let w = ms.window();
        assert_eq!(w.len(), 4, "window after compact");
        assert!(matches!(w[0].parts[0], MessagePart::CompactSummary { .. }));

        let full = ms.full_messages();
        assert!(
            full.len() >= 6,
            "full must retain pre-compact history, got {} msgs: {:?}",
            full.len(),
            full.iter().map(|m| m.text_concat()).collect::<Vec<_>>()
        );
        let texts: Vec<String> = full.iter().map(|m| m.text_concat()).collect();
        assert!(
            texts.iter().any(|t| t.contains("old user")),
            "full must contain pre-compact 'old user', got: {:?}",
            texts
        );
        assert!(
            texts.iter().any(|t| t.contains("old assistant")),
            "full must contain pre-compact 'old assistant', got: {:?}",
            texts
        );
    }

    #[test]
    fn spawned_compaction_and_checkpoint_do_not_mutate_root_window() {
        let child_run_id = crate::event::FlowRunId::now();
        let events = event_envelopes(vec![
            make_msg_event("user_msg", &user("root user"), 1),
            make_msg_event("assistant_msg", &assistant("root assistant"), 2),
            Event::FlowStart {
                run_id: child_run_id.clone(),
                flow_name: "child".into(),
                parent_run_id: None,
                parent_node_id: None,
                spawned: true,
            },
            Event::SystemMsg {
                turn_id: TurnId::now(),
                flow_run_id: Some(child_run_id.clone()),
                message: compact_summary("child summary"),
            },
            Event::ContextCompact {
                session_id: "test".into(),
                flow_run_id: Some(child_run_id.clone()),
                before_tokens: 100,
                after_tokens: 10,
                compacted_range_start: 0,
                compacted_range_end: 1,
                summary_text: Some("child summary".into()),
                replacement_msg_seq: Some(4),
            },
            Event::Checkpoint {
                session_id: "test".into(),
                flow_run_id: Some(child_run_id),
                messages: vec![compact_summary("child checkpoint")],
                window_tokens: 10,
            },
        ]);
        let stream = MessageStream::new(events);

        assert_eq!(
            stream
                .window()
                .iter()
                .map(Message::text_concat)
                .collect::<Vec<_>>(),
            ["root user", "root assistant"]
        );
        assert_eq!(stream.full_messages().len(), 2);
    }

    #[test]
    fn live_window_keeps_root_tree_once_and_excludes_spawned_tree() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let stream = MessageStream::new(Arc::clone(&events));
        let root = crate::event::FlowRunId::now();
        let ordinary = crate::event::FlowRunId::now();
        let spawned = crate::event::FlowRunId::now();
        let descendant = crate::event::FlowRunId::now();
        let flow_start = |run_id, parent_run_id, spawned| Event::FlowStart {
            run_id,
            flow_name: "test".into(),
            spawned,
            parent_run_id,
            parent_node_id: None,
        };

        events.lock().unwrap().extend([
            EventEnvelope::new(1, flow_start(root.clone(), None, false)),
            EventEnvelope::new(2, flow_start(ordinary.clone(), Some(root.clone()), false)),
            EventEnvelope::new(3, flow_start(spawned.clone(), Some(root.clone()), true)),
            EventEnvelope::new(
                4,
                flow_start(descendant.clone(), Some(spawned.clone()), false),
            ),
            EventEnvelope::new(
                5,
                Event::AssistantMsg {
                    turn_id: TurnId::now(),
                    flow_run_id: Some(root.clone()),
                    message: assistant("root one"),
                },
            ),
        ]);
        assert_eq!(stream.window().len(), 1);

        events.lock().unwrap().extend([
            EventEnvelope::new(
                6,
                Event::AssistantMsg {
                    turn_id: TurnId::now(),
                    flow_run_id: Some(ordinary),
                    message: assistant("ordinary one"),
                },
            ),
            EventEnvelope::new(
                7,
                Event::AssistantMsg {
                    turn_id: TurnId::now(),
                    flow_run_id: Some(spawned.clone()),
                    message: assistant("spawned one"),
                },
            ),
            EventEnvelope::new(
                8,
                Event::AssistantMsg {
                    turn_id: TurnId::now(),
                    flow_run_id: Some(descendant),
                    message: assistant("spawned descendant one"),
                },
            ),
        ]);
        let second = stream.window();
        assert_eq!(second.len(), 2);
        assert_eq!(second[0].text_concat(), "root one");
        assert_eq!(second[1].text_concat(), "ordinary one");

        events.lock().unwrap().push(EventEnvelope::new(
            9,
            Event::AssistantMsg {
                turn_id: TurnId::now(),
                flow_run_id: None,
                message: assistant("durable root"),
            },
        ));
        let third = stream.window();
        assert_eq!(third.len(), 3);
        assert_eq!(third[2].text_concat(), "durable root");
        assert_eq!(stream.window().len(), 3);

        events.lock().unwrap().extend([
            EventEnvelope::new(
                10,
                Event::SystemMsg {
                    turn_id: TurnId::now(),
                    flow_run_id: Some(root),
                    message: Message::system_text(TurnId::now(), "root system"),
                },
            ),
            EventEnvelope::new(
                11,
                Event::SystemMsg {
                    turn_id: TurnId::now(),
                    flow_run_id: Some(spawned),
                    message: Message::system_text(TurnId::now(), "spawned system"),
                },
            ),
            EventEnvelope::new(
                12,
                Event::SystemMsg {
                    turn_id: TurnId::now(),
                    flow_run_id: None,
                    message: Message::system_text(TurnId::now(), "durable system"),
                },
            ),
        ]);
        let fourth = stream.window();
        assert_eq!(fourth.len(), 5);
        assert!(
            fourth
                .iter()
                .any(|message| message.text_concat() == "root system")
        );
        assert!(
            fourth
                .iter()
                .all(|message| message.text_concat() != "spawned system")
        );
        assert!(
            fourth
                .iter()
                .any(|message| message.text_concat() == "durable system")
        );
    }

    #[test]
    fn batch_refresh_classifies_messages_after_collecting_flow_ownership() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let stream = MessageStream::new(Arc::clone(&events));
        let spawned = crate::event::FlowRunId::now();
        events.lock().unwrap().extend([
            EventEnvelope::new(
                1,
                Event::AssistantMsg {
                    turn_id: TurnId::now(),
                    flow_run_id: Some(spawned.clone()),
                    message: assistant("spawned output"),
                },
            ),
            EventEnvelope::new(
                2,
                Event::FlowStart {
                    run_id: spawned,
                    flow_name: "spawned".into(),
                    parent_run_id: None,
                    parent_node_id: None,
                    spawned: true,
                },
            ),
        ]);

        assert!(stream.window().is_empty());
        assert!(stream.full_messages().is_empty());
    }
}
