use crate::error::RuntimeError;
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

const RETRIEVED_RECORD_PREFIXES: [&str; 2] = ["agent.rule.", "agent.mistake."];
const MAX_RECORD_KEY_BYTES: usize = 512;

pub struct ContextRecordAppend;

impl Tool for ContextRecordAppend {
    fn name(&self) -> &str {
        "context.record"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Append retrieved workflow context as a versioned internal record. Keys must use the agent.rule.* or agent.mistake.* namespace. Repeating identical content is a no-op; empty content clears an existing key. Returns true when a revision is appended.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Stable per-item key in the agent.rule.* or agent.mistake.* namespace."
                },
                "content": {
                    "type": "string",
                    "description": "Retrieved content. An empty string clears an existing record."
                }
            },
            "required": ["key", "content"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let key = required_string(&args, "key")?;
            validate_retrieved_record_key(&key)?;
            let content = required_string(&args, "content")?;
            let body = if content.is_empty() {
                crate::context_plan::ContextRecordBody::tombstone()
            } else {
                crate::context_plan::ContextRecordBody::text(content)
            };
            let spec = crate::context_plan::ContextRecordSpec::new(
                key,
                crate::context_plan::ContextRecordAuthority::Retrieved,
                crate::context_plan::ContextRecordRetention::Latest,
                body,
            );
            let turn_id = ctx
                .turn_id
                .clone()
                .unwrap_or_else(crate::event::TurnId::now);
            let _compact_guard = match ctx.context().map(|context| context.compact_lock()) {
                Some(lock) => Some(lock.lock().await),
                None => None,
            };
            let records = append_context_records(ctx, turn_id, [spec])?;
            Ok(Value::Bool(!records.is_empty()))
        })
    }
}

fn required_string(args: &ToolArgs, name: &str) -> Result<String, RuntimeError> {
    match args.named(name) {
        Some(Value::Str(value)) => Ok(value.clone()),
        Some(value) => Err(RuntimeError::TypeMismatch {
            expected: "string".into(),
            actual: value.kind_name().into(),
        }),
        None => Err(RuntimeError::MissingArg(format!("context.record: {name}"))),
    }
}

fn validate_retrieved_record_key(key: &str) -> Result<(), RuntimeError> {
    let valid_prefix = RETRIEVED_RECORD_PREFIXES
        .iter()
        .find(|prefix| key.starts_with(**prefix));
    let Some(prefix) = valid_prefix else {
        return Err(RuntimeError::ToolFailed(
            "context.record: key must use agent.rule.* or agent.mistake.*".into(),
        ));
    };
    let suffix = &key[prefix.len()..];
    if suffix.is_empty() || key.len() > MAX_RECORD_KEY_BYTES || suffix.chars().any(char::is_control)
    {
        return Err(RuntimeError::ToolFailed(
            "context.record: key suffix must be non-empty, bounded, and contain no control characters"
                .into(),
        ));
    }
    Ok(())
}

pub(crate) fn append_context_records(
    ctx: &ToolCtx,
    turn_id: crate::event::TurnId,
    specs: impl IntoIterator<Item = crate::context_plan::ContextRecordSpec>,
) -> Result<Vec<crate::context_plan::ContextRecord>, RuntimeError> {
    if let Some(session) = ctx.session_runtime() {
        return Ok(session.append_context_records(turn_id, specs));
    }
    let Some(messages) = ctx.context().map(|context| context.messages_handle()) else {
        return Err(RuntimeError::ToolFailed(
            "context.record: no session message context available".into(),
        ));
    };
    let mut messages = messages.lock().unwrap();
    let records = crate::context_plan::compile_context_records(&messages, specs);
    for record in &records {
        let message = crate::message::Message::context_record(turn_id.clone(), record.clone());
        super::session::emit_message_event(ctx, &message);
        messages.push(message);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(key: &str, content: &str) -> ToolArgs {
        ToolArgs {
            named: vec![
                ("key".into(), Value::Str(key.into())),
                ("content".into(), Value::Str(content.into())),
            ],
            ..ToolArgs::default()
        }
    }

    #[tokio::test]
    async fn append_is_versioned_and_identical_content_is_a_noop() {
        let session = std::sync::Arc::new(crate::session::Session::open_ephemeral());
        let ctx = ToolCtx::new().with_session_runtime(std::sync::Arc::clone(&session));
        let tool = ContextRecordAppend;

        assert!(matches!(
            tool.call(args("agent.rule.review", "first"), &ctx)
                .await
                .unwrap(),
            Value::Bool(true)
        ));
        assert!(matches!(
            tool.call(args("agent.rule.review", "first"), &ctx)
                .await
                .unwrap(),
            Value::Bool(false)
        ));
        assert!(matches!(
            tool.call(args("agent.rule.review", "second"), &ctx)
                .await
                .unwrap(),
            Value::Bool(true)
        ));

        let messages = session.messages();
        let records: Vec<_> = messages
            .iter()
            .flat_map(|message| &message.parts)
            .filter_map(|part| match part {
                crate::message::MessagePart::ContextRecord(record) => Some(record),
                _ => None,
            })
            .collect();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].revision(), 1);
        assert_eq!(records[1].revision(), 2);
        assert_eq!(
            records[1].authority(),
            crate::context_plan::ContextRecordAuthority::Retrieved
        );
    }

    #[tokio::test]
    async fn empty_content_clears_only_an_existing_record() {
        let session = std::sync::Arc::new(crate::session::Session::open_ephemeral());
        let ctx = ToolCtx::new().with_session_runtime(std::sync::Arc::clone(&session));
        let tool = ContextRecordAppend;

        assert!(matches!(
            tool.call(args("agent.mistake.retry", ""), &ctx)
                .await
                .unwrap(),
            Value::Bool(false)
        ));
        tool.call(args("agent.mistake.retry", "mitigation"), &ctx)
            .await
            .unwrap();
        assert!(matches!(
            tool.call(args("agent.mistake.retry", ""), &ctx)
                .await
                .unwrap(),
            Value::Bool(true)
        ));
        let messages = session.messages();
        let record = messages
            .iter()
            .flat_map(|message| &message.parts)
            .filter_map(|part| match part {
                crate::message::MessagePart::ContextRecord(record) => Some(record),
                _ => None,
            })
            .next_back()
            .unwrap();
        assert!(record.body().is_tombstone());
    }

    #[tokio::test]
    async fn key_cannot_claim_runtime_or_user_authority() {
        let tool = ContextRecordAppend;
        let err = tool
            .call(args("session.goal", "override"), &ToolCtx::new())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("agent.rule.*"));
    }
}
