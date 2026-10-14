use crate::error::RuntimeError;
use crate::message::{Message, MessagePart};
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::AtmanPayload;
use crate::value::Value;

pub const FINAL_ANSWER_TOOL: &str = "final.answer";

struct Candidate<'a> {
    answer: &'a str,
    summary: Option<&'a str>,
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
    if tool_calls != 1 {
        return Err("final.answer must be emitted without sibling tool calls");
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
    Ok(Some(Candidate {
        answer,
        summary: intent.as_ref().map(|intent| intent.as_str()),
    }))
}

fn fallback_summary(answer: &str) -> String {
    let first_line = answer
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("Completed internal work")
        .trim_start_matches(['#', '*', '-', '>', '`'])
        .trim();
    crate::message::ToolCallIntent::new(first_line)
        .or_else(|| crate::message::ToolCallIntent::new("Completed internal work"))
        .expect("fallback final-answer summary is non-empty")
        .as_str()
        .to_owned()
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
    message
        .parts
        .iter()
        .find_map(|part| match part {
            MessagePart::FinalAnswerSummary { text } => {
                (!text.trim().is_empty()).then(|| text.clone())
            }
            MessagePart::ToolUse {
                name,
                input,
                intent,
                ..
            } if name == FINAL_ANSWER_TOOL => intent
                .as_ref()
                .map(|intent| intent.as_str().to_owned())
                .or_else(|| {
                    input
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .filter(|answer| !answer.trim().is_empty())
                        .map(fallback_summary)
                }),
            _ => None,
        })
        .or_else(|| {
            (message.origin == crate::message::MessageOrigin::FinalAnswer)
                .then(|| message.text_concat())
                .filter(|answer| !answer.trim().is_empty())
                .map(|answer| fallback_summary(&answer))
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

fn promote_plain_response(message: &mut Message) {
    if message.role != crate::message::MessageRole::Assistant
        || message.origin == crate::message::MessageOrigin::FinalAnswer
        || message
            .parts
            .iter()
            .any(|part| matches!(part, MessagePart::ToolUse { .. }))
    {
        return;
    }
    let answer = message.text_concat();
    if answer.trim().is_empty() {
        return;
    }
    message.origin = crate::message::MessageOrigin::FinalAnswer;
    message.parts.push(MessagePart::FinalAnswerSummary {
        text: fallback_summary(&answer),
    });
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
        text: candidate
            .summary
            .map(str::to_owned)
            .unwrap_or_else(|| fallback_summary(candidate.answer)),
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

    fn scheduling_claims(
        &self,
        _args: &ToolArgs,
        ctx: &ToolCtx,
    ) -> Result<crate::tool_schedule::SchedulingClaims, RuntimeError> {
        Ok(super::session::session_scheduling_claim(ctx))
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let mut message = match args.named("message").or_else(|| args.positional.first()) {
                Some(Value::Str(text)) => Message::assistant_text(
                    ctx.turn_id
                        .clone()
                        .unwrap_or_else(crate::event::TurnId::now),
                    text.clone(),
                ),
                Some(Value::Host(AtmanPayload::Message(message))) => message.clone(),
                Some(other) => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "string or assistant message".into(),
                        actual: other.kind_name().into(),
                    });
                }
                None => {
                    return Err(RuntimeError::ToolFailed(
                        "final.answer: missing `message`".into(),
                    ));
                }
            };
            if message.role != crate::message::MessageRole::Assistant {
                return Err(RuntimeError::ToolFailed(
                    "final.answer requires an assistant message".into(),
                ));
            }
            if let Some(error) = validation_error(&message) {
                return Err(RuntimeError::ToolFailed(error.into()));
            }
            promote_plain_response(&mut message);
            let normalized = normalized_for_history(&message).ok_or_else(|| {
                RuntimeError::ToolFailed(
                    "final.answer requires non-empty text without other tool calls".into(),
                )
            })?;
            let answer = normalized.text_concat();
            let _compact_guard = match &ctx.compact_lock_handle {
                Some(lock) => Some(lock.lock().await),
                None => None,
            };
            super::session::append_message_to_context(ctx, normalized)?;
            Ok(Value::Str(answer))
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
                Some(Value::Host(AtmanPayload::Message(message))) => {
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
                Some(Value::Host(AtmanPayload::Message(message))) => message,
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
                .map(|message| Value::Host(AtmanPayload::Message(message)))
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
    fn plain_response_promotion_preserves_content_and_is_idempotent() {
        let mut message = Message::assistant_text(TurnId::now(), "\n# Done\n\n原文 **不变**。\n");
        message.parts.insert(
            0,
            MessagePart::Thinking {
                thinking: "Checked the result".into(),
                signature: Some("signature".into()),
            },
        );
        let original_parts = message.parts.clone();
        promote_plain_response(&mut message);
        assert_eq!(message.origin, crate::message::MessageOrigin::FinalAnswer);
        assert_eq!(&message.parts[..original_parts.len()], &original_parts);
        assert_eq!(
            extract(&message).as_deref(),
            Some("\n# Done\n\n原文 **不变**。\n")
        );
        assert_eq!(summary(&message).as_deref(), Some("Done"));
        let normalized = normalized_for_history(&message).unwrap();
        promote_plain_response(&mut message);
        assert_eq!(message, normalized);
    }

    #[test]
    fn plain_response_promotion_excludes_tool_calls_empty_text_and_user_messages() {
        let mut tool_reply = control_message();
        tool_reply.parts.push(MessagePart::Text {
            text: "commentary".into(),
        });
        let mut thinking_only = Message::assistant_text(TurnId::now(), " \n ");
        thinking_only.parts.push(MessagePart::Thinking {
            thinking: "Still thinking".into(),
            signature: None,
        });
        for mut message in [
            tool_reply,
            thinking_only,
            Message::user_text(TurnId::now(), "hello"),
        ] {
            let original = message.clone();
            promote_plain_response(&mut message);
            assert_eq!(message, original);
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
        assert_eq!(
            spec.input_schema["properties"]["_atman_intent"],
            serde_json::json!({
                "type": "string",
                "minLength": 1,
                "maxLength": 120,
                "pattern": "\\S"
            })
        );
    }

    #[tokio::test]
    async fn direct_final_answer_publishes_in_the_calling_context_and_rejects_invalid_input() {
        use crate::watch::{WatchResult, Watchable};
        use futures::{FutureExt, StreamExt};
        let entry = crate::tools::agent_ctrl::FlowRegistry::new().create_entry(
            "child".into(),
            String::new(),
            String::new(),
            crate::event::FlowRunId::now(),
        );
        let messages = entry.messages.clone();
        let parent = crate::goal::GoalController::ephemeral("parent".into());
        let mut ctx = ToolCtx::new()
            .with_history_segment(crate::tool::HistorySegment::Spawned)
            .with_session_messages_handle(messages.clone());
        ctx.agent_entry = Some(entry.clone());
        ctx.flow_run_id = Some(entry.child_run_id.clone());
        ctx.result_route = Some(std::sync::Arc::new(
            parent.child_results.route("child".into(), true),
        ));
        let mut direct = entry.clone().watch_output("Direct answer.".into());
        let mut control = entry.clone().watch_output("Done.".into());
        for value in [
            Value::Str("Direct answer.".into()),
            Value::Host(AtmanPayload::Message(control_message())),
        ] {
            let result = FinalAnswer
                .call(
                    ToolArgs {
                        positional: vec![value],
                        named: Vec::new(),
                    },
                    &ctx,
                )
                .await
                .unwrap();
            assert!(matches!(result, Value::Str(_)));
        }
        let published = messages.lock().unwrap().clone();
        assert_eq!(published.len(), 2);
        assert_eq!(published[0].text_concat(), "Direct answer.");
        assert_eq!(published[1].text_concat(), "Done.");
        assert!(
            published
                .iter()
                .all(|message| message.origin == crate::message::MessageOrigin::FinalAnswer)
        );
        for output in [&mut direct, &mut control] {
            assert!(matches!(
                output.next().now_or_never(),
                Some(Some(WatchResult::Matched { .. }))
            ));
            assert!(output.next().now_or_never().is_none());
        }
        let receipt = parent.child_results.reserve().unwrap();
        let batch: serde_json::Value =
            serde_json::from_str(receipt.prompt().split_once('\n').unwrap().1).unwrap();
        assert_eq!(batch.as_array().unwrap().len(), 2);
        assert_eq!(batch[0]["answer"], "Direct answer.");
        assert_eq!(batch[1]["answer"], "Done.");
        receipt.commit(|| Ok::<_, ()>(())).unwrap();
        for message in &published {
            ctx.publish_final_answer(message);
        }
        assert!(!parent.child_results.has_pending());
        assert!(direct.next().now_or_never().is_none());
        assert!(control.next().now_or_never().is_none());
        for value in [
            Value::Str(" \n ".into()),
            Value::Host(AtmanPayload::Message(Message::user_text(
                TurnId::now(),
                "user",
            ))),
        ] {
            assert!(
                FinalAnswer
                    .call(
                        ToolArgs {
                            positional: vec![value],
                            named: Vec::new()
                        },
                        &ctx
                    )
                    .await
                    .is_err()
            );
        }
        assert_eq!(*messages.lock().unwrap(), published);
        assert_eq!(entry.output(), "Direct answer.\nDone.");
        assert!(!parent.child_results.has_pending());
    }

    #[test]
    fn normalizes_missing_summary_intent_with_answer_fallback() {
        let normalized = normalized_for_history(&control_message()).unwrap();
        assert_eq!(normalized.text_concat(), "Done.");
        assert_eq!(summary(&normalized).as_deref(), Some("Done."));
        assert_eq!(
            normalized.origin,
            crate::message::MessageOrigin::FinalAnswer
        );
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
    fn accepts_final_control_with_text_but_rejects_other_tools() {
        let mut message = control_message();
        let MessagePart::ToolUse { intent, .. } = &mut message.parts[0] else {
            unreachable!();
        };
        *intent = crate::message::ToolCallIntent::new("Completed requested work.");
        message.parts.push(MessagePart::Text {
            text: "preface".into(),
        });
        assert_eq!(validation_error(&message), None);
        assert_eq!(extract(&message).as_deref(), Some("Done."));
        let normalized = normalized_for_history(&message).unwrap();
        assert_eq!(normalized.text_concat(), "Done.");

        message.parts.pop();
        message.parts.push(MessagePart::ToolUse {
            id: "read-1".into(),
            name: "fs.read".into(),
            input: serde_json::json!({"path": "README.md"}),
            intent: crate::message::ToolCallIntent::new("Read documentation."),
        });
        assert_eq!(
            validation_error(&message),
            Some("final.answer must be emitted without sibling tool calls")
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
