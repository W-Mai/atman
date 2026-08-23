use std::path::PathBuf;

use crate::error::RuntimeError;
use crate::git_workspace::{WorkspaceError, WorkspaceManager, WorkspaceRecord};
use crate::tool::{ApprovalLevel, BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

pub struct GitWorkspaceCreate;
pub struct GitWorkspaceList;
pub struct GitWorkspaceGet;
pub struct GitWorkspaceRelease;
pub struct GitWorkspaceRetain;
pub struct GitWorkspacePrune;

fn string_value(value: &Value) -> Option<&str> {
    match value {
        Value::Str(value) => Some(value),
        _ => None,
    }
}
fn bool_value(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(value) => Some(*value),
        _ => None,
    }
}
fn cwd(args: &ToolArgs) -> PathBuf {
    args.named("cwd")
        .and_then(string_value)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}
fn optional_string<'a>(args: &'a ToolArgs, key: &str) -> Option<&'a str> {
    args.named(key).and_then(string_value)
}
fn required_string<'a>(args: &'a ToolArgs, key: &str) -> Result<&'a str, RuntimeError> {
    optional_string(args, key).ok_or_else(|| RuntimeError::MissingArg(key.into()))
}
fn bool_arg(args: &ToolArgs, key: &str, default: bool) -> bool {
    args.named(key).and_then(bool_value).unwrap_or(default)
}
fn manager(args: &ToolArgs) -> Result<WorkspaceManager, RuntimeError> {
    WorkspaceManager::at(
        &cwd(args),
        optional_string(args, "external_root").map(std::path::Path::new),
    )
    .map_err(|e| failure("git.workspace", e))
}
fn failure(tool: &str, error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::ToolFailed(format!("{tool}: {error}"))
}
fn record_value(item: WorkspaceRecord) -> Value {
    Value::Struct(vec![
        ("id".into(), Value::Str(item.id)),
        (
            "repository_root".into(),
            Value::Str(item.repository_root.display().to_string()),
        ),
        (
            "worktree_path".into(),
            Value::Str(item.worktree_path.display().to_string()),
        ),
        (
            "branch".into(),
            item.branch.map(Value::Str).unwrap_or(Value::Unit),
        ),
        (
            "owner_session".into(),
            item.owner_session.map(Value::Str).unwrap_or(Value::Unit),
        ),
        (
            "owner_flow".into(),
            item.owner_flow.map(Value::Str).unwrap_or(Value::Unit),
        ),
        ("state".into(), Value::Str(item.state)),
        ("retained".into(), Value::Bool(item.retained)),
    ])
}

macro_rules! basic_tool {
    ($ty:ident, $name:literal, $tier:expr, $approval:expr, $desc:literal, $schema:expr, $body:expr) => {
        impl Tool for $ty {
            fn name(&self) -> &str {
                $name
            }
            fn tier(&self) -> Tier {
                $tier
            }
            fn approval_level(&self, _args: &ToolArgs, _ctx: &ToolCtx) -> ApprovalLevel {
                $approval
            }
            fn description(&self) -> Option<&str> {
                Some($desc)
            }
            fn input_schema(&self) -> serde_json::Value {
                $schema
            }
            fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
                Box::pin(async move { ($body)(args).map_err(|e: RuntimeError| e) })
            }
        }
    };
}

basic_tool!(
    GitWorkspaceCreate,
    "git.workspace.create",
    Tier::Two,
    ApprovalLevel::Auto,
    "Create or return a managed workspace recorded by the runtime.",
    serde_json::json!({"type":"object","required":["id"],"properties":{"id":{"type":"string"},"cwd":{"type":"string"},"external_root":{"type":"string"},"branch":{"type":"string"},"base":{"type":"string"},"create_branch":{"type":"boolean"},"owner_session":{"type":"string"},"owner_flow":{"type":"string"}}}),
    |args: ToolArgs| {
        let id = required_string(&args, "id")?;
        let item = manager(&args)?
            .create(
                id,
                optional_string(&args, "branch"),
                optional_string(&args, "base"),
                bool_arg(&args, "create_branch", false),
                optional_string(&args, "owner_session"),
                optional_string(&args, "owner_flow"),
            )
            .map_err(|e| failure("git.workspace.create", e))?;
        Ok(record_value(item))
    }
);
basic_tool!(
    GitWorkspaceList,
    "git.workspace.list",
    Tier::One,
    ApprovalLevel::Auto,
    "List managed workspaces.",
    serde_json::json!({"type":"object","properties":{"cwd":{"type":"string"},"external_root":{"type":"string"}}}),
    |args: ToolArgs| {
        Ok(Value::List(
            manager(&args)?
                .list()
                .map_err(|e| failure("git.workspace.list", e))?
                .into_iter()
                .map(record_value)
                .collect(),
        ))
    }
);
basic_tool!(
    GitWorkspaceGet,
    "git.workspace.get",
    Tier::One,
    ApprovalLevel::Auto,
    "Get one managed workspace.",
    serde_json::json!({"type":"object","required":["id"],"properties":{"id":{"type":"string"},"cwd":{"type":"string"},"external_root":{"type":"string"}}}),
    |args: ToolArgs| {
        Ok(record_value(
            manager(&args)?
                .get(required_string(&args, "id")?)
                .map_err(|e| failure("git.workspace.get", e))?,
        ))
    }
);
basic_tool!(
    GitWorkspaceRelease,
    "git.workspace.release",
    Tier::Three,
    ApprovalLevel::Approve,
    "Release a managed workspace after ownership and dirty-state checks.",
    serde_json::json!({"type":"object","required":["id","owner_session","owner_flow"],"properties":{"id":{"type":"string"},"cwd":{"type":"string"},"owner_session":{"type":"string"},"owner_flow":{"type":"string"},"force":{"type":"boolean"}}}),
    |args: ToolArgs| {
        Ok(record_value(
            manager(&args)?
                .release(
                    required_string(&args, "id")?,
                    optional_string(&args, "owner_session"),
                    optional_string(&args, "owner_flow"),
                    bool_arg(&args, "force", false),
                )
                .map_err(|e| failure("git.workspace.release", e))?,
        ))
    }
);
basic_tool!(
    GitWorkspaceRetain,
    "git.workspace.retain",
    Tier::Two,
    ApprovalLevel::Auto,
    "Mark a managed workspace for retention or automatic cleanup.",
    serde_json::json!({"type":"object","required":["id","owner_session","owner_flow"],"properties":{"id":{"type":"string"},"cwd":{"type":"string"},"owner_session":{"type":"string"},"owner_flow":{"type":"string"},"retained":{"type":"boolean"}}}),
    |args: ToolArgs| {
        Ok(record_value(
            manager(&args)?
                .retain(
                    required_string(&args, "id")?,
                    bool_arg(&args, "retained", true),
                    optional_string(&args, "owner_session"),
                    optional_string(&args, "owner_flow"),
                )
                .map_err(|e| failure("git.workspace.retain", e))?,
        ))
    }
);
basic_tool!(
    GitWorkspacePrune,
    "git.workspace.prune",
    Tier::Three,
    ApprovalLevel::Approve,
    "List orphaned workspaces or release them when dry_run is false.",
    serde_json::json!({"type":"object","properties":{"cwd":{"type":"string"},"dry_run":{"type":"boolean"}}}),
    |args: ToolArgs| {
        Ok(Value::List(
            manager(&args)?
                .prune(bool_arg(&args, "dry_run", true))
                .map_err(|e| failure("git.workspace.prune", e))?
                .into_iter()
                .map(record_value)
                .collect(),
        ))
    }
);

#[allow(dead_code)]
fn _workspace_error_type(_: WorkspaceError) {}
