use crate::error::RuntimeError;
use crate::eval::ContextMode;
use crate::eval::llm_args::LlmNodeArgs;
use crate::value::Value;

pub struct LlmContext {
    pub messages: Vec<crate::message::Message>,
    pub budget_text: String,
    pub session_messages_len: usize,
    pub context_latest_epoch: Option<String>,
}

pub fn build_llm_context(
    args: &LlmNodeArgs,
    context_mode: ContextMode,
    session: Option<&std::sync::Arc<crate::session::Session>>,
    session_messages_handle: Option<
        &std::sync::Arc<std::sync::Mutex<Vec<crate::message::Message>>>,
    >,
    turn_id: &crate::event::TurnId,
    events: Option<&crate::event::EventSink>,
    flow_run_id: Option<&crate::event::FlowRunId>,
) -> Result<LlmContext, Value> {
    let session_snapshot = if let Some(session) = session {
        Some(session.messages().to_vec())
    } else {
        session_messages_handle.map(|handle| handle.lock().unwrap().clone())
    };
    let live_records = session_snapshot
        .as_deref()
        .map(crate::context_plan::latest_live_context_record_messages)
        .unwrap_or_default();
    let (final_messages, prompt_for_budget) = if let Some(msgs) = args.messages_override.clone() {
        let budget_text = msgs.last().map(|m| m.text_concat()).unwrap_or_default();
        let mut messages = live_records;
        messages.extend(msgs);
        (messages, budget_text)
    } else if !matches!(context_mode, ContextMode::None) {
        let mut history = session_snapshot
            .as_deref()
            .map(|messages| project_session_messages(messages, context_mode, turn_id))
            .unwrap_or_default();
        let budget_text = args.prompt.clone().unwrap_or_default();
        if let Some(p) = args.prompt.clone()
            && !p.is_empty()
        {
            history.push(crate::message::Message::user_text(turn_id.clone(), p));
        }
        (history, budget_text)
    } else {
        let Some(mut prompt_text) = args.prompt.clone() else {
            return Err(Value::Err(RuntimeError::MissingArg(
                "llm node: either `prompt:` or `messages:` required".into(),
            )));
        };
        if let Some(budget) = args.context_budget {
            let (truncated, stat) = super::truncate_prompt_to_budget_tracked(prompt_text, budget);
            prompt_text = truncated;
            if let (Some(sink), Some(stat)) = (events, stat) {
                sink.emit(crate::event::Event::ContextTruncated {
                    turn_id: Some(turn_id.clone()),
                    flow_run_id: flow_run_id.cloned(),
                    original_chars: stat.original_chars as u64,
                    result_chars: stat.result_chars as u64,
                    dropped_chars: stat.dropped_chars as u64,
                    budget_tokens: stat.budget_tokens,
                });
            }
        }
        let user_msg = crate::message::Message::user_text(turn_id.clone(), prompt_text.clone());
        let mut messages = live_records;
        messages.push(user_msg);
        (messages, prompt_text)
    };
    let mut final_messages = final_messages;
    let context_latest_epoch = project_context_records(&mut final_messages, args);
    move_current_turn_interjections_to_tail(&mut final_messages, turn_id);
    let session_messages_len = final_messages.len();

    Ok(LlmContext {
        messages: final_messages,
        budget_text: prompt_for_budget,
        session_messages_len,
        context_latest_epoch,
    })
}

pub(super) fn project_context_records(
    messages: &mut Vec<crate::message::Message>,
    args: &LlmNodeArgs,
) -> Option<String> {
    use crate::context_plan::ContextRecordRetention;
    use crate::message::MessagePart;

    let mut latest = std::collections::HashMap::new();
    if let Some(keys) = &args.context_latest {
        for (message_index, message) in messages.iter().enumerate() {
            for (part_index, part) in message.parts.iter().enumerate() {
                if let MessagePart::ContextRecord(record) = part
                    && record.retention() == ContextRecordRetention::Latest
                    && let Some(key) = keys.iter().find(|key| key.as_str() == record.key())
                {
                    latest.insert(key.as_str(), (message_index, part_index));
                }
            }
        }
    }
    let context_latest_epoch = args
        .context_latest
        .as_ref()
        .filter(|keys| !keys.is_empty())
        .map(|keys| {
            let selected: std::collections::BTreeMap<_, _> = keys
                .iter()
                .map(|key| {
                    let identity = latest.get(key.as_str()).map(|(message, part)| {
                        let MessagePart::ContextRecord(record) = &messages[*message].parts[*part]
                        else {
                            unreachable!("latest record index must refer to a context record");
                        };
                        (record.revision(), record.digest().as_str())
                    });
                    (key.as_str(), identity)
                })
                .collect();
            let bytes = serde_json::to_vec(&selected)
                .expect("selected context record identity must serialize");
            format!("blake3:{}", blake3::hash(&bytes).to_hex())
        });
    if args.context_records.is_none() && latest.is_empty() {
        return context_latest_epoch;
    }
    let mut message_index = 0;
    messages.retain_mut(|message| {
        let had_records = message.contains_context_record();
        let mut part_index = 0;
        message.parts.retain(|part| {
            let position = (message_index, part_index);
            part_index += 1;
            let MessagePart::ContextRecord(record) = part else {
                return true;
            };
            args.context_records
                .as_ref()
                .is_none_or(|keys| keys.iter().any(|key| key == record.key()))
                && (record.retention() != ContextRecordRetention::Latest
                    || latest
                        .get(record.key())
                        .is_none_or(|latest| *latest == position && !record.body().is_tombstone()))
        });
        message_index += 1;
        !had_records || !message.parts.is_empty()
    });
    context_latest_epoch
}

pub(super) fn projected_context_epoch(
    projection: Option<&str>,
    epoch: Option<String>,
) -> Option<String> {
    let Some(projection) = projection else {
        return epoch;
    };
    let bytes = serde_json::to_vec(&(epoch, projection))
        .expect("selected context record epoch must serialize");
    Some(format!("blake3:{}", blake3::hash(&bytes).to_hex()))
}

pub(super) fn move_current_turn_interjections_to_tail(
    messages: &mut Vec<crate::message::Message>,
    turn_id: &crate::event::TurnId,
) {
    let last_reply = messages.iter().rposition(|message| {
        message.role == crate::message::MessageRole::Assistant && message.turn_id == *turn_id
    });
    let mut interjections = Vec::new();
    let mut index = 0;
    messages.retain(|message| {
        let move_to_tail = last_reply.is_none_or(|reply| index > reply)
            && matches!(
                message.origin,
                crate::message::MessageOrigin::Interjection
                    | crate::message::MessageOrigin::Peer
                    | crate::message::MessageOrigin::ChildResult
            )
            && message.turn_id == *turn_id;
        index += 1;
        if move_to_tail {
            interjections.push(message.clone());
            false
        } else {
            true
        }
    });
    messages.extend(interjections);
}

pub(super) fn project_session_messages(
    messages: &[crate::message::Message],
    context_mode: ContextMode,
    current_turn_id: &crate::event::TurnId,
) -> Vec<crate::message::Message> {
    let visible = |message: &&crate::message::Message| {
        message.origin != crate::message::MessageOrigin::Internal
            || message.turn_id == *current_turn_id
            || message.contains_context_record()
    };
    match context_mode {
        ContextMode::Session => messages.iter().filter(visible).cloned().collect(),
        ContextMode::SessionRecent(n) => {
            let mut projected = crate::context_plan::latest_live_context_record_messages(messages);
            let last_reply = messages.iter().rposition(|message| {
                message.role == crate::message::MessageRole::Assistant
                    && message.turn_id == *current_turn_id
            });
            let ordinary: Vec<_> = messages
                .iter()
                .enumerate()
                .filter(|(_, message)| visible(message))
                .filter_map(|(source_index, message)| {
                    let mut message = message.clone();
                    message.parts.retain(|part| {
                        !matches!(part, crate::message::MessagePart::ContextRecord(_))
                    });
                    (!message.parts.is_empty()).then_some((source_index, message))
                })
                .collect();
            let start = ordinary.len().saturating_sub(n);
            projected.extend(ordinary.into_iter().enumerate().filter_map(
                |(index, (source_index, message))| {
                    (index >= start
                        || (matches!(
                            message.origin,
                            crate::message::MessageOrigin::Interjection
                                | crate::message::MessageOrigin::Peer
                                | crate::message::MessageOrigin::ChildResult
                        ) && message.turn_id == *current_turn_id
                            && last_reply.is_none_or(|reply| source_index > reply)))
                    .then_some(message)
                },
            ));
            projected
        }
        ContextMode::None => crate::context_plan::latest_live_context_record_messages(messages),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::llm_args::LlmNodeArgs;
    use crate::event::TurnId;
    use crate::message::{Message, MessageOrigin, MessagePart, MessageRole};

    fn args() -> LlmNodeArgs {
        LlmNodeArgs {
            model: None,
            prompt: None,
            messages_override: None,
            system: None,
            input: crate::value::Value::Unit,
            retry_count: 0,
            retry_kinds: None,
            cache_prompt: false,
            context_budget: None,
            context_mode: "session".into(),
            context_records: None,
            context_latest: None,
            record_response: true,
            return_response: false,
            fallback_value: None,
            tool_specs: Vec::new(),
            mcp_scope: Default::default(),
            reasoning: None,
            call_purpose: crate::context_plan::ContextCallPurpose::General,
            stall_timeout_secs: 0,
        }
    }

    #[test]
    fn latest_reference_replaces_its_images_and_compaction_retains_only_the_current_record() {
        use crate::context_plan::{
            ContextRecord, ContextRecordAuthority, ContextRecordBody, ContextRecordRetention,
        };
        let turn = TurnId::now();
        let image = |suffix: u8| {
            let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\r".to_vec();
            bytes.push(suffix);
            crate::attachment_store::AttachmentStore::at("")
                .import_bytes(&bytes, None)
                .unwrap()
        };
        let old_image = image(1);
        let new_image = image(2);
        let record = |revision, image| {
            ContextRecord::new(
                "quickask.source",
                revision,
                ContextRecordAuthority::Retrieved,
                ContextRecordRetention::Latest,
                ContextRecordBody::Reference {
                    text: "same text".into(),
                    images: vec![image],
                },
            )
        };
        let old = record(1, old_image);
        let new = record(2, new_image.clone());
        assert_ne!(old.digest(), new.digest());
        let original = vec![
            Message::context_record(turn.clone(), old),
            message("question"),
            Message::context_record(turn.clone(), new),
        ];
        let mut selected = original.clone();
        let mut arguments = args();
        arguments.context_latest = Some(vec!["quickask.source".into()]);
        let epoch = project_context_records(&mut selected, &arguments).unwrap();
        let records = selected
            .iter()
            .flat_map(|message| &message.parts)
            .filter_map(|part| match part {
                MessagePart::ContextRecord(record) => Some(record),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].body().images(), std::slice::from_ref(&new_image));
        let tokens = crate::compaction::estimate_tokens_for_messages(&selected);
        assert!(tokens >= 1024);
        let compacted = crate::compaction::replace_range_with_summary(
            &original,
            &crate::compaction::CompactRange {
                start: 0,
                end: original.len(),
                tokens_saved_estimate: 0,
            },
            "question summary".into(),
            turn,
        );
        let mut compacted = compacted;
        assert_eq!(
            project_context_records(&mut compacted, &arguments).unwrap(),
            epoch
        );
        let images = compacted
            .iter()
            .flat_map(|message| &message.parts)
            .filter_map(|part| match part {
                MessagePart::ContextRecord(record) => Some(record.body().images()),
                _ => None,
            })
            .flatten()
            .collect::<Vec<_>>();
        assert_eq!(images, [&new_image]);
        assert_eq!(original.len(), 3);
    }

    fn message(text: &str) -> Message {
        Message {
            role: MessageRole::User,
            parts: vec![MessagePart::Text { text: text.into() }],
            turn_id: TurnId::now(),
            origin: MessageOrigin::User,
        }
    }

    #[test]
    fn record_selection_is_request_local_for_root_child_and_explicit_contexts() {
        let turn = TurnId::now();
        let record = |key| {
            Message::context_record(
                turn.clone(),
                crate::context_plan::ContextRecord::new(
                    key,
                    1,
                    crate::context_plan::ContextRecordAuthority::User,
                    crate::context_plan::ContextRecordRetention::Latest,
                    crate::context_plan::ContextRecordBody::text(key),
                ),
            )
        };
        let original = vec![
            record("session.goal"),
            record("agent.rule.full"),
            message("conversation"),
        ];
        let session = std::sync::Arc::new(crate::session::Session::open_ephemeral());
        for message in &original {
            session.append_message(message.clone(), None);
        }
        let local = std::sync::Arc::new(std::sync::Mutex::new(original.clone()));
        for root in [false, true] {
            for mode in [
                ContextMode::None,
                ContextMode::Session,
                ContextMode::SessionRecent(1),
            ] {
                for keys in [None, Some(vec![]), Some(vec!["session.goal".to_string()])] {
                    let mut arguments = args();
                    arguments.prompt = Some("current request".into());
                    arguments.context_records = keys.clone();
                    let context = build_llm_context(
                        &arguments,
                        mode,
                        root.then_some(&session),
                        Some(&local),
                        &turn,
                        None,
                        None,
                    )
                    .unwrap();
                    let record_count = context
                        .messages
                        .iter()
                        .filter(|message| message.contains_context_record())
                        .count();
                    assert_eq!(record_count, keys.as_ref().map_or(2, Vec::len));
                    assert!(
                        context
                            .messages
                            .iter()
                            .any(|message| message.text_concat() == "current request")
                    );
                    arguments.prompt = None;
                    arguments.messages_override = Some(original.clone());
                    let context = build_llm_context(
                        &arguments,
                        ContextMode::None,
                        root.then_some(&session),
                        Some(&local),
                        &turn,
                        None,
                        None,
                    )
                    .unwrap();
                    assert!(
                        context
                            .messages
                            .iter()
                            .any(|message| message.text_concat() == "conversation")
                    );
                    assert_eq!(
                        context
                            .messages
                            .iter()
                            .filter(|message| message.contains_context_record())
                            .count(),
                        keys.as_ref().map_or(4, |keys| 2 * keys.len())
                    );
                }
            }
        }
        assert_eq!(local.lock().unwrap().len(), original.len());
        assert_eq!(session.messages().len(), original.len());
    }

    #[test]
    fn latest_record_projection_is_request_local_and_preserves_timeline() {
        use crate::context_plan::{
            ContextRecord, ContextRecordAuthority, ContextRecordBody, ContextRecordRetention,
        };

        let turn = TurnId::now();
        let record = |key, revision, retention, body| {
            Message::context_record(
                turn.clone(),
                ContextRecord::new(
                    key,
                    revision,
                    ContextRecordAuthority::Retrieved,
                    retention,
                    body,
                ),
            )
        };
        let mut newest = record(
            "source",
            2,
            ContextRecordRetention::Latest,
            ContextRecordBody::text("new reference"),
        );
        newest.parts.push(MessagePart::Text {
            text: "retain mixed content".into(),
        });
        let original = vec![
            record(
                "source",
                1,
                ContextRecordRetention::Latest,
                ContextRecordBody::text("old reference"),
            ),
            record(
                "source",
                1,
                ContextRecordRetention::Timeline,
                ContextRecordBody::text("historical observation"),
            ),
            record(
                "other",
                1,
                ContextRecordRetention::Latest,
                ContextRecordBody::text("unselected record"),
            ),
            message("original question"),
            newest,
        ];
        let saved = serde_json::to_value(&original).unwrap();
        let local = std::sync::Arc::new(std::sync::Mutex::new(original.clone()));
        let session = std::sync::Arc::new(crate::session::Session::open_ephemeral());
        for message in &original {
            session.append_message(message.clone(), None);
        }
        let mut arguments = args();
        arguments.context_latest = Some(vec!["source".into()]);
        for root in [false, true] {
            let context = build_llm_context(
                &arguments,
                ContextMode::Session,
                root.then_some(&session),
                Some(&local),
                &turn,
                None,
                None,
            )
            .unwrap();
            let text = context
                .messages
                .iter()
                .map(Message::text_concat)
                .collect::<String>();
            assert!(!text.contains("old reference"));
            for retained in [
                "new reference",
                "historical observation",
                "unselected record",
                "original question",
                "retain mixed content",
            ] {
                assert!(text.contains(retained), "missing {retained}");
            }
        }
        assert_eq!(
            serde_json::to_value(&*local.lock().unwrap()).unwrap(),
            saved
        );
        assert_eq!(serde_json::to_value(&*session.messages()).unwrap(), saved);

        let mut cleared = original.clone();
        cleared.push(record(
            "source",
            3,
            ContextRecordRetention::Latest,
            ContextRecordBody::tombstone(),
        ));
        let _ = project_context_records(&mut cleared, &arguments);
        assert!(
            cleared
                .iter()
                .flat_map(|message| &message.parts)
                .all(|part| {
                    !matches!(part, MessagePart::ContextRecord(record)
                if record.key() == "source" && record.retention() == ContextRecordRetention::Latest)
                })
        );
        assert!(
            cleared
                .iter()
                .any(|message| { message.text_concat().contains("historical observation") })
        );
        arguments.context_records = Some(vec![]);
        let _ = project_context_records(&mut cleared, &arguments);
        assert!(!cleared.iter().any(Message::contains_context_record));
        assert!(
            cleared
                .iter()
                .any(|message| { message.text_concat() == "retain mixed content" })
        );
    }

    #[test]
    fn latest_record_epoch_changes_only_with_the_selected_projection() {
        use crate::context_plan::{
            ContextRecord, ContextRecordAuthority, ContextRecordBody, ContextRecordRetention,
        };

        let turn = TurnId::now();
        let record = |key, revision, content| {
            Message::context_record(
                turn.clone(),
                ContextRecord::new(
                    key,
                    revision,
                    ContextRecordAuthority::Retrieved,
                    ContextRecordRetention::Latest,
                    ContextRecordBody::text(content),
                ),
            )
        };
        let mut arguments = args();
        arguments.context_latest = Some(vec!["source".into()]);
        let epoch = |messages: &[Message], base: &str| {
            let mut projection = messages.to_vec();
            let identity = project_context_records(&mut projection, &arguments);
            projected_context_epoch(identity.as_deref(), Some(base.into()))
        };
        let initial = vec![record("source", 1, "first")];
        let original_epoch = epoch(&initial, "checkpoint");
        let mut extended = initial.clone();
        extended.push(message("new question"));
        extended.push(record("other", 1, "new state"));
        assert_eq!(epoch(&extended, "checkpoint"), original_epoch);
        extended.push(record("source", 2, "replacement"));
        assert_ne!(epoch(&extended, "checkpoint"), original_epoch);
        assert_ne!(epoch(&initial, "other checkpoint"), original_epoch);
        let cleared = Message::context_record(
            turn.clone(),
            ContextRecord::new(
                "source",
                3,
                ContextRecordAuthority::Retrieved,
                ContextRecordRetention::Latest,
                ContextRecordBody::tombstone(),
            ),
        );
        assert_ne!(epoch(&[cleared], "checkpoint"), epoch(&[], "checkpoint"));
        for keys in [None, Some(vec![])] {
            arguments.context_latest = keys;
            let mut projection = initial.clone();
            let identity = project_context_records(&mut projection, &arguments);
            assert_eq!(
                projected_context_epoch(identity.as_deref(), Some("checkpoint".into())),
                Some("checkpoint".into())
            );
        }
        assert_eq!(projected_context_epoch(None, None), None);
    }

    #[test]
    fn root_context_uses_session_window_over_stale_handle() {
        let session = std::sync::Arc::new(crate::session::Session::open_ephemeral());
        session.append_message(message("canonical"), None);
        let stale = std::sync::Arc::new(std::sync::Mutex::new(vec![message("stale")]));
        let turn_id = TurnId::now();

        let context = build_llm_context(
            &args(),
            ContextMode::Session,
            Some(&session),
            Some(&stale),
            &turn_id,
            None,
            None,
        )
        .expect("context");

        assert_eq!(context.messages.len(), 1);
        assert_eq!(context.messages[0].text_concat(), "canonical");
    }

    #[test]
    fn root_recent_context_slices_session_window_without_stale_history() {
        let session = std::sync::Arc::new(crate::session::Session::open_ephemeral());
        session.append_message(message("first"), None);
        session.append_message(message("second"), None);
        let stale = std::sync::Arc::new(std::sync::Mutex::new(vec![
            message("stale-one"),
            message("stale-two"),
            message("stale-three"),
        ]));
        let turn_id = TurnId::now();

        let context = build_llm_context(
            &args(),
            ContextMode::SessionRecent(1),
            Some(&session),
            Some(&stale),
            &turn_id,
            None,
            None,
        )
        .expect("context");

        assert_eq!(context.messages.len(), 1);
        assert_eq!(context.messages[0].text_concat(), "second");
    }

    #[test]
    fn recent_context_keeps_current_turn_interjections_outside_the_recent_limit() {
        let turn_id = TurnId::now();
        for origin in [MessageOrigin::Interjection, MessageOrigin::Peer] {
            let mut interjection = Message::user_text(turn_id.clone(), "new direction");
            interjection.origin = origin;
            let messages = vec![interjection, message("later assistant context")];
            let mut projected =
                project_session_messages(&messages, ContextMode::SessionRecent(1), &turn_id);
            assert_eq!(projected.len(), 2);
            assert_eq!(projected[0].text_concat(), "new direction");
            move_current_turn_interjections_to_tail(&mut projected, &turn_id);
            assert_eq!(projected.last().unwrap().text_concat(), "new direction");
        }
    }

    #[test]
    fn answered_nudge_stays_in_history_instead_of_becoming_the_latest_prompt() {
        let turn_id = TurnId::now();
        let mut nudge = Message::user_text(turn_id.clone(), "old reminder");
        nudge.origin = MessageOrigin::Interjection;
        let reply = Message::assistant_text(turn_id.clone(), "handled reminder");
        let prompt = Message::user_text(turn_id.clone(), "continue work");
        let mut messages = vec![nudge, reply, prompt];

        move_current_turn_interjections_to_tail(&mut messages, &turn_id);

        assert_eq!(messages.last().unwrap().text_concat(), "continue work");
        let recent = project_session_messages(&messages, ContextMode::SessionRecent(1), &turn_id);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].text_concat(), "continue work");
    }

    #[test]
    fn claimed_user_input_follows_the_current_prompt() {
        let session = std::sync::Arc::new(crate::session::Session::open_ephemeral());
        let turn_id = TurnId::now();
        let mut input = Message::user_text(turn_id.clone(), "change direction");
        input.origin = MessageOrigin::Interjection;
        session.append_message(input, None);
        let mut request = args();
        request.prompt = Some("original task".into());

        let context = build_llm_context(
            &request,
            ContextMode::Session,
            Some(&session),
            None,
            &turn_id,
            None,
            None,
        )
        .unwrap();

        assert_eq!(context.messages[0].text_concat(), "original task");
        assert_eq!(context.messages[1].text_concat(), "change direction");
    }

    #[test]
    fn child_context_uses_local_handle_without_session() {
        let local = std::sync::Arc::new(std::sync::Mutex::new(vec![message("child")]));
        let turn_id = TurnId::now();

        let context = build_llm_context(
            &args(),
            ContextMode::Session,
            None,
            Some(&local),
            &turn_id,
            None,
            None,
        )
        .expect("context");

        assert_eq!(context.messages.len(), 1);
        assert_eq!(context.messages[0].text_concat(), "child");
    }

    #[test]
    fn recent_context_projects_only_the_latest_live_record() {
        let turn_id = TurnId::now();
        let record = |revision, body| {
            Message::context_record(
                turn_id.clone(),
                crate::context_plan::ContextRecord::new(
                    "session.goal",
                    revision,
                    crate::context_plan::ContextRecordAuthority::User,
                    crate::context_plan::ContextRecordRetention::Latest,
                    crate::context_plan::ContextRecordBody::text(body),
                ),
            )
        };
        let messages = vec![record(1, "old"), record(2, "current"), message("tail")];

        let projected =
            project_session_messages(&messages, ContextMode::SessionRecent(3), &turn_id);

        assert_eq!(projected.len(), 2);
        assert!(projected[0].text_concat().contains("current"));
        assert!(!projected[0].text_concat().contains("old"));
        assert_eq!(projected[1].text_concat(), "tail");
    }

    #[test]
    fn session_context_keeps_only_current_turn_internal_controls() {
        let previous_turn = TurnId::now();
        let current_turn = TurnId::now();
        let internal = |turn_id: TurnId, text: &str| Message {
            role: MessageRole::User,
            parts: vec![MessagePart::Text { text: text.into() }],
            turn_id,
            origin: MessageOrigin::Internal,
        };
        let messages = vec![
            internal(previous_turn, "stale loop control"),
            message("visible user message"),
            internal(current_turn.clone(), "current loop control"),
        ];

        let projected = project_session_messages(&messages, ContextMode::Session, &current_turn);

        assert_eq!(projected.len(), 2);
        assert_eq!(projected[0].text_concat(), "visible user message");
        assert_eq!(projected[1].text_concat(), "current loop control");
    }
}
