use crate::error::RuntimeError;
use crate::message::{Message, MessageRole};
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

const EPHEMERAL_CONTEXT_MESSAGE_LIMIT: usize = 100;

pub struct SessionPush;

impl Tool for SessionPush {
    fn name(&self) -> &str {
        "session.push"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Push a Message value into the current session's message history. \
             Use after dispatch_all to persist tool results so the next \
             llm.call(context: \"session\") call can see them. The message role \
             (user/assistant/tool/system) is preserved. Returns unit.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "object",
                    "description": "The Message value to push (e.g. a tool_result from dispatch_all). Pass the value returned by dispatch_all directly."
                }
            },
            "required": ["message"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let val = match args.named("message").or_else(|| args.positional(0).ok()) {
                Some(v) => v.clone(),
                None => {
                    return Err(RuntimeError::MissingArg("session.push: message".into()));
                }
            };
            let msgs = match val {
                Value::Message(m) => vec![m],
                Value::List(items) => items
                    .into_iter()
                    .filter_map(|v| match v {
                        Value::Message(m) => Some(m),
                        _ => None,
                    })
                    .collect(),
                other => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "message or list of message".into(),
                        actual: other.kind_name().into(),
                    });
                }
            };
            let Some(_handle) = &ctx.session_messages_handle else {
                return Err(RuntimeError::ToolFailed(
                    "session.push: no session messages handle available".into(),
                ));
            };
            let _compact_guard = match &ctx.compact_lock_handle {
                Some(lock) => Some(lock.lock().await),
                None => None,
            };
            for msg in msgs {
                let msg = crate::tools::tool_output::maybe_truncate_tool_message_with_budget(
                    &msg,
                    ctx.output_store.as_deref(),
                    ctx.tool_output_budget,
                );
                append_message_to_context(ctx, msg)?;
            }
            Ok(Value::Unit)
        })
    }
}

pub(crate) fn append_message_to_context(ctx: &ToolCtx, msg: Message) -> Result<(), RuntimeError> {
    let Some(handle) = &ctx.session_messages_handle else {
        return Err(RuntimeError::ToolFailed(
            "session message context is unavailable".into(),
        ));
    };
    emit_message_event(ctx, &msg);
    let flow_run_id = match msg.role {
        MessageRole::Assistant | MessageRole::Tool => {
            ctx.flow_run_id.as_ref().map(|run_id| run_id.0.to_string())
        }
        MessageRole::User => ctx.message_flow_run_id().map(|run_id| run_id.0.to_string()),
        MessageRole::System => None,
    };
    if let Some(tx) = &ctx.stream_tx {
        let _ = tx.send(crate::stream::StreamFrame::ToolResultMsg {
            flow_run_id,
            message: msg.clone(),
        });
    }
    let mut messages = handle.lock().unwrap();
    messages.push(msg);
    // Cap ephemeral sub-agent segments; root persists via event sink.
    if ctx.session_runtime.is_none() {
        trim_ephemeral_context(&mut messages, EPHEMERAL_CONTEXT_MESSAGE_LIMIT);
    }
    Ok(())
}

fn trim_ephemeral_context(messages: &mut Vec<Message>, limit: usize) {
    if messages.len() <= limit {
        return;
    }
    let mut start = messages.len() - limit;
    loop {
        let retained_result_ids: std::collections::HashSet<&str> = messages[start..]
            .iter()
            .flat_map(|message| {
                message.parts.iter().filter_map(|part| match part {
                    crate::message::MessagePart::ToolResult { tool_use_id, .. } => {
                        Some(tool_use_id.as_str())
                    }
                    _ => None,
                })
            })
            .collect();
        let Some(transaction_start) = messages[..start]
            .iter()
            .enumerate()
            .filter_map(|(index, message)| {
                message
                    .parts
                    .iter()
                    .any(|part| {
                        matches!(part, crate::message::MessagePart::ToolUse { id, .. }
                            if retained_result_ids.contains(id.as_str()))
                    })
                    .then_some(index)
            })
            .min()
        else {
            break;
        };
        start = transaction_start;
    }
    messages.drain(..start);
}

fn emit_message_event(ctx: &ToolCtx, msg: &Message) {
    use crate::event::{Event, TurnId};
    let Some(sink) = &ctx.events else {
        return;
    };
    let turn_id = ctx.turn_id.clone().unwrap_or_else(TurnId::now);
    let flow_run_id = ctx.message_flow_run_id();
    let event = match msg.role {
        MessageRole::User => Event::UserMsg {
            turn_id,
            flow_run_id,
            message: msg.clone(),
        },
        MessageRole::Assistant => Event::AssistantMsg {
            turn_id,
            flow_run_id,
            message: msg.clone(),
        },
        MessageRole::Tool => Event::ToolResultMsg {
            turn_id,
            flow_run_id,
            message: msg.clone(),
        },
        MessageRole::System => Event::SystemMsg {
            turn_id,
            message: msg.clone(),
        },
    };
    sink.emit(event);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message_with_parts(parts: Vec<crate::message::MessagePart>) -> Message {
        Message {
            role: MessageRole::Assistant,
            parts,
            turn_id: crate::event::TurnId::now(),
            origin: crate::message::MessageOrigin::User,
        }
    }

    #[test]
    fn ephemeral_context_trims_at_complete_tool_transaction_boundary() {
        use crate::message::MessagePart;

        let mut messages = vec![Message::user_text(
            crate::event::TurnId::now(),
            "drop this old message",
        )];
        messages.push(message_with_parts(vec![
            MessagePart::ToolUse {
                id: "call-a".into(),
                name: "fs.read".into(),
                input: serde_json::json!({}),
                intent: None,
            },
            MessagePart::ToolUse {
                id: "call-b".into(),
                name: "fs.read".into(),
                input: serde_json::json!({}),
                intent: None,
            },
        ]));
        for id in ["call-a", "call-b"] {
            messages.push(Message {
                role: MessageRole::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_use_id: id.into(),
                    content: "done".into(),
                    is_error: false,
                }],
                turn_id: crate::event::TurnId::now(),
                origin: crate::message::MessageOrigin::User,
            });
        }
        messages.extend((0..98).map(|index| {
            Message::assistant_text(crate::event::TurnId::now(), format!("tail-{index}"))
        }));

        trim_ephemeral_context(&mut messages, EPHEMERAL_CONTEXT_MESSAGE_LIMIT);

        assert_eq!(messages.len(), 101);
        assert!(messages.iter().any(|message| {
            message
                .parts
                .iter()
                .any(|part| matches!(part, MessagePart::ToolUse { id, .. } if id == "call-a"))
        }));
        assert!(
            !messages
                .iter()
                .any(|message| message.text_concat() == "drop this old message")
        );
    }

    #[test]
    fn ephemeral_context_keeps_active_unpaired_tool_use() {
        use crate::message::MessagePart;

        let mut messages = (0..100)
            .map(|index| {
                Message::assistant_text(crate::event::TurnId::now(), format!("old-{index}"))
            })
            .collect::<Vec<_>>();
        messages.push(message_with_parts(vec![MessagePart::ToolUse {
            id: "active".into(),
            name: "flow.spawn".into(),
            input: serde_json::json!({}),
            intent: None,
        }]));

        trim_ephemeral_context(&mut messages, EPHEMERAL_CONTEXT_MESSAGE_LIMIT);

        assert_eq!(messages.len(), EPHEMERAL_CONTEXT_MESSAGE_LIMIT);
        assert!(messages.iter().any(|message| {
            message
                .parts
                .iter()
                .any(|part| matches!(part, MessagePart::ToolUse { id, .. } if id == "active"))
        }));
    }

    #[test]
    fn session_push_name_and_tier() {
        let tool = SessionPush;
        assert_eq!(tool.name(), "session.push");
        assert_eq!(tool.tier(), Tier::Zero);
        assert!(tool.description().is_some());
    }

    #[tokio::test]
    async fn session_push_scopes_live_tool_result_without_scoping_session_history() {
        use crate::event::{Event, FlowRunId, TurnId};
        use crate::message::{MessageOrigin, MessagePart};

        let session = std::sync::Arc::new(crate::session::Session::open_ephemeral());
        let run_id = FlowRunId::now();
        let (stream_tx, mut stream_rx) = tokio::sync::broadcast::channel(8);
        let ctx = ToolCtx::new()
            .with_anchors(Some(TurnId::now()), Some(run_id.clone()), None)
            .with_events(session.sink().clone())
            .with_session_messages_handle(session.messages_handle())
            .with_session_runtime(session.clone())
            .with_stream_tx(stream_tx);
        let message = Message {
            role: MessageRole::Tool,
            parts: vec![MessagePart::ToolResult {
                tool_use_id: "tu_1".into(),
                content: "done".into(),
                is_error: false,
            }],
            turn_id: TurnId::now(),
            origin: MessageOrigin::User,
        };

        SessionPush
            .call(
                ToolArgs {
                    positional: vec![Value::Message(message)],
                    named: Vec::new(),
                },
                &ctx,
            )
            .await
            .unwrap();

        let crate::stream::StreamFrame::ToolResultMsg { flow_run_id, .. } =
            stream_rx.recv().await.unwrap()
        else {
            panic!("tool result frame");
        };
        assert_eq!(flow_run_id.as_deref(), Some(run_id.0.to_string().as_str()));
        assert!(session.sink().snapshot().iter().any(|event| {
            matches!(
                event,
                Event::ToolResultMsg {
                    flow_run_id: None,
                    ..
                }
            )
        }));
        assert!(
            session
                .messages()
                .iter()
                .any(|message| message.role == MessageRole::Tool)
        );
    }
}
