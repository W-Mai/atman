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

fn cwd(args: &ToolArgs, ctx: &ToolCtx) -> Result<PathBuf, RuntimeError> {
    let explicit = args.named("cwd").and_then(|value| match value {
        Value::Str(path) => Some(std::path::Path::new(path)),
        _ => None,
    });
    ctx.resolve_cwd(explicit)
}

async fn cwd_mut(args: &ToolArgs, ctx: &ToolCtx, tool: &str) -> Result<PathBuf, RuntimeError> {
    let cwd = cwd(args, ctx)?;
    crate::fs_access::authorize_write(ctx, &cwd, tool, true).await?;
    Ok(cwd)
}

fn string_arg<'a>(args: &'a ToolArgs, key: &str) -> Result<&'a str, RuntimeError> {
    args.named(key)
        .and_then(string_value)
        .ok_or_else(|| RuntimeError::MissingArg(key.into()))
}

fn worktree_path(args: &ToolArgs, ctx: &ToolCtx) -> Result<PathBuf, RuntimeError> {
    ctx.resolve_path(std::path::Path::new(string_arg(args, "path")?))
}

async fn mutation_paths(
    args: &ToolArgs,
    ctx: &ToolCtx,
    tool: &str,
) -> Result<(PathBuf, PathBuf), RuntimeError> {
    let cwd = cwd_mut(args, ctx, tool).await?;
    let path = worktree_path(args, ctx)?;
    crate::fs_access::authorize_write(ctx, &path, tool, true).await?;
    Ok((cwd, path))
}

fn registered_worktree_path(cwd: &std::path::Path, requested: &std::path::Path) -> PathBuf {
    GitCli::at(cwd)
        .worktree_list()
        .ok()
        .and_then(|entries| {
            entries.into_iter().find_map(|entry| {
                (crate::fs_access::canonicalize_stable(&entry.path) == requested)
                    .then_some(entry.path)
            })
        })
        .unwrap_or_else(|| requested.to_path_buf())
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
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let entries = GitCli::at(cwd(&args, ctx)?)
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
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let (cwd, path) = mutation_paths(&args, ctx, self.name()).await?;
            let entry = GitCli::at(cwd)
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
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let cwd = cwd_mut(&args, ctx, self.name()).await?;
            let requested = worktree_path(&args, ctx)?;
            let path = registered_worktree_path(&cwd, &requested);
            crate::fs_access::authorize_write(ctx, &path, self.name(), true).await?;
            GitCli::at(cwd)
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
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let dry_run = bool_arg(&args, "dry_run", true);
            let cwd = if dry_run {
                cwd(&args, ctx)?
            } else {
                cwd_mut(&args, ctx, self.name()).await?
            };
            let output = GitCli::at(cwd)
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
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let (cwd, path) = mutation_paths(&args, ctx, self.name()).await?;
            GitCli::at(cwd)
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
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let (cwd, path) = mutation_paths(&args, ctx, self.name()).await?;
            GitCli::at(cwd)
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
    use std::path::{Path, PathBuf};
    use std::process::Command;

    struct ExternalPath(PathBuf);

    impl ExternalPath {
        fn new(label: &str) -> Self {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .join(format!("atman-r4-{label}-{}", uuid::Uuid::now_v7()));
            assert!(!path.exists());
            Self(path)
        }
    }

    impl Drop for ExternalPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn git(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn init_repo() -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q"]);
        git(
            repo.path(),
            &["config", "user.email", "atman@example.invalid"],
        );
        git(repo.path(), &["config", "user.name", "Atman Test"]);
        std::fs::write(repo.path().join("README.md"), "seed\n").unwrap();
        git(repo.path(), &["add", "README.md"]);
        git(repo.path(), &["commit", "-qm", "seed"]);
        repo
    }

    fn managed_ctx(repo: &Path) -> ToolCtx {
        ToolCtx::new()
            .with_fs_access(crate::fs_access::FsAccessPolicy::workspace_write(
                repo.to_path_buf(),
            ))
            .with_workspace(crate::git_workspace::WorkspaceBinding {
                workspace_id: "test".into(),
                repository_root: repo.to_path_buf(),
                path: repo.to_path_buf(),
                branch: None,
            })
    }

    fn worktree_args(path: impl Into<String>) -> ToolArgs {
        ToolArgs {
            named: vec![
                ("path".into(), Value::Str(path.into())),
                ("detach".into(), Value::Bool(true)),
            ],
            ..ToolArgs::default()
        }
    }

    fn registered_paths(repo: &Path) -> Vec<PathBuf> {
        GitCli::at(repo)
            .worktree_list()
            .unwrap()
            .into_iter()
            .map(|entry| crate::fs_access::canonicalize_stable(&entry.path))
            .collect()
    }

    fn refs(repo: &Path) -> String {
        git(repo, &["show-ref"])
    }

    fn args(key: &str, value: bool) -> ToolArgs {
        ToolArgs {
            named: vec![(key.into(), Value::Bool(value))],
            ..ToolArgs::default()
        }
    }

    #[test]
    fn worktree_path_resolves_relative_to_context_cwd() {
        let workspace = tempfile::tempdir().unwrap();
        let ctx = ToolCtx::new().with_workspace(crate::git_workspace::WorkspaceBinding {
            workspace_id: "test".into(),
            repository_root: workspace.path().to_path_buf(),
            path: workspace.path().to_path_buf(),
            branch: None,
        });
        let args = ToolArgs {
            named: vec![("path".into(), Value::Str("linked".into()))],
            ..ToolArgs::default()
        };

        assert_eq!(
            worktree_path(&args, &ctx).unwrap(),
            workspace.path().canonicalize().unwrap().join("linked")
        );
    }

    #[tokio::test]
    async fn add_relative_path_creates_worktree_under_managed_root() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "atman@example.invalid"]);
        git(&repo, &["config", "user.name", "Atman Test"]);
        std::fs::write(repo.join("README.md"), "seed\n").unwrap();
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-qm", "seed"]);
        let ctx = managed_ctx(root.path());
        let mut args = worktree_args("linked");
        args.named.push(("cwd".into(), Value::Str("repo".into())));

        GitWorktreeAdd.call(args, &ctx).await.unwrap();

        let expected = root.path().join("linked").canonicalize().unwrap();
        assert!(expected.exists());
        assert!(registered_paths(&repo).contains(&expected));
        GitCli::at(&repo).worktree_remove(&expected, true).unwrap();
    }

    #[tokio::test]
    async fn add_external_path_rejection_preserves_directory_registration_and_refs() {
        let repo = init_repo();
        let target = ExternalPath::new("add-deny");
        let ctx = managed_ctx(repo.path());
        let registrations_before = registered_paths(repo.path());
        let refs_before = refs(repo.path());

        let error = GitWorktreeAdd
            .call(worktree_args(target.0.to_string_lossy().into_owned()), &ctx)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("outside workspace"));
        assert!(!target.0.exists());
        assert_eq!(registered_paths(repo.path()), registrations_before);
        assert_eq!(refs(repo.path()), refs_before);
    }

    #[tokio::test]
    async fn add_external_temp_path_is_allowed() {
        let repo = init_repo();
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("linked");
        let ctx = managed_ctx(repo.path());

        GitWorktreeAdd
            .call(worktree_args(target.to_string_lossy().into_owned()), &ctx)
            .await
            .unwrap();

        let expected = target.canonicalize().unwrap();
        assert!(registered_paths(repo.path()).contains(&expected));
        GitCli::at(repo.path())
            .worktree_remove(&expected, true)
            .unwrap();
    }

    #[tokio::test]
    async fn add_external_path_is_allowed_with_full_access() {
        let repo = init_repo();
        let target = ExternalPath::new("add-full");
        let ctx = managed_ctx(repo.path())
            .with_fs_access(crate::fs_access::FsAccessPolicy::danger_full_access());

        GitWorktreeAdd
            .call(worktree_args(target.0.to_string_lossy().into_owned()), &ctx)
            .await
            .unwrap();

        let expected = target.0.canonicalize().unwrap();
        assert!(registered_paths(repo.path()).contains(&expected));
        GitCli::at(repo.path())
            .worktree_remove(&expected, true)
            .unwrap();
    }

    #[tokio::test]
    async fn remove_external_rejection_preserves_directory_and_registration() {
        let repo = init_repo();
        let target = ExternalPath::new("remove-deny");
        GitCli::at(repo.path())
            .worktree_add(&target.0, None, None, false, true)
            .unwrap();
        let canonical = target.0.canonicalize().unwrap();
        let registrations_before = registered_paths(repo.path());
        let ctx = managed_ctx(repo.path());
        let args = ToolArgs {
            named: vec![(
                "path".into(),
                Value::Str(target.0.to_string_lossy().into_owned()),
            )],
            ..ToolArgs::default()
        };

        let error = GitWorktreeRemove.call(args, &ctx).await.unwrap_err();

        assert!(error.to_string().contains("outside workspace"));
        assert!(canonical.exists());
        assert_eq!(registered_paths(repo.path()), registrations_before);
        GitCli::at(repo.path())
            .worktree_remove(&canonical, true)
            .unwrap();
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
