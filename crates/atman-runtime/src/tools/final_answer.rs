use crate::error::RuntimeError;
use crate::message::{Message, MessagePart};
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

pub const FINAL_ANSWER_TOOL: &str = "final.answer";

pub fn summary(message: &Message) -> Option<String> {
    message.parts.iter().find_map(|part| match part {
        MessagePart::FinalAnswerSummary { text } => Some(text.clone()),
        MessagePart::ToolUse { name, intent, .. } if name == FINAL_ANSWER_TOOL => {
            intent.as_ref().map(|intent| intent.as_str().to_owned())
        }
        _ => None,
    })
}

pub fn extract(message: &Message) -> Option<String> {
    if message.origin == crate::message::MessageOrigin::FinalAnswer {
        return Some(message.text_concat());
    }
    message.parts.iter().find_map(|part| match part {
        MessagePart::ToolUse { name, input, .. } if name == FINAL_ANSWER_TOOL => input
            .get("message")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        _ => None,
    })
}

pub fn normalized_for_history(message: &Message) -> Option<Message> {
    let answer = extract(message)?;
    let summary = summary(message)?;
    let mut normalized = message.clone();
    normalized.origin = crate::message::MessageOrigin::FinalAnswer;
    normalized
        .parts
        .retain(|part| matches!(part, MessagePart::Thinking { .. }));
    normalized
        .parts
        .push(MessagePart::FinalAnswerSummary { text: summary });
    normalized.parts.push(MessagePart::Text { text: answer });
    Some(normalized)
}

pub struct FinalAnswer;

impl Tool for FinalAnswer {
    fn name(&self) -> &str {
        FINAL_ANSWER_TOOL
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Deliver the final user-facing answer after all thinking and tool work is complete. Use _atman_intent to summarize the completed work for the collapsed activity header.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "Complete final answer in Markdown."
                }
            },
            "required": ["message"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            match args.named("message").or_else(|| args.positional.first()) {
                Some(Value::Str(message)) => Ok(Value::Str(message.clone())),
                Some(other) => Err(RuntimeError::TypeMismatch {
                    expected: "string".into(),
                    actual: other.kind_name().into(),
                }),
                None => Err(RuntimeError::ToolFailed(
                    "final.answer: missing `message`".into(),
                )),
            }
        })
    }
}

pub struct ExtractFinalAnswer;

impl Tool for ExtractFinalAnswer {
    fn name(&self) -> &str {
        "extract_final_answer"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some("Extract a final.answer control payload from an assistant Message.")
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"message": {"description": "Assistant Message value."}},
            "required": ["message"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let value = args.named("message").or_else(|| args.positional.first());
            match value {
                Some(Value::Message(message)) => {
                    Ok(extract(message).map(Value::Str).unwrap_or(Value::Unit))
                }
                Some(Value::Str(_)) | None => Ok(Value::Unit),
                Some(other) => Err(RuntimeError::TypeMismatch {
                    expected: "message or string".into(),
                    actual: other.kind_name().into(),
                }),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::TurnId;

    fn control_message() -> Message {
        Message {
            turn_id: TurnId::now(),
            role: crate::message::MessageRole::Assistant,
            parts: vec![MessagePart::ToolUse {
                id: "answer-1".into(),
                name: FINAL_ANSWER_TOOL.into(),
                input: serde_json::json!({"message": "Done."}),
                intent: None,
            }],
            origin: crate::message::MessageOrigin::User,
        }
    }

    #[test]
    fn final_answer_schema_requires_summary_intent() {
        let spec = crate::tool::tool_spec(&FinalAnswer);
        assert_eq!(
            spec.input_schema["required"],
            serde_json::json!(["message", "_atman_intent"])
        );
        assert!(
            spec.input_schema["properties"]
                .get("_atman_intent")
                .is_some()
        );
    }

    #[test]
    fn rejects_history_normalization_without_summary_intent() {
        assert!(normalized_for_history(&control_message()).is_none());
    }

    #[test]
    fn normalizes_valid_control_to_plain_assistant_text() {
        let mut message = control_message();
        let MessagePart::ToolUse { intent, .. } = &mut message.parts[0] else {
            unreachable!();
        };
        *intent = crate::message::ToolCallIntent::new("Completed requested work.");
        let normalized = normalized_for_history(&message).unwrap();
        assert_eq!(normalized.text_concat(), "Done.");
        assert!(!normalized.parts.iter().any(
            |part| matches!(part, MessagePart::ToolUse { name, .. } if name == FINAL_ANSWER_TOOL)
        ));
    }

    #[test]
    fn preserves_final_answer_summary_for_replay() {
        let mut message = control_message();
        let MessagePart::ToolUse { intent, .. } = &mut message.parts[0] else {
            unreachable!();
        };
        *intent = crate::message::ToolCallIntent::new("Checked the renderer and tests.");

        let normalized = normalized_for_history(&message).unwrap();
        assert_eq!(
            summary(&normalized).as_deref(),
            Some("Checked the renderer and tests.")
        );
        assert_eq!(normalized.text_concat(), "Done.");
    }
}
