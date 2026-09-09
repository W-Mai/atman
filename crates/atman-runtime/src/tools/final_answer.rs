use crate::error::RuntimeError;
use crate::message::{Message, MessagePart};
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

pub const FINAL_ANSWER_TOOL: &str = "final.answer";

struct Candidate<'a> {
    answer: &'a str,
    summary: &'a str,
}

fn raw_candidate(message: &Message) -> Result<Option<Candidate<'_>>, &'static str> {
    let final_calls = message
        .parts
        .iter()
        .filter(
            |part| matches!(part, MessagePart::ToolUse { name, .. } if name == FINAL_ANSWER_TOOL),
        )
        .count();
    if final_calls == 0 {
        return Ok(None);
    }
    if final_calls != 1 {
        return Err("final.answer must be the only tool call in the assistant response");
    }
    let tool_calls = message
        .parts
        .iter()
        .filter(|part| matches!(part, MessagePart::ToolUse { .. }))
        .count();
    let has_text = message
        .parts
        .iter()
        .any(|part| matches!(part, MessagePart::Text { text } if !text.trim().is_empty()));
    if tool_calls != 1 || has_text {
        return Err(
            "final.answer must be emitted alone, without sibling tool calls or assistant text",
        );
    }
    let Some(MessagePart::ToolUse { input, intent, .. }) = message.parts.iter().find(
        |part| matches!(part, MessagePart::ToolUse { name, .. } if name == FINAL_ANSWER_TOOL),
    ) else {
        unreachable!();
    };
    let Some(answer) = input
        .get("message")
        .and_then(serde_json::Value::as_str)
        .filter(|answer| !answer.trim().is_empty())
    else {
        return Err("final.answer requires a non-empty `message`");
    };
    let Some(summary) = intent.as_ref().map(|intent| intent.as_str()) else {
        return Err("final.answer requires a non-empty `_atman_intent`");
    };
    Ok(Some(Candidate { answer, summary }))
}

pub fn attempted(message: &Message) -> bool {
    message
        .parts
        .iter()
        .any(|part| matches!(part, MessagePart::ToolUse { name, .. } if name == FINAL_ANSWER_TOOL))
}

pub fn validation_error(message: &Message) -> Option<&'static str> {
    raw_candidate(message).err()
}

pub fn summary(message: &Message) -> Option<String> {
    message.parts.iter().find_map(|part| match part {
        MessagePart::FinalAnswerSummary { text } => (!text.trim().is_empty()).then(|| text.clone()),
        MessagePart::ToolUse { name, intent, .. } if name == FINAL_ANSWER_TOOL => {
            intent.as_ref().map(|intent| intent.as_str().to_owned())
        }
        _ => None,
    })
}

pub fn extract(message: &Message) -> Option<String> {
    if message.origin == crate::message::MessageOrigin::FinalAnswer {
        return (!message.text_concat().trim().is_empty()).then(|| message.text_concat());
    }
    raw_candidate(message)
        .ok()
        .flatten()
        .map(|candidate| candidate.answer.to_owned())
}

pub fn normalized_for_history(message: &Message) -> Option<Message> {
    if message.origin == crate::message::MessageOrigin::FinalAnswer {
        return extract(message)
            .and_then(|_| summary(message))
            .map(|_| message.clone());
    }
    let candidate = raw_candidate(message).ok().flatten()?;
    let mut normalized = message.clone();
    normalized.origin = crate::message::MessageOrigin::FinalAnswer;
    normalized
        .parts
        .retain(|part| matches!(part, MessagePart::Thinking { .. }));
    normalized.parts.push(MessagePart::FinalAnswerSummary {
        text: candidate.summary.to_owned(),
    });
    normalized.parts.push(MessagePart::Text {
        text: candidate.answer.to_owned(),
    });
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

pub struct FinalizeResponse;

impl Tool for FinalizeResponse {
    fn name(&self) -> &str {
        "finalize_response"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn requires_call_intent(&self) -> bool {
        false
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let message = match args.named("message").or_else(|| args.positional.first()) {
                Some(Value::Message(message)) => message,
                Some(other) => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "message".into(),
                        actual: other.kind_name().into(),
                    });
                }
                None => {
                    return Err(RuntimeError::MissingArg(
                        "finalize_response: message".into(),
                    ));
                }
            };
            normalized_for_history(message)
                .map(Value::Message)
                .ok_or_else(|| {
                    RuntimeError::ToolFailed(
                        validation_error(message)
                            .unwrap_or("invalid final.answer control")
                            .into(),
                    )
                })
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

    #[test]
    fn rejects_final_control_mixed_with_text_or_other_tools() {
        let mut message = control_message();
        let MessagePart::ToolUse { intent, .. } = &mut message.parts[0] else {
            unreachable!();
        };
        *intent = crate::message::ToolCallIntent::new("Completed requested work.");
        message.parts.push(MessagePart::Text {
            text: "preface".into(),
        });
        assert_eq!(
            validation_error(&message),
            Some(
                "final.answer must be emitted alone, without sibling tool calls or assistant text"
            )
        );

        message.parts.pop();
        message.parts.push(MessagePart::ToolUse {
            id: "read-1".into(),
            name: "fs.read".into(),
            input: serde_json::json!({"path": "README.md"}),
            intent: crate::message::ToolCallIntent::new("Read documentation."),
        });
        assert_eq!(
            validation_error(&message),
            Some(
                "final.answer must be emitted alone, without sibling tool calls or assistant text"
            )
        );
    }

    #[test]
    fn rejects_empty_or_repeated_final_controls() {
        let mut message = control_message();
        let MessagePart::ToolUse { input, intent, .. } = &mut message.parts[0] else {
            unreachable!();
        };
        *input = serde_json::json!({"message": "  "});
        *intent = crate::message::ToolCallIntent::new("Completed requested work.");
        assert_eq!(
            validation_error(&message),
            Some("final.answer requires a non-empty `message`")
        );

        message.parts.push(message.parts[0].clone());
        assert_eq!(
            validation_error(&message),
            Some("final.answer must be the only tool call in the assistant response")
        );
    }
}
