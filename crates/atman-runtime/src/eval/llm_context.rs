use crate::error::RuntimeError;
use crate::eval::ContextMode;
use crate::eval::llm_args::LlmNodeArgs;
use crate::value::Value;

pub struct LlmContext {
    pub messages: Vec<crate::message::Message>,
    pub budget_text: String,
    pub session_messages_len: usize,
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
    let session_messages_len = final_messages.len();

    Ok(LlmContext {
        messages: final_messages,
        budget_text: prompt_for_budget,
        session_messages_len,
    })
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
            let ordinary: Vec<_> = messages
                .iter()
                .filter(visible)
                .filter_map(|message| {
                    let mut message = message.clone();
                    message.parts.retain(|part| {
                        !matches!(part, crate::message::MessagePart::ContextRecord(_))
                    });
                    (!message.parts.is_empty()).then_some(message)
                })
                .collect();
            let start = ordinary.len().saturating_sub(n);
            projected.extend_from_slice(&ordinary[start..]);
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
            fallback_value: None,
            tool_specs: Vec::new(),
            reasoning: None,
            call_purpose: crate::context_plan::ContextCallPurpose::General,
            stall_timeout_secs: 0,
        }
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
