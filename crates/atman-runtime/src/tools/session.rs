use crate::error::RuntimeError;
use crate::message::{Message, MessageRole};
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;
use chrono::Utc;

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
             llm { context: session } call can see them. The message role \
             (user/assistant/tool/system) is preserved. Returns unit.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "message",
                    "description": "The Message value to push (e.g. a tool_result from dispatch_all)."
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
                Value::Str(s) => {
                    let turn_id = ctx
                        .turn_id
                        .clone()
                        .unwrap_or_else(crate::event::TurnId::now);
                    vec![Message::assistant_text(turn_id, s)]
                }
                other => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "message, list of message, or string".into(),
                        actual: other.kind_name().into(),
                    });
                }
            };
            let Some(handle) = &ctx.session_messages_handle else {
                return Err(RuntimeError::ToolFailed(
                    "session.push: no session messages handle available".into(),
                ));
            };
            for msg in msgs {
                emit_message_event(ctx, &msg);
                if let Some(tx) = &ctx.stream_tx {
                    let _ = tx.send(crate::stream::StreamFrame::ToolResultMsg {
                        flow_run_id: ctx.flow_run_id.as_ref().map(|r| r.0.to_string()),
                        message: msg.clone(),
                    });
                }
                handle.lock().unwrap().push(msg);
            }
            Ok(Value::Unit)
        })
    }
}

fn emit_message_event(ctx: &ToolCtx, msg: &Message) {
    use crate::event::{Event, TurnId};
    let Some(sink) = &ctx.events else {
        return;
    };
    let ts = Utc::now();
    let turn_id = ctx.turn_id.clone().unwrap_or_else(TurnId::now);
    let event = match msg.role {
        MessageRole::User => Event::UserMsg {
            seq: 0,
            turn_id,
            message: msg.clone(),
            ts,
        },
        MessageRole::Assistant => Event::AssistantMsg {
            seq: 0,
            turn_id,
            flow_run_id: ctx.flow_run_id.clone(),
            message: msg.clone(),
            ts,
        },
        MessageRole::Tool => Event::ToolResultMsg {
            seq: 0,
            turn_id,
            flow_run_id: ctx.flow_run_id.clone(),
            message: msg.clone(),
            ts,
        },
        MessageRole::System => Event::SystemMsg {
            seq: 0,
            turn_id,
            message: msg.clone(),
            ts,
        },
    };
    sink.emit(event);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_push_name_and_tier() {
        let tool = SessionPush;
        assert_eq!(tool.name(), "session.push");
        assert_eq!(tool.tier(), Tier::Zero);
        assert!(tool.description().is_some());
    }
}
