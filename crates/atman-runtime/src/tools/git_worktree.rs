use std::path::PathBuf;

use crate::error::RuntimeError;
use crate::git::{GitCli, WorktreeInfo};
use crate::tool::{ApprovalLevel, BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

pub struct GitWorktreeAdd;
pub struct GitWorktreeList;
pub struct GitWorktreeRemove;
pub struct GitWorktreePrune;
pub struct GitWorktreeLock;
pub struct GitWorktreeUnlock;

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
        .unwrap_or_else(|| PathBuf::from("."))
}

fn string_arg<'a>(args: &'a ToolArgs, key: &str) -> Result<&'a str, RuntimeError> {
    args.named(key)
        .and_then(string_value)
        .ok_or_else(|| RuntimeError::MissingArg(key.into()))
}

fn bool_arg(args: &ToolArgs, key: &str, default: bool) -> bool {
    args.named(key).and_then(bool_value).unwrap_or(default)
}

fn failure(tool: &str, error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::ToolFailed(format!("{tool}: {error}"))
}

fn entry_value(entry: WorktreeInfo) -> Value {
    Value::Struct(vec![
        ("path".into(), Value::Str(entry.path.display().to_string())),
        (
            "head".into(),
            entry.head.map(Value::Str).unwrap_or(Value::Unit),
        ),
        (
            "branch".into(),
            entry.branch.map(Value::Str).unwrap_or(Value::Unit),
        ),
        ("detached".into(), Value::Bool(entry.detached)),
        ("bare".into(), Value::Bool(entry.bare)),
        (
            "locked".into(),
            entry.locked.map(Value::Str).unwrap_or(Value::Unit),
        ),
        (
            "prunable".into(),
            entry.prunable.map(Value::Str).unwrap_or(Value::Unit),
        ),
    ])
}

impl Tool for GitWorktreeList {
    fn name(&self) -> &str {
        "git.worktree.list"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some("List repository worktrees with attached, detached, locked, and prunable state.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"cwd":{"type":"string","description":"Optional repository working directory."}}})
    }
    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let entries = GitCli::at(cwd(&args))
                .worktree_list()
                .map_err(|e| failure(self.name(), e))?;
            Ok(Value::List(entries.into_iter().map(entry_value).collect()))
        })
    }
}

impl Tool for GitWorktreeAdd {
    fn name(&self) -> &str {
        "git.worktree.add"
    }
    fn tier(&self) -> Tier {
        Tier::Two
    }
    fn description(&self) -> Option<&str> {
        Some("Add a linked Git worktree after validating path and branch conflicts.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type":"object",
            "properties":{
                "path":{"type":"string"}, "branch":{"type":"string"}, "base":{"type":"string"},
                "create_branch":{"type":"boolean","default":false}, "detach":{"type":"boolean","default":false},
                "cwd":{"type":"string","description":"Optional repository working directory."}
            },
            "required":["path"]
        })
    }
    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let path = PathBuf::from(string_arg(&args, "path")?);
            let entry = GitCli::at(cwd(&args))
                .worktree_add(
                    &path,
                    args.named("branch").and_then(string_value),
                    args.named("base").and_then(string_value),
                    bool_arg(&args, "create_branch", false),
                    bool_arg(&args, "detach", false),
                )
                .map_err(|e| failure(self.name(), e))?;
            Ok(entry_value(entry))
        })
    }
}

impl Tool for GitWorktreeRemove {
    fn name(&self) -> &str {
        "git.worktree.remove"
    }
    fn tier(&self) -> Tier {
        Tier::Two
    }
    fn approval_level(&self, args: &ToolArgs, _ctx: &ToolCtx) -> ApprovalLevel {
        if bool_arg(args, "force", false) {
            ApprovalLevel::Dangerous
        } else {
            ApprovalLevel::Approve
        }
    }
    fn description(&self) -> Option<&str> {
        Some("Remove a registered linked worktree; dirty worktrees require force=true.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"path":{"type":"string"},"force":{"type":"boolean","default":false},"cwd":{"type":"string"}},"required":["path"]})
    }
    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let path = PathBuf::from(string_arg(&args, "path")?);
            GitCli::at(cwd(&args))
                .worktree_remove(&path, bool_arg(&args, "force", false))
                .map_err(|e| failure(self.name(), e))?;
            Ok(Value::Struct(vec![
                ("path".into(), Value::Str(path.display().to_string())),
                ("removed".into(), Value::Bool(true)),
            ]))
        })
    }
}

impl Tool for GitWorktreePrune {
    fn name(&self) -> &str {
        "git.worktree.prune"
    }
    fn tier(&self) -> Tier {
        Tier::Three
    }
    fn approval_level(&self, args: &ToolArgs, _ctx: &ToolCtx) -> ApprovalLevel {
        if bool_arg(args, "dry_run", true) {
            ApprovalLevel::Auto
        } else {
            ApprovalLevel::Dangerous
        }
    }
    fn description(&self) -> Option<&str> {
        Some("Inspect or prune stale worktree metadata; dry-run is the default.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"cwd":{"type":"string"},"dry_run":{"type":"boolean","default":true}}})
    }
    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let dry_run = bool_arg(&args, "dry_run", true);
            let output = GitCli::at(cwd(&args))
                .worktree_prune(dry_run)
                .map_err(|e| failure(self.name(), e))?;
            Ok(Value::Struct(vec![
                ("dry_run".into(), Value::Bool(dry_run)),
                ("output".into(), Value::Str(output)),
            ]))
        })
    }
}

impl Tool for GitWorktreeLock {
    fn name(&self) -> &str {
        "git.worktree.lock"
    }
    fn tier(&self) -> Tier {
        Tier::Two
    }
    fn description(&self) -> Option<&str> {
        Some("Lock a registered linked worktree, optionally recording a reason.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"path":{"type":"string"},"reason":{"type":"string"},"cwd":{"type":"string"}},"required":["path"]})
    }
    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let path = PathBuf::from(string_arg(&args, "path")?);
            GitCli::at(cwd(&args))
                .worktree_lock(&path, args.named("reason").and_then(string_value))
                .map_err(|e| failure(self.name(), e))?;
            Ok(Value::Struct(vec![
                ("path".into(), Value::Str(path.display().to_string())),
                ("locked".into(), Value::Bool(true)),
            ]))
        })
    }
}

impl Tool for GitWorktreeUnlock {
    fn name(&self) -> &str {
        "git.worktree.unlock"
    }
    fn tier(&self) -> Tier {
        Tier::Two
    }
    fn description(&self) -> Option<&str> {
        Some("Unlock a registered linked worktree.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"path":{"type":"string"},"cwd":{"type":"string"}},"required":["path"]})
    }
    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let path = PathBuf::from(string_arg(&args, "path")?);
            GitCli::at(cwd(&args))
                .worktree_unlock(&path)
                .map_err(|e| failure(self.name(), e))?;
            Ok(Value::Struct(vec![
                ("path".into(), Value::Str(path.display().to_string())),
                ("locked".into(), Value::Bool(false)),
            ]))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(key: &str, value: bool) -> ToolArgs {
        ToolArgs {
            named: vec![(key.into(), Value::Bool(value))],
            ..ToolArgs::default()
        }
    }

    #[test]
    fn remove_force_escalates_approval() {
        let tool = GitWorktreeRemove;
        let ctx = ToolCtx::default();
        assert_eq!(
            tool.approval_level(&ToolArgs::default(), &ctx),
            ApprovalLevel::Approve
        );
        assert_eq!(
            tool.approval_level(&args("force", true), &ctx),
            ApprovalLevel::Dangerous
        );
    }

    #[test]
    fn prune_defaults_to_dry_run_and_escalates_mutation() {
        let tool = GitWorktreePrune;
        let ctx = ToolCtx::default();
        assert_eq!(
            tool.approval_level(&ToolArgs::default(), &ctx),
            ApprovalLevel::Auto
        );
        assert_eq!(
            tool.approval_level(&args("dry_run", false), &ctx),
            ApprovalLevel::Dangerous
        );
    }
}
