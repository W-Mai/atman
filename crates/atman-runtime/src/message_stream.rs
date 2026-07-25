//! Message window derived from the event log. Does not own a persistence
//! layer. `window()` replays in-memory events, applies `ContextCompact`
//! transformations, then returns from the last compaction summary (inclusive).
//! With no summary the full message list is returned.
//!
//! The accumulator is incrementally maintained: new events are replayed only
//! once, and the cache is reused across calls until invalidated.

use std::sync::{Arc, Mutex};

use crate::compaction::is_compaction_summary;
use crate::event::EventEnvelope;
use crate::message::Message;

/// Cached accumulator.  `replayed` tracks how many events have already been
/// applied so that subsequent `full_messages()` calls only replay new events.
struct Acc {
    messages: Vec<(u64, Message)>,
    replayed: usize,
}

pub struct MessageStream {
    events: Arc<Mutex<Vec<EventEnvelope>>>,
    initial_messages: Vec<Message>,
    acc: Mutex<Acc>,
}

impl MessageStream {
    pub fn new(events: Arc<Mutex<Vec<EventEnvelope>>>) -> Self {
        Self {
            events,
            initial_messages: Vec::new(),
            acc: Mutex::new(Acc {
                messages: Vec::new(),
                replayed: 0,
            }),
        }
    }

    /// Reopened sessions use this so the stream falls back to the
    /// pre-loaded messages until new events arrive.
    pub fn with_initial(events: Arc<Mutex<Vec<EventEnvelope>>>, initial: Vec<Message>) -> Self {
        Self {
            events,
            initial_messages: initial,
            acc: Mutex::new(Acc {
                messages: Vec::new(),
                replayed: 0,
            }),
        }
    }

    pub fn full_messages(&self) -> Vec<Message> {
        let events = self.events.lock().expect("events poisoned");
        let mut acc = self.acc.lock().expect("acc poisoned");

        if acc.messages.is_empty() {
            // First call: seed from initial messages.
            acc.messages = self
                .initial_messages
                .iter()
                .cloned()
                .map(|m| (0, m))
                .collect();
        }

        if acc.replayed < events.len() {
            // Replay only new events since the last call.
            for ev in &events[acc.replayed..] {
                crate::projection::message_window::apply_envelope_to_messages(
                    ev,
                    &mut acc.messages,
                );
            }
            acc.replayed = events.len();
        }

        acc.messages.iter().map(|(_, msg)| msg).cloned().collect()
    }

    pub fn window(&self) -> Vec<Message> {
        let full = self.full_messages();
        match full.iter().rposition(is_compaction_summary) {
            Some(idx) => full[idx..].to_vec(),
            None => full,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::TurnId;
    use crate::event::{Event, EventEnvelope};
    use crate::message::{MessagePart, MessageRole};

    fn user(text: &str) -> Message {
        Message {
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                text: text.to_string(),
            }],
            turn_id: TurnId::now(),
        }
    }

    fn assistant(text: &str) -> Message {
        Message {
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Text {
                text: text.to_string(),
            }],
            turn_id: TurnId::now(),
        }
    }

    fn compact_summary(text: &str) -> Message {
        Message::system_compact_summary(TurnId::now(), text, 0, 1, 2)
    }

    fn make_msg_event(ty: &str, msg: &Message, _seq: u64) -> Event {
        match ty {
            "user_msg" => Event::UserMsg {
                turn_id: msg.turn_id.clone(),
                message: msg.clone(),
            },
            "assistant_msg" => Event::AssistantMsg {
                turn_id: msg.turn_id.clone(),
                flow_run_id: None,
                message: msg.clone(),
            },
            "system_msg" => Event::SystemMsg {
                turn_id: msg.turn_id.clone(),
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
            make_context_compact(1, 2, 150, 80, "second summary", 6),
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
    fn reopened_session_keeps_initial_messages_after_new_events() {
        let initial = vec![
            compact_summary("compaction summary"),
            assistant("tail assistant"),
        ];
        let events = Arc::new(Mutex::new(Vec::new()));
        let ms = MessageStream::with_initial(events.clone(), initial);

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
                message: user("latest user"),
            },
        ));

        let w = ms.window();
        assert_eq!(w.len(), 3);
        assert!(matches!(w[0].parts[0], MessagePart::CompactSummary { .. }));
        assert_eq!(w[1].text_concat(), "tail assistant");
        assert_eq!(w[2].text_concat(), "latest user");
    }
}
