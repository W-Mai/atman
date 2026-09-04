//! Selective context replay from immutable ancestry boundaries.

use std::collections::{HashMap, HashSet};
use std::io;

use crate::compaction::is_compaction_summary;
use crate::event::{ContextBase, ContextId, Event, EventEnvelope};
use crate::message::Message;

use super::message_window::{
    FlowOwnership, apply_envelope_to_messages, message_belongs_to_root, message_positions,
};

/// Validated ancestry membership without materialized message contents.
#[derive(Debug, PartialEq)]
pub struct ContextSelection {
    pub context_id: Option<ContextId>,
    cutoffs: HashMap<Option<ContextId>, u64>,
}

impl ContextSelection {
    pub fn includes(&self, envelope: &EventEnvelope) -> bool {
        self.cutoffs
            .get(&envelope.context_id)
            .is_some_and(|end| envelope.seq <= *end)
    }
}

/// One materialized context. Raw history ignores checkpoints and range replacement.
#[derive(Debug, PartialEq)]
pub struct ContextReplay {
    pub selection: ContextSelection,
    pub(crate) compacted: Vec<(u64, Message)>,
    window_start: usize,
    pub raw: Vec<(u64, Message)>,
    /// Digest of the last checkpoint selected by the ancestry and cutoff.
    pub checkpoint_epoch: Option<String>,
}

impl ContextReplay {
    /// Whether an event falls inside this view's fixed ancestry boundaries.
    pub fn includes(&self, envelope: &EventEnvelope) -> bool {
        self.selection.includes(envelope)
    }

    pub fn window(&self) -> &[(u64, Message)] {
        &self.compacted[self.window_start..]
    }
}

struct CreatedContext {
    seq: u64,
    base: Option<ContextBase>,
}

struct ContextLineage {
    contexts: HashMap<ContextId, CreatedContext>,
    last_seq: u64,
}

impl ContextLineage {
    fn from_envelopes<'a>(events: impl Iterator<Item = &'a EventEnvelope>) -> io::Result<Self> {
        let mut lineage = Self {
            contexts: HashMap::new(),
            last_seq: 0,
        };
        let mut scoped = false;
        for envelope in events {
            scoped |= envelope.context_id.is_some()
                || matches!(
                    envelope.event,
                    Event::ContextCreated { .. } | Event::ContextHeadSelected { .. }
                );
            if matches!(envelope.event, Event::ContextHeadSelected { .. })
                && envelope.context_id.is_none()
            {
                return Err(invalid("context head selection has no identity"));
            }
            if scoped && envelope.seq <= lineage.last_seq {
                return Err(invalid(format!(
                    "context event sequence is not increasing: {}",
                    envelope.seq
                )));
            }
            if let Event::ContextCreated { base, .. } = &envelope.event {
                let id = envelope
                    .context_id
                    .as_ref()
                    .ok_or_else(|| invalid("context creation has no identity"))?;
                if lineage.contexts.contains_key(id) {
                    return Err(invalid(format!("context was created more than once: {id}")));
                }
                if let Some(base) = base {
                    lineage.validate_base(base)?;
                    if base.through_seq() >= envelope.seq {
                        return Err(invalid("context base must precede its creation"));
                    }
                }
                lineage.contexts.insert(
                    id.clone(),
                    CreatedContext {
                        seq: envelope.seq,
                        base: base.clone(),
                    },
                );
            } else if let Some(id) = &envelope.context_id
                && !lineage.contexts.contains_key(id)
            {
                return Err(invalid(format!("event refers to an unknown context: {id}")));
            }
            lineage.last_seq = lineage.last_seq.max(envelope.seq);
        }
        Ok(lineage)
    }

    fn validate_base(&self, base: &ContextBase) -> io::Result<()> {
        if base.through_seq() > self.last_seq {
            return Err(invalid("context boundary exceeds the available event log"));
        }
        if let Some(id) = base.context_id() {
            let created = self
                .contexts
                .get(id)
                .ok_or_else(|| invalid(format!("unknown context: {id}")))?;
            if base.through_seq() < created.seq {
                return Err(invalid("context boundary precedes its creation"));
            }
        }
        Ok(())
    }

    fn cutoffs(&self, target: &ContextBase) -> io::Result<HashMap<Option<ContextId>, u64>> {
        self.validate_base(target)?;
        let mut cutoffs = HashMap::new();
        let mut next = Some(target);
        while let Some(base) = next {
            cutoffs.insert(base.context_id().cloned(), base.through_seq());
            next = base
                .context_id()
                .and_then(|id| self.contexts[id].base.as_ref());
        }
        Ok(cutoffs)
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn retain_active_window(
    messages: &mut Vec<(u64, Message)>,
    positions: &mut HashMap<u64, usize>,
    start: &mut usize,
) {
    if *start > 0 {
        messages.drain(..*start);
        *positions = message_positions(messages);
        *start = 0;
    }
}

/// Selects the last accepted head, independent of later output from other contexts.
pub fn select_default_context<'a, I>(events: I) -> io::Result<ContextSelection>
where
    I: IntoIterator<Item = &'a EventEnvelope>,
    I::IntoIter: Clone,
{
    let events = events.into_iter();
    let mut head = None;
    let mut through_seq = 0;
    for envelope in events.clone() {
        through_seq = through_seq.max(envelope.seq);
        if matches!(envelope.event, Event::ContextHeadSelected { .. }) {
            head = envelope.context_id.clone();
        }
    }
    let target = match head {
        Some(context_id) => ContextBase::Context {
            context_id,
            through_seq,
        },
        None => ContextBase::LegacyRoot { through_seq },
    };
    let cutoffs = ContextLineage::from_envelopes(events)?.cutoffs(&target)?;
    Ok(ContextSelection {
        context_id: target.context_id().cloned(),
        cutoffs,
    })
}

/// Materializes the selected default ancestry while preserving raw history separately.
pub fn replay_default_context<'a, I>(events: I) -> io::Result<ContextReplay>
where
    I: IntoIterator<Item = &'a EventEnvelope>,
    I::IntoIter: Clone,
{
    let events = events.into_iter();
    let selection = select_default_context(events.clone())?;
    Ok(replay_selected_context(events, selection))
}

/// Replays one selected ancestry without materializing every historical branch.
/// Invalid or missing lineage is an error, never a fallback to unscoped history.
pub fn replay_context<'a, I>(events: I, target: &ContextBase) -> io::Result<ContextReplay>
where
    I: IntoIterator<Item = &'a EventEnvelope>,
    I::IntoIter: Clone,
{
    let events = events.into_iter();
    let cutoffs = ContextLineage::from_envelopes(events.clone())?.cutoffs(target)?;
    Ok(replay_selected_context(
        events,
        ContextSelection {
            context_id: target.context_id().cloned(),
            cutoffs,
        },
    ))
}

fn replay_selected_context<'a>(
    events: impl Iterator<Item = &'a EventEnvelope> + Clone,
    selection: ContextSelection,
) -> ContextReplay {
    let ownership = FlowOwnership::from_events(events.clone().map(|envelope| &envelope.event));
    let no_exclusions = HashSet::new();
    let mut window = Vec::new();
    let mut positions = HashMap::new();
    let mut window_start = 0;
    let mut raw = Vec::new();
    let mut raw_positions = HashMap::new();
    let mut checkpoint = None;
    for envelope in events {
        if !selection.includes(envelope) {
            continue;
        }
        if let Event::ContextCreated { inheritance, .. } = &envelope.event {
            retain_active_window(&mut window, &mut positions, &mut window_start);
            if *inheritance == crate::event::ContextInheritance::CompleteToolPairs {
                crate::message::retain_complete_tool_pairs_in(&mut window, |(_, message)| message);
                positions = message_positions(&window);
            }
        }
        // Selected typed contexts already establish ownership, including spawned runs.
        let excluded = if envelope.context_id.is_some() {
            &no_exclusions
        } else {
            &ownership.spawned
        };
        if let Event::Checkpoint {
            flow_run_id,
            messages,
            ..
        } = &envelope.event
            && message_belongs_to_root(flow_run_id.as_ref(), excluded)
        {
            checkpoint = Some(messages.as_slice());
        }
        if apply_envelope_to_messages(envelope, excluded, &mut window, &mut positions) {
            if let Some((message, _)) = envelope.event.context_message() {
                if is_compaction_summary(message) {
                    window_start = window.len() - 1;
                }
            } else {
                window_start = window
                    .iter()
                    .rposition(|(_, message)| is_compaction_summary(message))
                    .unwrap_or(0);
            }
        }
        if let Some((message, owner)) = envelope.event.context_message()
            && message_belongs_to_root(owner, excluded)
        {
            raw_positions.insert(envelope.seq, raw.len());
            raw.push((envelope.seq, message.replayed(envelope.seq, None)));
        }
        if matches!(envelope.event, Event::AttachmentDegraded { .. }) {
            apply_envelope_to_messages(envelope, excluded, &mut raw, &mut raw_positions);
        }
    }
    ContextReplay {
        selection,
        compacted: window,
        window_start,
        raw,
        checkpoint_epoch: checkpoint.map(crate::context_state::checkpoint_epoch_digest),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventSink, FlowRunId, TurnId};

    fn push(sink: &EventSink, text: &str, owner: Option<FlowRunId>) -> u64 {
        let turn_id = TurnId::now();
        sink.emit_returning_seq(Event::UserMsg {
            turn_id: turn_id.clone(),
            flow_run_id: owner,
            message: Message::user_text(turn_id, text),
        })
    }

    fn create(sink: &EventSink, base: Option<ContextBase>) -> (ContextId, EventSink) {
        let id = ContextId::now();
        let scoped = sink.clone().with_context(id.clone());
        scoped.emit(Event::ContextCreated {
            base,
            inheritance: crate::event::ContextInheritance::Full,
        });
        (id, scoped)
    }

    fn target(id: &ContextId, through_seq: u64) -> ContextBase {
        ContextBase::Context {
            context_id: id.clone(),
            through_seq,
        }
    }

    fn texts(messages: &[(u64, Message)]) -> Vec<String> {
        messages
            .iter()
            .map(|(_, message)| message.text_concat())
            .collect()
    }

    #[tokio::test]
    async fn branches_preserve_their_fixed_prefix_through_writer_and_replay() {
        let sink = EventSink::new();
        let child_run = FlowRunId::now();
        let start = Event::FlowStart {
            run_id: child_run.clone(),
            turn_id: None,
            flow_name: "child".into(),
            parent_run_id: None,
            parent_node_id: None,
            spawned: true,
        };
        sink.emit(start.clone());
        push(&sink, "legacy child", Some(child_run.clone()));
        let shared = push(&sink, "shared", None);
        let (left_id, left) = create(
            &sink,
            Some(ContextBase::LegacyRoot {
                through_seq: shared,
            }),
        );
        let left_suffix = push(&left, "left", None);
        let (right_id, right) = create(&sink, Some(target(&left_id, left_suffix)));
        let accepted_turn = TurnId::now();
        right.emit(Event::ContextHeadSelected {
            turn_id: accepted_turn.clone(),
        });
        push(&left, "late parent", None);
        let (empty_id, empty) = create(&sink, None);
        push(&empty, "isolated", None);
        right.emit(start);
        push(&right, "right", Some(child_run));
        let events = sink.snapshot_envelopes();
        let right_target = target(&right_id, sink.published_seq());
        let expected = replay_context(&events, &right_target).unwrap();
        assert_eq!(replay_default_context(&events).unwrap(), expected);
        assert_eq!(texts(expected.window()), ["shared", "left", "right"]);
        assert_eq!(expected.raw, expected.window());
        assert_eq!(
            texts(
                replay_context(&events, &target(&left_id, sink.published_seq()))
                    .unwrap()
                    .window()
            ),
            ["shared", "left", "late parent"]
        );
        assert_eq!(
            texts(
                replay_context(&events, &target(&empty_id, sink.published_seq()))
                    .unwrap()
                    .window()
            ),
            ["isolated"]
        );

        let dir = tempfile::tempdir().unwrap();
        let pattern = format!("{left_id}|{right_id}|{empty_id}|{accepted_turn}");
        let redactor = std::sync::Arc::new(crate::redact::Redactor::from_pairs(
            &[("context", &pattern)],
            crate::redact::RedactMode::Full,
        ));
        let writer =
            crate::event_writer::EventWriter::spawn_with(dir.path(), Some(redactor)).unwrap();
        for event in &events {
            writer.sender().send(event.clone()).unwrap();
        }
        assert_eq!(writer.flush().await.unwrap().seq, sink.published_seq());
        writer.shutdown().await;
        let restored = crate::event_log::replay::SessionReplay::from_path(
            &dir.path().join("events.jsonl"),
            None,
        )
        .unwrap();
        assert_eq!(
            replay_context(&restored.events, &right_target).unwrap(),
            expected
        );
        assert_eq!(replay_default_context(&restored.events).unwrap(), expected);
        assert_eq!(
            serde_json::to_value(&restored.events).unwrap(),
            serde_json::to_value(&events).unwrap()
        );
    }

    #[test]
    fn forks_inherit_active_windows_and_keep_raw_history_across_owned_compaction() {
        let sink = EventSink::new();
        push(&sink, "hidden prefix", None);
        push(&sink, "old", None);
        let base_summary = Message::system_compact_summary(TurnId::now(), "base", 1, 1, 1);
        let summary_seq = sink.emit_returning_seq(Event::SystemMsg {
            turn_id: base_summary.turn_id.clone(),
            flow_run_id: None,
            message: base_summary,
        });
        let compact = |sink: &EventSink, range: u64, replacement: u64, summary: &str| {
            sink.emit(Event::ContextCompact {
                session_id: "session".into(),
                flow_run_id: None,
                before_tokens: 100,
                after_tokens: 10,
                compacted_range_start: range,
                compacted_range_end: range,
                replacement_msg_seq: Some(replacement),
                summary_text: Some(summary.into()),
            });
        };
        compact(&sink, 1, summary_seq, "base");
        let (parent_id, parent) = create(
            &sink,
            Some(ContextBase::LegacyRoot {
                through_seq: sink.published_seq(),
            }),
        );
        push(&parent, "retained", None);
        let summary = Message::system_compact_summary(TurnId::now(), "branch", 0, 0, 1);
        let summary_seq = parent.emit_returning_seq(Event::SystemMsg {
            turn_id: summary.turn_id.clone(),
            flow_run_id: None,
            message: summary.clone(),
        });
        compact(&parent, 0, summary_seq, "branch");
        let cutoff = parent.published_seq();
        let (child_id, child) = create(&sink, Some(target(&parent_id, cutoff)));
        parent.emit(Event::Checkpoint {
            session_id: "session".into(),
            flow_run_id: None,
            messages: vec![Message::user_text(TurnId::now(), "parent checkpoint")],
            window_tokens: 10,
        });
        push(&child, "child", None);
        let selected = target(&child_id, sink.published_seq());
        let before = replay_context(&sink.snapshot_envelopes(), &selected).unwrap();
        assert_eq!(texts(before.window()), ["branch", "retained", "child"]);
        assert_eq!(before.window()[0].1, summary);
        assert_eq!(
            texts(&before.raw),
            [
                "hidden prefix",
                "old",
                "base",
                "retained",
                "branch",
                "child"
            ]
        );
        child.emit(Event::Checkpoint {
            session_id: "session".into(),
            flow_run_id: None,
            messages: vec![Message::user_text(TurnId::now(), "child checkpoint")],
            window_tokens: 10,
        });
        let events = sink.snapshot_envelopes();
        let after = replay_context(&events, &target(&child_id, sink.published_seq())).unwrap();
        assert_eq!(texts(after.window()), ["child checkpoint"]);
        assert_eq!(after.raw, before.raw);
        assert_eq!(replay_context(&events, &selected).unwrap(), before);
        assert_eq!(
            texts(
                replay_context(&events, &target(&parent_id, sink.published_seq()))
                    .unwrap()
                    .window()
            ),
            ["parent checkpoint"]
        );
    }

    #[test]
    fn inherited_window_selection_matches_live_filtering_without_rewriting_raw_history() {
        use crate::event::ContextInheritance;
        use crate::message::{ImageData, ImageSource, MessagePart, MessageRole};

        for scoped in [false, true] {
            for checkpoint in [false, true] {
                for inheritance in [
                    ContextInheritance::Full,
                    ContextInheritance::CompleteToolPairs,
                ] {
                    let sink = EventSink::new();
                    let (parent_id, parent) = if scoped {
                        let (id, owner) = create(&sink, None);
                        (Some(id), owner)
                    } else {
                        (None, sink.clone())
                    };
                    let turn = TurnId::now();
                    let tool_use = |id: &str| MessagePart::ToolUse {
                        id: id.into(),
                        name: "fs.read".into(),
                        input: serde_json::json!({"path": "README.md"}),
                        intent: None,
                    };
                    let tool_result = |id: &str| Message {
                        role: MessageRole::Tool,
                        parts: vec![MessagePart::ToolResult {
                            tool_use_id: id.into(),
                            content: "contents".into(),
                            is_error: false,
                        }],
                        turn_id: turn.clone(),
                        origin: Default::default(),
                    };
                    let emit = |owner: &EventSink, message: Message| {
                        let event = match message.role {
                            MessageRole::Tool => Event::ToolResultMsg {
                                turn_id: turn.clone(),
                                flow_run_id: None,
                                message,
                            },
                            _ => Event::AssistantMsg {
                                turn_id: turn.clone(),
                                flow_run_id: None,
                                message,
                            },
                        };
                        owner.emit(event);
                    };
                    let mut mixed = Message::assistant_text(turn.clone(), "keep this text");
                    mixed
                        .parts
                        .extend([tool_use("complete"), tool_use("parent-open")]);
                    mixed.parts.push(MessagePart::Image {
                        id: None,
                        source: ImageSource {
                            media_type: "image/png".into(),
                            data: ImageData::Base64 {
                                data: "AA==".into(),
                            },
                            detail: Default::default(),
                        },
                    });
                    mixed.ensure_part_ids();
                    let image_id = mixed.part_id(0, None, 3);
                    let only_open = Message {
                        parts: vec![tool_use("only-open")],
                        ..mixed.clone()
                    };
                    let initial = vec![
                        mixed,
                        tool_result("complete"),
                        tool_result("orphan"),
                        only_open,
                    ];
                    for message in initial.iter().cloned() {
                        emit(&parent, message);
                    }
                    if checkpoint {
                        parent.emit(Event::Checkpoint {
                            session_id: "session".into(),
                            flow_run_id: None,
                            messages: initial,
                            window_tokens: 10,
                        });
                    }
                    let base = match parent_id {
                        Some(ref id) => target(id, sink.published_seq()),
                        None => ContextBase::LegacyRoot {
                            through_seq: sink.published_seq(),
                        },
                    };
                    let parent_replay = replay_context(&sink.snapshot_envelopes(), &base).unwrap();
                    let mut expected = parent_replay.window().to_vec();
                    let mut live = expected.iter().map(|(_, m)| m.clone()).collect::<Vec<_>>();
                    if inheritance == ContextInheritance::CompleteToolPairs {
                        crate::message::retain_complete_tool_pairs(&mut live);
                        crate::message::retain_complete_tool_pairs_in(&mut expected, |(_, m)| m);
                        assert_eq!(expected.len(), 2);
                        assert!(expected[0].1.parts.iter().all(|part| !matches!(part, MessagePart::ToolUse { id, .. } if id == "parent-open")));
                    }
                    let child_id = ContextId::now();
                    let child = sink.clone().with_context(child_id.clone());
                    child.emit(Event::ContextCreated {
                        base: Some(base),
                        inheritance,
                    });
                    let stream = crate::message_stream::MessageStream::from_context(
                        sink.events_handle(),
                        child_id.clone(),
                    )
                    .unwrap();
                    assert_eq!(stream.window().to_vec(), live);
                    let before = replay_context(
                        &sink.snapshot_envelopes(),
                        &target(&child_id, sink.published_seq()),
                    )
                    .unwrap();
                    assert_eq!(before.window(), expected);
                    assert_eq!(before.raw, parent_replay.raw);
                    assert_eq!(before.checkpoint_epoch, parent_replay.checkpoint_epoch);
                    assert_eq!(
                        before.window()[0]
                            .1
                            .parts
                            .iter()
                            .find_map(|part| match part {
                                MessagePart::Image { id, .. } => *id,
                                _ => None,
                            }),
                        image_id
                    );

                    emit(&parent, tool_result("parent-open"));
                    let child_open = Message {
                        role: MessageRole::Assistant,
                        parts: vec![tool_use("child-open")],
                        turn_id: turn.clone(),
                        origin: Default::default(),
                    };
                    emit(&child, child_open.clone());
                    live.push(child_open);
                    assert_eq!(stream.window().to_vec(), live);
                    let child_base = target(&child_id, sink.published_seq());
                    let (grand_id, _) = create(&sink, Some(child_base.clone()));
                    let at_fork = replay_context(&sink.snapshot_envelopes(), &child_base).unwrap();
                    emit(&child, tool_result("child-open"));
                    let events = sink
                        .snapshot_envelopes()
                        .into_iter()
                        .map(|event| {
                            serde_json::from_slice::<EventEnvelope>(
                                &serde_json::to_vec(&event).unwrap(),
                            )
                            .unwrap()
                        })
                        .collect::<Vec<_>>();
                    let grand =
                        replay_context(&events, &target(&grand_id, sink.published_seq())).unwrap();
                    assert_ne!(grand.selection.context_id, at_fork.selection.context_id);
                    assert_eq!(grand.window(), at_fork.window());
                    assert_eq!(grand.raw, at_fork.raw);
                    assert_eq!(grand.checkpoint_epoch, at_fork.checkpoint_epoch);
                    let child_replay =
                        replay_context(&events, &target(&child_id, sink.published_seq())).unwrap();
                    assert_eq!(
                        child_replay
                            .window()
                            .iter()
                            .map(|(_, m)| m.clone())
                            .collect::<Vec<_>>(),
                        stream.window().to_vec()
                    );
                    assert_eq!(
                        child_replay
                            .raw
                            .iter()
                            .map(|(_, m)| m.clone())
                            .collect::<Vec<_>>(),
                        *stream.full_messages()
                    );
                }
            }
        }
        for policy in [serde_json::Value::Null, serde_json::json!("unknown")] {
            assert!(
                serde_json::from_value::<Event>(serde_json::json!({
                    "type": "context_created", "base": null, "inheritance": policy,
                }))
                .is_err()
            );
        }
        assert!(
            serde_json::from_value::<Event>(serde_json::json!({
                "type": "context_created", "base": null,
            }))
            .is_err()
        );
    }

    #[test]
    fn checkpoint_epoch_respects_ancestry_cutoffs_and_empty_checkpoints() {
        let sink = EventSink::new();
        let (parent_id, parent) = create(&sink, None);
        let checkpoint = vec![Message::user_text(TurnId::now(), "retained")];
        parent.emit(Event::Checkpoint {
            session_id: "session".into(),
            flow_run_id: None,
            messages: checkpoint.clone(),
            window_tokens: 10,
        });
        let (child_id, child) = create(&sink, Some(target(&parent_id, sink.published_seq())));
        parent.emit(Event::Checkpoint {
            session_id: "session".into(),
            flow_run_id: None,
            messages: Vec::new(),
            window_tokens: 0,
        });
        push(&child, "later", None);
        let inherited = target(&child_id, sink.published_seq());
        let expected = Some(crate::context_state::checkpoint_epoch_digest(&checkpoint));
        assert_eq!(
            replay_context(&sink.snapshot_envelopes(), &inherited)
                .unwrap()
                .checkpoint_epoch,
            expected
        );
        child.emit(Event::Checkpoint {
            session_id: "session".into(),
            flow_run_id: None,
            messages: Vec::new(),
            window_tokens: 0,
        });
        let (independent_id, _) = create(&sink, None);
        let events = sink.snapshot_envelopes();
        assert_eq!(
            replay_context(&events, &inherited)
                .unwrap()
                .checkpoint_epoch,
            expected
        );
        for id in [&parent_id, &child_id] {
            let replay = replay_context(&events, &target(id, sink.published_seq())).unwrap();
            assert!(replay.window().is_empty());
            assert_eq!(
                replay.checkpoint_epoch,
                Some(crate::context_state::checkpoint_epoch_digest(&[]))
            );
        }
        assert_eq!(
            replay_context(&events, &target(&independent_id, sink.published_seq()))
                .unwrap()
                .checkpoint_epoch,
            None
        );
    }

    #[test]
    fn captured_steering_and_attachment_patches_stay_in_the_selected_branch() {
        use crate::message::{ImageData, ImageSource, MessagePart};
        for stable_id in [false, true] {
            let sink = EventSink::new();
            let (parent_id, parent) = create(&sink, None);
            let mut image = Message::user_text(TurnId::now(), "image");
            image.parts.push(MessagePart::Image {
                id: None,
                source: ImageSource {
                    media_type: "image/png".into(),
                    data: ImageData::Base64 {
                        data: "AA==".into(),
                    },
                    detail: Default::default(),
                },
            });
            let image_seq = parent.emit_returning_seq(Event::UserMsg {
                turn_id: image.turn_id.clone(),
                flow_run_id: None,
                message: image.clone(),
            });
            let (child_id, child) = create(&sink, Some(target(&parent_id, image_seq)));
            let child_stream = crate::message_stream::MessageStream::from_context(
                sink.events_handle(),
                child_id.clone(),
            )
            .unwrap();
            let parent_stream = crate::message_stream::MessageStream::from_context(
                sink.events_handle(),
                parent_id.clone(),
            )
            .unwrap();
            let patch = |sink: &EventSink, reason: &str| {
                sink.emit(Event::AttachmentDegraded {
                    turn_id: Some(image.turn_id.clone()),
                    flow_run_id: None,
                    patch: crate::message::AttachmentPatch {
                        target: if stable_id {
                            crate::message::AttachmentTarget::Part {
                                part_id: image.part_id(image_seq, None, 1).unwrap(),
                            }
                        } else {
                            crate::message::AttachmentTarget::Legacy {
                                message_seq: image_seq,
                                part_index: 1,
                            }
                        },
                        file_basename: "image.png".into(),
                        reason: reason.into(),
                    },
                })
            };
            patch(&parent, "parent only");
            let mut injection =
                crate::injection::Injection::new_pending(image.turn_id.clone(), "source");
            child.emit(Event::UserInject {
                turn_id: image.turn_id.clone(),
                injection: injection.clone(),
                context_message: None,
            });
            injection.state = crate::injection::InjectionState::Injected;
            let captured = Message::user_text(image.turn_id.clone(), "captured steering");
            child.emit(Event::UserInject {
                turn_id: image.turn_id.clone(),
                injection,
                context_message: Some(captured.clone()),
            });
            let before = replay_context(
                &sink.snapshot_envelopes(),
                &target(&child_id, sink.published_seq()),
            )
            .unwrap();
            assert_eq!(
                before
                    .window()
                    .iter()
                    .map(|(_, message)| message)
                    .collect::<Vec<_>>(),
                [&image.replayed(image_seq, None), &captured]
            );
            if stable_id {
                child.emit(Event::Checkpoint {
                    session_id: "session".into(),
                    flow_run_id: None,
                    messages: vec![image.replayed(image_seq, None), captured.clone()],
                    window_tokens: 0,
                });
            }
            patch(&child, "child only");
            patch(&child, "must not overwrite the marker");
            let events = sink.snapshot_envelopes();
            let after = replay_context(&events, &target(&child_id, sink.published_seq())).unwrap();
            assert!(after.window()[0].1.text_concat().contains("child only"));
            let window = after
                .window()
                .iter()
                .map(|(_, message)| message.clone())
                .collect::<Vec<_>>();
            let raw = after
                .raw
                .iter()
                .map(|(_, message)| message.clone())
                .collect::<Vec<_>>();
            assert_eq!(raw, window);
            assert_eq!(child_stream.window().to_vec(), window);
            assert_eq!(*child_stream.full_messages(), raw);
            let parent =
                replay_context(&events, &target(&parent_id, sink.published_seq())).unwrap();
            assert_eq!(parent.window().len(), 1);
            assert!(parent.window()[0].1.text_concat().contains("parent only"));
            assert_eq!(
                parent_stream.window().to_vec(),
                parent
                    .window()
                    .iter()
                    .map(|(_, message)| message.clone())
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn invalid_lineage_never_falls_back_to_legacy_history() {
        let id = ContextId::now();
        let make = |seq, scope, base| {
            let mut event = EventEnvelope::new(
                seq,
                Event::ContextCreated {
                    base,
                    inheritance: crate::event::ContextInheritance::Full,
                },
            );
            event.context_id = scope;
            event
        };
        let first = make(1, Some(id.clone()), None);
        let invalid_cases = [
            vec![make(1, None, None)],
            vec![first.clone(), make(2, Some(id.clone()), None)],
            vec![make(1, Some(id.clone()), Some(target(&id, 0)))],
            vec![
                first.clone(),
                make(
                    2,
                    Some(ContextId::now()),
                    Some(target(&ContextId::now(), 1)),
                ),
            ],
            vec![
                first.clone(),
                make(2, Some(ContextId::now()), Some(target(&id, 0))),
            ],
            vec![
                first.clone(),
                make(2, Some(ContextId::now()), Some(target(&id, 2))),
            ],
            vec![first.clone(), make(1, Some(ContextId::now()), None)],
        ];
        for events in invalid_cases {
            assert_eq!(
                replay_context(&events, &target(&id, 1)).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            assert_eq!(
                replay_default_context(&events).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
        let mut before_creation = first.clone();
        before_creation.event = Event::RunCancelRequested {
            run_id: FlowRunId::now(),
        };
        assert!(replay_context(&[before_creation], &target(&id, 1)).is_err());
        for boundary in [0, 2] {
            assert!(replay_context(std::slice::from_ref(&first), &target(&id, boundary)).is_err());
        }
        for value in [
            serde_json::json!({"through_seq": 1}),
            serde_json::json!({"kind": "context", "through_seq": 1}),
        ] {
            assert!(serde_json::from_value::<ContextBase>(value).is_err());
        }
    }

    #[test]
    fn accepted_heads_select_messages_and_facts_at_the_same_boundaries() {
        let sink = EventSink::new();
        let legacy = push(&sink, "legacy", None);
        let (first_id, first) = create(
            &sink,
            Some(ContextBase::LegacyRoot {
                through_seq: legacy,
            }),
        );
        assert_eq!(
            texts(
                replay_default_context(&sink.snapshot_envelopes())
                    .unwrap()
                    .window()
            ),
            ["legacy"]
        );
        first.emit(Event::ContextHeadSelected {
            turn_id: TurnId::now(),
        });
        let first_input = push(&first, "first input", None);
        let (second_id, second) = create(&sink, Some(target(&first_id, first_input)));
        second.emit(Event::ContextHeadSelected {
            turn_id: TurnId::now(),
        });
        let second_input = push(&second, "second input", None);
        let late_first = first.emit_returning_seq(Event::Checkpoint {
            session_id: "session".into(),
            flow_run_id: None,
            messages: vec![Message::user_text(TurnId::now(), "late first checkpoint")],
            window_tokens: 10,
        });
        let (_, unrelated) = create(&sink, None);
        let unrelated_input = push(&unrelated, "unrelated", None);
        let events = sink.snapshot_envelopes();
        let selected = replay_default_context(events.iter()).unwrap();
        assert_eq!(
            select_default_context(events.iter()).unwrap(),
            selected.selection
        );
        assert_eq!(selected.selection.context_id, Some(second_id));
        assert_eq!(
            texts(selected.window()),
            ["legacy", "first input", "second input"]
        );
        assert_eq!(selected.raw, selected.window());
        for event in &events {
            if [legacy, first_input, second_input].contains(&event.seq) {
                assert!(selected.includes(event));
            }
            if [late_first, unrelated_input].contains(&event.seq) {
                assert!(!selected.includes(event));
            }
        }
        assert_eq!(
            texts(
                replay_context(&events, &target(&first_id, sink.published_seq()))
                    .unwrap()
                    .window()
            ),
            ["late first checkpoint"]
        );

        for scope in [None, Some(ContextId::now())] {
            let mut broken = events.clone();
            let mut selection = EventEnvelope::new(
                sink.published_seq() + 1,
                Event::ContextHeadSelected {
                    turn_id: TurnId::now(),
                },
            );
            selection.context_id = scope;
            broken.push(selection);
            assert_eq!(
                replay_default_context(&broken).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
        let empty = replay_default_context(&[]).unwrap();
        assert!(empty.selection.context_id.is_none());
        assert!(empty.window().is_empty());
        assert!(empty.raw.is_empty());
    }

    #[test]
    fn deep_lineage_retains_one_message_copy_per_event() {
        let sink = EventSink::new();
        let mut base = None;
        for _ in 0..2_000 {
            let (id, scoped) = create(&sink, base);
            push(&scoped, "turn", None);
            base = Some(target(&id, sink.published_seq()));
        }
        let replay = replay_context(&sink.snapshot_envelopes(), &base.unwrap()).unwrap();
        assert_eq!(replay.window().len(), 2_000);
        assert_eq!(replay.raw, replay.window());
    }
}
