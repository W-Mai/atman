use crate::error::RuntimeError;
use crate::message::{Message, MessageOrigin, MessagePart, MessageRole};
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

pub struct ImageRead;

impl Tool for ImageRead {
    fn name(&self) -> &str {
        "image.read"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Read a local PNG, JPEG, GIF, or WebP image and attach it as visual input for the next model call. The image must be at most 20 MiB.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Local image path. Relative paths resolve inside the active workspace."
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    fn invocation_provenance(
        &self,
        args: &ToolArgs,
        ctx: &ToolCtx,
    ) -> Result<crate::permission::ResourceProvenance, RuntimeError> {
        crate::permission::ResourceProvenance::for_ctx(ctx).with_path(ctx, &extract_path(args)?)
    }

    fn model_followups(&self, result: &Value, _ctx: &ToolCtx) -> Vec<Message> {
        match result {
            Value::Message(message)
                if message
                    .parts
                    .iter()
                    .any(|part| matches!(part, MessagePart::Image { .. })) =>
            {
                vec![message.clone()]
            }
            _ => Vec::new(),
        }
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let path = ctx.resolve_path(&extract_path(&args)?)?;
            let byte_len = tokio::fs::metadata(&path)
                .await
                .map_err(|error| {
                    RuntimeError::ToolFailed(format!("image.read({}): {error}", path.display()))
                })?
                .len();
            let source = match ctx.session_runtime.as_ref() {
                Some(session) => session.import_image_path(&path)?,
                None => crate::attachment_store::AttachmentStore::at(
                    ctx.session_dir
                        .as_deref()
                        .unwrap_or_else(|| std::path::Path::new("")),
                )
                .import_path(&path)?,
            };
            let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            ctx.note_read(&canonical);
            let label = format!(
                "Loaded image {} ({}; {} bytes) as visual input for the next model call.",
                crate::attachment_store::display_name(&source),
                source.media_type,
                byte_len
            );
            Ok(Value::Message(Message {
                role: MessageRole::User,
                parts: vec![
                    MessagePart::Text { text: label },
                    MessagePart::Image { source },
                ],
                turn_id: ctx
                    .turn_id
                    .clone()
                    .unwrap_or_else(crate::event::TurnId::now),
                origin: MessageOrigin::Internal,
            }))
        })
    }
}

fn extract_path(args: &ToolArgs) -> Result<std::path::PathBuf, RuntimeError> {
    let value = args.named("path").or_else(|| args.positional.first());
    match value {
        Some(Value::Str(path)) if !path.is_empty() => Ok(path.into()),
        Some(Value::Path(path)) => Ok(path.clone()),
        Some(other) => Err(RuntimeError::TypeMismatch {
            expected: "non-empty image path".into(),
            actual: other.kind_name().into(),
        }),
        None => Err(RuntimeError::MissingArg("image.read.path".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG_HEADER: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\r";

    #[tokio::test]
    async fn read_returns_internal_model_image_without_base64_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pixel.png");
        std::fs::write(&path, PNG_HEADER).unwrap();
        let reads = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        let ctx = ToolCtx::new().with_read_files(reads.clone());

        let value = ImageRead
            .call(
                ToolArgs {
                    positional: Vec::new(),
                    named: vec![("path".into(), Value::Str(path.display().to_string()))],
                },
                &ctx,
            )
            .await
            .unwrap();

        let Value::Message(message) = &value else {
            panic!("expected image message")
        };
        assert_eq!(message.role, MessageRole::User);
        assert_eq!(message.origin, MessageOrigin::Internal);
        assert!(
            matches!(message.parts.as_slice(), [MessagePart::Text { text }, MessagePart::Image { source }] if !text.contains("base64") && source.media_type == "image/png")
        );
        assert_eq!(
            ImageRead.model_followups(&value, &ctx),
            vec![message.clone()]
        );
        assert!(
            reads
                .lock()
                .unwrap()
                .contains(&std::fs::canonicalize(path).unwrap())
        );
    }

    #[tokio::test]
    async fn read_rejects_non_image_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, b"not an image").unwrap();
        let ctx = ToolCtx::new();

        let error = ImageRead
            .call(
                ToolArgs {
                    positional: vec![Value::Str(path.display().to_string())],
                    named: Vec::new(),
                },
                &ctx,
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("unsupported image format"));
    }
}
