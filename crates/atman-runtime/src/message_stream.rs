//! Message window derived from the event log. Does not own a persistence
//! layer. `window()` replays in-memory events, applies `ContextCompact`
//! transformations, then returns from the last compaction summary (inclusive).
//! With no summary the full message list is returned.

use std::sync::{Arc, Mutex};

use crate::compaction::is_compaction_summary;
use crate::event::Event;
use crate::message::Message;

pub struct MessageStream {
    events: Arc<Mutex<Vec<Event>>>,
    initial_messages: Vec<Message>,
}

impl MessageStream {
    pub fn new(events: Arc<Mutex<Vec<Event>>>) -> Self {
        Self {
            events,
            initial_messages: Vec::new(),
        }
    }

    /// Reopened sessions use this so the stream falls back to the
    /// pre-loaded messages until new events arrive.
    pub fn with_initial(events: Arc<Mutex<Vec<Event>>>, initial: Vec<Message>) -> Self {
        Self {
            events,
            initial_messages: initial,
        }
    }

    pub fn full_messages(&self) -> Vec<Message> {
        let guard = self.events.lock().expect("events poisoned");
        if guard.is_empty() {
            return self.initial_messages.clone();
        }
        let mut acc: Vec<(u64, Message)> = self
            .initial_messages
            .iter()
            .cloned()
            .map(|m| (0, m))
            .collect();
        replay_events_into(&mut acc, &guard);
        acc.into_iter().map(|(_, msg)| msg).collect()
    }

    pub fn window(&self) -> Vec<Message> {
        let full = self.full_messages();
        match full.iter().rposition(is_compaction_summary) {
            Some(idx) => full[idx..].to_vec(),
            None => full,
        }
    }
}

fn replay_events_into(acc: &mut Vec<(u64, Message)>, events: &[Event]) {
    for ev in events {
        let seq = ev.seq();
        match ev {
            Event::UserMsg { message, .. }
            | Event::AssistantMsg { message, .. }
            | Event::ToolResultMsg { message, .. }
            | Event::SystemMsg { message, .. } => {
                acc.push((seq, message.clone()));
            }
            Event::ContextCompact {
                compacted_range_start,
                compacted_range_end,
                replacement_msg_seq,
                summary_text,
                after_tokens,
                before_tokens,
                ..
            } => {
                let range_start = *compacted_range_start as usize;
                let range_end = *compacted_range_end as usize;
                if range_start > range_end || range_end >= acc.len() {
                    continue;
                }
                let Some(rep_seq) = replacement_msg_seq else {
                    continue;
                };
                let Some(rep_idx) = acc.iter().position(|(s, _)| s == rep_seq) else {
                    continue;
                };
                if after_tokens >= before_tokens {
                    continue;
                }
                let replacement = acc.remove(rep_idx);
                let removed_count = range_end - range_start + 1;
                for _ in 0..removed_count {
                    acc.remove(range_start);
                }
                let insertion_idx = range_start.min(acc.len());
                if let Some(summary) = summary_text {
                    acc.insert(
                        insertion_idx,
                        (
                            *rep_seq,
                            Message::system_compact_summary(
                                crate::event::TurnId::now(),
                                summary.clone(),
                                range_start as u64,
                                range_end as u64,
                                removed_count,
                            ),
                        ),
                    );
                } else {
                    acc.insert(insertion_idx, replacement);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::TurnId;
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

    fn make_msg_event(ty: &str, msg: &Message, seq: u64) -> Event {
        let ts = chrono::Utc::now();
        match ty {
            "user_msg" => Event::UserMsg {
                seq,
                turn_id: msg.turn_id.clone(),
                message: msg.clone(),
                ts,
            },
            "assistant_msg" => Event::AssistantMsg {
                seq,
                turn_id: msg.turn_id.clone(),
                flow_run_id: None,
                message: msg.clone(),
                ts,
            },
            "system_msg" => Event::SystemMsg {
                seq,
                turn_id: msg.turn_id.clone(),
                message: msg.clone(),
                ts,
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
            seq: 0,
            session_id: "test".into(),
            before_tokens,
            after_tokens,
            compacted_range_start: range_start,
            compacted_range_end: range_end,
            summary_text: Some(summary_text.into()),
            replacement_msg_seq: Some(replacement_msg_seq),
            ts: chrono::Utc::now(),
        }
    }

    #[test]
    fn full_messages_filters_only_message_events() {
        let u1 = user("hello");
        let a1 = assistant("hi there");
        let events = Arc::new(Mutex::new(vec![
            make_msg_event("user_msg", &u1, 1),
            Event::TurnStart {
                seq: 0,
                turn_id: TurnId::now(),
                ts: chrono::Utc::now(),
            },
            make_msg_event("assistant_msg", &a1, 2),
            Event::LlmCall {
                seq: 0,
                model: "m".into(),
                provider: "p".into(),
                usage: crate::provider::TokenUsage::default(),
                wallclock_ms: 0,
                ttft_ms: None,
                tokens_per_second: None,
                status: crate::event::LlmCallStatus::Ok,
                run_id: None,
                node_id: None,
                ts: chrono::Utc::now(),
            },
        ]));
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
        let ms = MessageStream::new(Arc::new(Mutex::new(events)));
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
        let ms = MessageStream::new(Arc::new(Mutex::new(events)));
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
        let ms = MessageStream::new(Arc::new(Mutex::new(events)));
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
        let ms = MessageStream::new(Arc::new(Mutex::new(events)));
        let w = ms.window();
        assert_eq!(w.len(), 2);
        assert!(matches!(w[0].parts[0], MessagePart::CompactSummary { .. }));
        assert_eq!(w[1].text_concat(), "new");
    }

    #[test]
    fn window_empty_stream_returns_empty() {
        let ms = MessageStream::new(Arc::new(Mutex::new(Vec::new())));
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
        let ms = MessageStream::new(Arc::new(Mutex::new(events)));
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
        let ms = MessageStream::new(Arc::new(Mutex::new(events)));
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
        let ms = MessageStream::new(Arc::new(Mutex::new(events)));
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
        let ms = MessageStream::new(Arc::new(Mutex::new(events)));
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
            make_context_compact(0, 1, 80, 40, "s2 text", 5),
            make_msg_event("user_msg", &user("d"), 6),
            make_msg_event("system_msg", &compact_summary("s3"), 7),
            make_context_compact(0, 1, 70, 30, "s3 text", 7),
            make_msg_event("user_msg", &user("final"), 8),
        ];
        let ms = MessageStream::new(Arc::new(Mutex::new(events)));
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

        events.lock().unwrap().push(Event::TurnStart {
            seq: 1,
            turn_id: TurnId::now(),
            ts: chrono::Utc::now(),
        });
        events.lock().unwrap().push(Event::UserMsg {
            seq: 2,
            turn_id: TurnId::now(),
            message: user("latest user"),
            ts: chrono::Utc::now(),
        });

        let w = ms.window();
        assert_eq!(w.len(), 3);
        assert!(matches!(w[0].parts[0], MessagePart::CompactSummary { .. }));
        assert_eq!(w[1].text_concat(), "tail assistant");
        assert_eq!(w[2].text_concat(), "latest user");
    }
}
