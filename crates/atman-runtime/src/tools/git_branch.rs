use std::path::PathBuf;

use git2::{BranchType, Repository};

use crate::error::RuntimeError;
use crate::git::{GitCli, has_changes};
use crate::tool::{ApprovalLevel, BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

pub struct GitBranchList;
pub struct GitBranchCreate;
pub struct GitBranchSwitch;
pub struct GitBranchRename;
pub struct GitBranchDelete;
pub struct GitRemoteList;

fn cwd(args: &ToolArgs, ctx: &ToolCtx) -> Result<PathBuf, RuntimeError> {
    let explicit = args.named("cwd").and_then(|value| match value {
        Value::Str(path) => Some(std::path::Path::new(path)),
        _ => None,
    });
    ctx.resolve_cwd(explicit)
}

fn string_arg<'a>(args: &'a ToolArgs, name: &str) -> Result<&'a str, RuntimeError> {
    match args.named(name) {
        Some(Value::Str(value)) => Ok(value),
        Some(other) => Err(RuntimeError::TypeMismatch {
            expected: "string".into(),
            actual: other.kind_name().into(),
        }),
        None => Err(RuntimeError::MissingArg(name.into())),
    }
}

fn optional_string<'a>(args: &'a ToolArgs, name: &str) -> Result<Option<&'a str>, RuntimeError> {
    match args.named(name) {
        Some(Value::Str(value)) => Ok(Some(value)),
        Some(other) => Err(RuntimeError::TypeMismatch {
            expected: "string".into(),
            actual: other.kind_name().into(),
        }),
        None => Ok(None),
    }
}

fn bool_arg(args: &ToolArgs, name: &str, default: bool) -> Result<bool, RuntimeError> {
    match args.named(name) {
        Some(Value::Bool(value)) => Ok(*value),
        Some(other) => Err(RuntimeError::TypeMismatch {
            expected: "boolean".into(),
            actual: other.kind_name().into(),
        }),
        None => Ok(default),
    }
}

fn repo(args: &ToolArgs, ctx: &ToolCtx, tool: &str) -> Result<(PathBuf, Repository), RuntimeError> {
    let cwd = cwd(args, ctx)?;
    let repository = Repository::open(&cwd)
        .map_err(|error| RuntimeError::ToolFailed(format!("{tool}: {error}")))?;
    Ok((cwd, repository))
}

async fn repo_mut(
    args: &ToolArgs,
    ctx: &ToolCtx,
    tool: &str,
) -> Result<(PathBuf, Repository), RuntimeError> {
    let cwd = cwd(args, ctx)?;
    crate::fs_access::authorize_write(ctx, &cwd, tool, true).await?;
    let repository = Repository::open(&cwd)
        .map_err(|error| RuntimeError::ToolFailed(format!("{tool}: {error}")))?;
    Ok((cwd, repository))
}

fn result(tool: &str, error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::ToolFailed(format!("{tool}: {error}"))
}

fn branch_value(name: String, branch: &git2::Branch<'_>, current: bool) -> Value {
    let reference = branch.get();
    let commit = reference.target().map(|oid| oid.to_string());
    let upstream = branch
        .upstream()
        .ok()
        .and_then(|upstream| upstream.name().ok().flatten().map(str::to_owned));
    Value::Struct(vec![
        ("name".into(), Value::Str(name)),
        ("sha".into(), commit.map(Value::Str).unwrap_or(Value::Unit)),
        ("current".into(), Value::Bool(current)),
        (
            "upstream".into(),
            upstream.map(Value::Str).unwrap_or(Value::Unit),
        ),
    ])
}

impl Tool for GitBranchList {
    fn name(&self) -> &str {
        "git.branch.list"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some("List local branches and their current/upstream state.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"cwd":{"type":"string"}}})
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let (_, repository) = repo(&args, ctx, self.name())?;
            let current = repository
                .head()
                .ok()
                .and_then(|head| head.shorthand().map(str::to_owned));
            let mut values = Vec::new();
            for item in repository
                .branches(Some(BranchType::Local))
                .map_err(|e| result(self.name(), e))?
            {
                let (branch, _) = item.map_err(|e| result(self.name(), e))?;
                let name = branch
                    .name()
                    .map_err(|e| result(self.name(), e))?
                    .unwrap_or_default()
                    .to_owned();
                values.push(branch_value(
                    name.clone(),
                    &branch,
                    current.as_deref() == Some(name.as_str()),
                ));
            }
            Ok(Value::List(values))
        })
    }
}

impl Tool for GitBranchCreate {
    fn name(&self) -> &str {
        "git.branch.create"
    }
    fn tier(&self) -> Tier {
        Tier::Two
    }
    fn description(&self) -> Option<&str> {
        Some("Create a local branch without checking it out.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","required":["name"],"properties":{"name":{"type":"string"},"start":{"type":"string"},"cwd":{"type":"string"}}})
    }
    fn invocation_provenance(
        &self,
        args: &ToolArgs,
        ctx: &ToolCtx,
    ) -> Result<crate::permission::ResourceProvenance, RuntimeError> {
        crate::tools::git_ops::git_mutation_provenance(args, ctx)
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let (_, repository) = repo_mut(&args, ctx, self.name()).await?;
            let name = string_arg(&args, "name")?;
            let start = optional_string(&args, "start")?;
            let commit = match start {
                Some(revision) => repository
                    .revparse_single(revision)
                    .and_then(|object| object.peel_to_commit()),
                None => repository.head().and_then(|head| head.peel_to_commit()),
            }
            .map_err(|e| result(self.name(), e))?;
            repository
                .branch(name, &commit, false)
                .map_err(|e| result(self.name(), e))?;
            Ok(Value::Struct(vec![
                ("name".into(), Value::Str(name.into())),
                ("sha".into(), Value::Str(commit.id().to_string())),
            ]))
        })
    }
}

impl Tool for GitBranchSwitch {
    fn name(&self) -> &str {
        "git.branch.switch"
    }
    fn tier(&self) -> Tier {
        Tier::Two
    }
    fn approval_level(&self, args: &ToolArgs, _ctx: &ToolCtx) -> ApprovalLevel {
        match bool_arg(args, "force", false) {
            Ok(true) => ApprovalLevel::Dangerous,
            _ => ApprovalLevel::Approve,
        }
    }
    fn description(&self) -> Option<&str> {
        Some("Switch branches after refusing a dirty worktree unless force=true.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","required":["name"],"properties":{"name":{"type":"string"},"create":{"type":"boolean"},"start":{"type":"string"},"force":{"type":"boolean"},"cwd":{"type":"string"}}})
    }
    fn invocation_provenance(
        &self,
        args: &ToolArgs,
        ctx: &ToolCtx,
    ) -> Result<crate::permission::ResourceProvenance, RuntimeError> {
        crate::tools::git_ops::git_mutation_provenance(args, ctx)
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let (cwd, repository) = repo_mut(&args, ctx, self.name()).await?;
            let name = string_arg(&args, "name")?;
            let create = bool_arg(&args, "create", false)?;
            let force = bool_arg(&args, "force", false)?;
            if !force && has_changes(&cwd).map_err(|e| result(self.name(), e))? {
                return Err(result(self.name(), "dirty worktree requires force=true"));
            }
            if !create {
                repository
                    .find_branch(name, BranchType::Local)
                    .map_err(|e| result(self.name(), e))?;
            }
            GitCli::at(&cwd)
                .switch_branch(name, create, optional_string(&args, "start")?)
                .map_err(|e| result(self.name(), e))?;
            Ok(Value::Struct(vec![
                ("name".into(), Value::Str(name.into())),
                ("created".into(), Value::Bool(create)),
            ]))
        })
    }
}

impl Tool for GitBranchRename {
    fn name(&self) -> &str {
        "git.branch.rename"
    }
    fn tier(&self) -> Tier {
        Tier::Two
    }
    fn approval_level(&self, _args: &ToolArgs, _ctx: &ToolCtx) -> ApprovalLevel {
        ApprovalLevel::Approve
    }
    fn description(&self) -> Option<&str> {
        Some("Rename a local branch.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","required":["old","new"],"properties":{"old":{"type":"string"},"new":{"type":"string"},"cwd":{"type":"string"}}})
    }
    fn invocation_provenance(
        &self,
        args: &ToolArgs,
        ctx: &ToolCtx,
    ) -> Result<crate::permission::ResourceProvenance, RuntimeError> {
        crate::tools::git_ops::git_mutation_provenance(args, ctx)
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let (_, repository) = repo_mut(&args, ctx, self.name()).await?;
            let old = string_arg(&args, "old")?;
            let new = string_arg(&args, "new")?;
            let mut branch = repository
                .find_branch(old, BranchType::Local)
                .map_err(|e| result(self.name(), e))?;
            branch
                .rename(new, false)
                .map_err(|e| result(self.name(), e))?;
            Ok(Value::Struct(vec![
                ("old".into(), Value::Str(old.into())),
                ("new".into(), Value::Str(new.into())),
            ]))
        })
    }
}

impl Tool for GitBranchDelete {
    fn name(&self) -> &str {
        "git.branch.delete"
    }
    fn tier(&self) -> Tier {
        Tier::Three
    }
    fn approval_level(&self, _args: &ToolArgs, _ctx: &ToolCtx) -> ApprovalLevel {
        ApprovalLevel::Dangerous
    }
    fn description(&self) -> Option<&str> {
        Some("Delete a local branch; current branches are refused.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","required":["name"],"properties":{"name":{"type":"string"},"force":{"type":"boolean"},"cwd":{"type":"string"}}})
    }
    fn invocation_provenance(
        &self,
        args: &ToolArgs,
        ctx: &ToolCtx,
    ) -> Result<crate::permission::ResourceProvenance, RuntimeError> {
        crate::tools::git_ops::git_mutation_provenance(args, ctx)
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let (_, repository) = repo_mut(&args, ctx, self.name()).await?;
            let name = string_arg(&args, "name")?;
            if repository
                .head()
                .ok()
                .and_then(|head| head.shorthand().map(str::to_owned))
                .as_deref()
                == Some(name)
            {
                return Err(result(self.name(), "cannot delete current branch"));
            }
            let mut branch = repository
                .find_branch(name, BranchType::Local)
                .map_err(|e| result(self.name(), e))?;
            let force = bool_arg(&args, "force", false)?;
            if force {
                branch.delete().map_err(|e| result(self.name(), e))?;
            } else {
                branch.delete().map_err(|e| result(self.name(), e))?;
            }
            Ok(Value::Struct(vec![
                ("name".into(), Value::Str(name.into())),
                ("deleted".into(), Value::Bool(true)),
            ]))
        })
    }
}

impl Tool for GitRemoteList {
    fn name(&self) -> &str {
        "git.remote.list"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some("List configured Git remotes and fetch/push URLs.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"cwd":{"type":"string"}}})
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let (_, repository) = repo(&args, ctx, self.name())?;
            let mut values = Vec::new();
            for name in repository
                .remotes()
                .map_err(|e| result(self.name(), e))?
                .iter()
                .flatten()
            {
                let remote = repository
                    .find_remote(name)
                    .map_err(|e| result(self.name(), e))?;
                values.push(Value::Struct(vec![
                    ("name".into(), Value::Str(name.to_string())),
                    (
                        "url".into(),
                        remote
                            .url()
                            .map(str::to_owned)
                            .map(Value::Str)
                            .unwrap_or(Value::Unit),
                    ),
                    (
                        "push_url".into(),
                        remote
                            .pushurl()
                            .map(str::to_owned)
                            .map(Value::Str)
                            .unwrap_or(Value::Unit),
                    ),
                ]));
            }
            Ok(Value::List(values))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn run(dir: &std::path::Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
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

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["init", "-q", "-b", "main"]);
        run(dir.path(), &["config", "user.name", "test"]);
        run(dir.path(), &["config", "user.email", "test@example.com"]);
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        run(dir.path(), &["add", "."]);
        run(dir.path(), &["commit", "-qm", "initial"]);
        dir
    }

    #[test]
    fn branch_lifecycle_and_dirty_switch_safety() {
        let dir = repo();
        let args = ToolArgs {
            named: vec![
                ("cwd".into(), Value::Str(dir.path().display().to_string())),
                ("name".into(), Value::Str("feature".into())),
            ],
            ..ToolArgs::default()
        };
        let ctx = ToolCtx::default();
        futures::executor::block_on(GitBranchCreate.call(args, &ctx)).unwrap();
        assert!(
            run(dir.path(), &["show-ref", "--verify", "refs/heads/feature"]).contains("feature")
        );
        let switch_args = ToolArgs {
            named: vec![
                ("cwd".into(), Value::Str(dir.path().display().to_string())),
                ("name".into(), Value::Str("feature".into())),
            ],
            ..ToolArgs::default()
        };
        futures::executor::block_on(GitBranchSwitch.call(switch_args, &ctx)).unwrap();
        assert_eq!(run(dir.path(), &["branch", "--show-current"]), "feature");
        std::fs::write(dir.path().join("dirty"), "x").unwrap();
        let back_args = ToolArgs {
            named: vec![
                ("cwd".into(), Value::Str(dir.path().display().to_string())),
                ("name".into(), Value::Str("main".into())),
            ],
            ..ToolArgs::default()
        };
        assert!(futures::executor::block_on(GitBranchSwitch.call(back_args, &ctx)).is_err());
        let rename_args = ToolArgs {
            named: vec![
                ("cwd".into(), Value::Str(dir.path().display().to_string())),
                ("old".into(), Value::Str("feature".into())),
                ("new".into(), Value::Str("renamed".into())),
            ],
            ..ToolArgs::default()
        };
        futures::executor::block_on(GitBranchRename.call(rename_args, &ctx)).unwrap();
        let delete_args = ToolArgs {
            named: vec![
                ("cwd".into(), Value::Str(dir.path().display().to_string())),
                ("name".into(), Value::Str("renamed".into())),
            ],
            ..ToolArgs::default()
        };
        assert!(futures::executor::block_on(GitBranchDelete.call(delete_args, &ctx)).is_err());
    }

    #[test]
    fn force_switch_requires_dangerous_approval() {
        let tool = GitBranchSwitch;
        let ctx = ToolCtx::default();
        assert_eq!(
            tool.approval_level(&ToolArgs::default(), &ctx),
            ApprovalLevel::Approve
        );
        let args = ToolArgs {
            named: vec![("force".into(), Value::Bool(true))],
            ..ToolArgs::default()
        };
        assert_eq!(tool.approval_level(&args, &ctx), ApprovalLevel::Dangerous);
    }

    #[test]
    fn branch_list_and_remote_list_return_structured_values() {
        let dir = repo();
        run(
            dir.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ],
        );
        let ctx = ToolCtx::default();
        let args = ToolArgs {
            named: vec![("cwd".into(), Value::Str(dir.path().display().to_string()))],
            ..ToolArgs::default()
        };
        let branches = futures::executor::block_on(GitBranchList.call(args.clone(), &ctx)).unwrap();
        assert!(matches!(branches, Value::List(items) if !items.is_empty()));
        let remotes = futures::executor::block_on(GitRemoteList.call(args, &ctx)).unwrap();
        assert!(matches!(remotes, Value::List(items) if items.len() == 1));
    }
}
