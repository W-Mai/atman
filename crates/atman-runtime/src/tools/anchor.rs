use std::path::PathBuf;

use crate::error::RuntimeError;
use crate::tool::{ApprovalLevel, BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::tools::anchor_fs::{self, AnchorError, StateStore};
use crate::value::Value;

pub struct AnchorRead;
pub struct AnchorEdit;
pub struct AnchorWrite;
pub struct AnchorUndo;

fn text(args: &ToolArgs, name: &str, required: bool) -> Result<Option<String>, RuntimeError> {
    match args.named(name) {
        Some(Value::Str(value)) => Ok(Some(value.clone())),
        Some(Value::Unit) if !required => Ok(None),
        None if !required => Ok(None),
        Some(Value::Unit) | None => Err(RuntimeError::MissingArg(name.into())),
        Some(value) => Err(RuntimeError::TypeMismatch {
            expected: "string".into(),
            actual: value.kind_name().into(),
        }),
    }
}

fn path(args: &ToolArgs) -> Result<PathBuf, RuntimeError> {
    match args.named("path").or_else(|| args.positional.first()) {
        Some(Value::Path(path)) => Ok(path.clone()),
        Some(Value::Str(path)) => Ok(PathBuf::from(path)),
        Some(value) => Err(RuntimeError::TypeMismatch {
            expected: "path or string".into(),
            actual: value.kind_name().into(),
        }),
        None => Err(RuntimeError::MissingArg("path".into())),
    }
}

fn store(ctx: &ToolCtx) -> StateStore {
    StateStore::new(
        ctx.data_root
            .clone()
            .unwrap_or_else(|| PathBuf::from(".atman"))
            .join("anchor"),
    )
}

fn error(error: AnchorError) -> RuntimeError {
    RuntimeError::ToolFailed(error.to_string())
}

fn change_value(change: &anchor_fs::ChangeRecord) -> Value {
    Value::Struct(vec![
        ("change_id".into(), Value::Str(change.change_id.clone())),
        ("path".into(), Value::Str(change.path.clone())),
        ("before_hash".into(), Value::Str(change.before_hash.clone())),
        ("after_hash".into(), Value::Str(change.after_hash.clone())),
    ])
}

fn schema(properties: serde_json::Value, required: &[&str]) -> serde_json::Value {
    serde_json::json!({"type":"object", "properties":properties, "required":required})
}

impl Tool for AnchorRead {
    fn name(&self) -> &str {
        "anchor.read"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some("Read a file with stable hashline anchors.")
    }
    fn input_schema(&self) -> serde_json::Value {
        schema(
            serde_json::json!({"path":{"type":["string","object"]}}),
            &["path"],
        )
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let p = ctx.resolve_path(&path(&args)?)?;
            anchor_fs::read_anchor_text(&p, &store(ctx))
                .map(Value::Str)
                .map_err(error)
        })
    }
}

impl Tool for AnchorEdit {
    fn name(&self) -> &str {
        "anchor.edit"
    }
    fn tier(&self) -> Tier {
        Tier::Two
    }
    fn approval_level(&self, _: &ToolArgs, _: &ToolCtx) -> ApprovalLevel {
        ApprovalLevel::Approve
    }
    fn description(&self) -> Option<&str> {
        Some("Apply a strict replace, insert, or remove mutation using hashline anchors.")
    }
    fn input_schema(&self) -> serde_json::Value {
        schema(
            serde_json::json!({"path":{"type":"string"},"operation":{"enum":["replace","insert","remove"]},"target":{"type":"string"},"from":{"type":"string"},"to":{"type":"string"},"at":{"type":"string"},"position":{"enum":["before","after"]},"content":{"type":"string"}}),
            &["path", "operation"],
        )
    }
    fn invocation_provenance(
        &self,
        args: &ToolArgs,
        ctx: &ToolCtx,
    ) -> Result<crate::permission::ResourceProvenance, RuntimeError> {
        Ok(crate::permission::ResourceProvenance::for_ctx(ctx)
            .with_path(ctx, &path(args)?)?
            .with_risk(crate::trust::RiskKind::FilesystemWrite))
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let p = ctx.resolve_path_with_origin(&path(&args)?)?.path;
            crate::fs_access::authorize_write(ctx, &p, self.name(), true).await?;
            let operation = text(&args, "operation", true)?.unwrap();
            let change = anchor_fs::edit_by_anchor(
                &p,
                &operation,
                text(&args, "target", false)?.as_deref(),
                text(&args, "from", false)?.as_deref(),
                text(&args, "to", false)?.as_deref(),
                text(&args, "at", false)?.as_deref(),
                text(&args, "position", false)?.as_deref(),
                text(&args, "content", false)?.as_deref(),
                &store(ctx),
            )
            .map_err(error)?;
            Ok(change_value(&change))
        })
    }
}

impl Tool for AnchorWrite {
    fn name(&self) -> &str {
        "anchor.write"
    }
    fn tier(&self) -> Tier {
        Tier::Two
    }
    fn approval_level(&self, _: &ToolArgs, _: &ToolCtx) -> ApprovalLevel {
        ApprovalLevel::Approve
    }
    fn description(&self) -> Option<&str> {
        Some("Overwrite a file only when its expected hash matches.")
    }
    fn input_schema(&self) -> serde_json::Value {
        schema(
            serde_json::json!({"path":{"type":"string"},"expected_file_hash":{"type":"string"},"content":{"type":"string"}}),
            &["path", "expected_file_hash", "content"],
        )
    }
    fn invocation_provenance(
        &self,
        args: &ToolArgs,
        ctx: &ToolCtx,
    ) -> Result<crate::permission::ResourceProvenance, RuntimeError> {
        Ok(crate::permission::ResourceProvenance::for_ctx(ctx)
            .with_path(ctx, &path(args)?)?
            .with_risk(crate::trust::RiskKind::FilesystemWrite))
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let p = ctx.resolve_path_with_origin(&path(&args)?)?.path;
            crate::fs_access::authorize_write(ctx, &p, self.name(), true).await?;
            let h = text(&args, "expected_file_hash", true)?.unwrap();
            let c = text(&args, "content", true)?.unwrap();
            anchor_fs::overwrite_with_hash(&p, &h, &c, &store(ctx))
                .map(|v| change_value(&v))
                .map_err(error)
        })
    }
}

impl Tool for AnchorUndo {
    fn name(&self) -> &str {
        "anchor.undo"
    }
    fn tier(&self) -> Tier {
        Tier::Two
    }
    fn approval_level(&self, _: &ToolArgs, _: &ToolCtx) -> ApprovalLevel {
        ApprovalLevel::Approve
    }
    fn description(&self) -> Option<&str> {
        Some("Undo the latest anchor change, refusing if the file changed since.")
    }
    fn input_schema(&self) -> serde_json::Value {
        schema(
            serde_json::json!({"path":{"type":"string"},"change_id":{"type":"string"}}),
            &["path"],
        )
    }
    fn invocation_provenance(
        &self,
        args: &ToolArgs,
        ctx: &ToolCtx,
    ) -> Result<crate::permission::ResourceProvenance, RuntimeError> {
        Ok(crate::permission::ResourceProvenance::for_ctx(ctx)
            .with_path(ctx, &path(args)?)?
            .with_risk(crate::trust::RiskKind::FilesystemWrite))
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let p = ctx.resolve_path_with_origin(&path(&args)?)?.path;
            crate::fs_access::authorize_write(ctx, &p, self.name(), true).await?;
            let s = store(ctx);
            let id = text(&args, "change_id", false)?;
            let change = match id {
                Some(id) => s.change(&id),
                None => s.latest_change(&p),
            }
            .map_err(error)?
            .ok_or_else(|| {
                RuntimeError::ToolFailed(format!("no anchor change for {}", p.display()))
            })?;
            let current = std::fs::read(&p).map_err(|e| RuntimeError::ToolFailed(e.to_string()))?;
            s.undo_strict(&p, &change, &current).map_err(error)?;
            Ok(change_value(&change))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn anchor_write_rejects_external_path_without_changing_file() {
        let workspace = tempfile::tempdir().unwrap();
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("r4-anchor-{}.txt", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(fixture.parent().unwrap()).unwrap();
        std::fs::write(&fixture, "original").unwrap();
        let ctx = ToolCtx::new()
            .with_fs_access(crate::fs_access::FsAccessPolicy::workspace_write(
                workspace.path().into(),
            ))
            .with_workspace(crate::git_workspace::WorkspaceBinding {
                workspace_id: "test".into(),
                repository_root: workspace.path().into(),
                path: workspace.path().into(),
                branch: None,
            });
        let error = AnchorWrite
            .call(
                ToolArgs {
                    positional: vec![],
                    named: vec![
                        ("path".into(), Value::Path(fixture.clone())),
                        ("expected_file_hash".into(), Value::Str("unused".into())),
                        ("content".into(), Value::Str("changed".into())),
                    ],
                },
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("outside workspace"));
        assert_eq!(std::fs::read_to_string(&fixture).unwrap(), "original");
        std::fs::remove_file(fixture).unwrap();
    }
}
