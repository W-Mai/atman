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
fn cwd(args: &ToolArgs, ctx: &ToolCtx) -> Result<PathBuf, RuntimeError> {
    let explicit = args
        .named("cwd")
        .and_then(string_value)
        .map(std::path::Path::new);
    ctx.resolve_cwd(explicit)
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
struct ManagerPaths {
    cwd: PathBuf,
    external_root: Option<PathBuf>,
}

fn manager_paths(args: &ToolArgs, ctx: &ToolCtx) -> Result<ManagerPaths, RuntimeError> {
    Ok(ManagerPaths {
        cwd: cwd(args, ctx)?,
        external_root: optional_string(args, "external_root")
            .map(|root| ctx.resolve_path(std::path::Path::new(root)))
            .transpose()?,
    })
}

fn manager(paths: &ManagerPaths) -> Result<WorkspaceManager, RuntimeError> {
    WorkspaceManager::at(&paths.cwd, paths.external_root.as_deref())
        .map_err(|e| failure("git.workspace", e))
}

fn existing_manager(paths: &ManagerPaths) -> Result<Option<WorkspaceManager>, RuntimeError> {
    WorkspaceManager::open_existing(&paths.cwd, paths.external_root.as_deref())
        .map_err(|e| failure("git.workspace", e))
}
fn failure(tool: &str, error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::ToolFailed(format!("{tool}: {error}"))
}

async fn authorize_mutation(
    name: &str,
    args: &ToolArgs,
    ctx: &ToolCtx,
    paths: &ManagerPaths,
) -> Result<(), RuntimeError> {
    let mutates = match name {
        "git.workspace.create" | "git.workspace.release" | "git.workspace.retain" => true,
        "git.workspace.prune" => !bool_arg(args, "dry_run", true),
        _ => false,
    };
    if !mutates {
        return Ok(());
    }
    crate::fs_access::authorize_write(ctx, &paths.cwd, name, true).await?;
    if let Some(root) = &paths.external_root {
        crate::fs_access::authorize_write(ctx, root, name, true).await?;
    }
    Ok(())
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
            fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
                Box::pin(async move {
                    let paths = manager_paths(&args, ctx)?;
                    authorize_mutation(self.name(), &args, ctx, &paths).await?;
                    ($body)(args, &paths).map_err(|e: RuntimeError| e)
                })
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
    |args: ToolArgs, paths: &ManagerPaths| {
        let id = required_string(&args, "id")?;
        let item = manager(paths)?
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
    |_args: ToolArgs, paths: &ManagerPaths| {
        let items = match existing_manager(paths)? {
            Some(manager) => manager
                .list()
                .map_err(|e| failure("git.workspace.list", e))?,
            None => Vec::new(),
        };
        Ok(Value::List(items.into_iter().map(record_value).collect()))
    }
);
basic_tool!(
    GitWorkspaceGet,
    "git.workspace.get",
    Tier::One,
    ApprovalLevel::Auto,
    "Get one managed workspace.",
    serde_json::json!({"type":"object","required":["id"],"properties":{"id":{"type":"string"},"cwd":{"type":"string"},"external_root":{"type":"string"}}}),
    |args: ToolArgs, paths: &ManagerPaths| {
        let id = required_string(&args, "id")?;
        let manager = existing_manager(paths)?
            .ok_or_else(|| failure("git.workspace.get", format!("workspace {id} not found")))?;
        Ok(record_value(
            manager
                .get(id)
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
    |args: ToolArgs, paths: &ManagerPaths| {
        Ok(record_value(
            manager(paths)?
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
    |args: ToolArgs, paths: &ManagerPaths| {
        Ok(record_value(
            manager(paths)?
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
    |args: ToolArgs, paths: &ManagerPaths| {
        let dry_run = bool_arg(&args, "dry_run", true);
        let items = if dry_run {
            match existing_manager(paths)? {
                Some(manager) => manager
                    .prune(true)
                    .map_err(|e| failure("git.workspace.prune", e))?,
                None => Vec::new(),
            }
        } else {
            manager(paths)?
                .prune(false)
                .map_err(|e| failure("git.workspace.prune", e))?
        };
        Ok(Value::List(items.into_iter().map(record_value).collect()))
    }
);

#[allow(dead_code)]
fn _workspace_error_type(_: WorkspaceError) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

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

    fn init_repo(path: &Path) {
        git(path, &["init", "-q"]);
        git(path, &["config", "user.email", "atman@example.invalid"]);
        git(path, &["config", "user.name", "Atman Test"]);
        std::fs::write(path.join("README.md"), "seed\n").unwrap();
        git(path, &["add", "README.md"]);
        git(path, &["commit", "-qm", "seed"]);
    }

    fn managed_ctx(root: &Path) -> ToolCtx {
        ToolCtx::new()
            .with_fs_access(crate::fs_access::FsAccessPolicy::workspace_write(
                root.to_path_buf(),
            ))
            .with_workspace(crate::git_workspace::WorkspaceBinding {
                workspace_id: "test".into(),
                repository_root: root.to_path_buf(),
                path: root.to_path_buf(),
                branch: None,
            })
    }

    #[tokio::test]
    async fn list_and_dry_run_prune_without_registry_have_no_filesystem_side_effects() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let exclude = repo.path().join(".git/info/exclude");
        let exclude_before = std::fs::read(&exclude).unwrap();
        let status_before = git(
            repo.path(),
            &["status", "--porcelain=v1", "--untracked-files=all"],
        );
        let ctx = managed_ctx(repo.path());

        let listed = GitWorkspaceList
            .call(ToolArgs::default(), &ctx)
            .await
            .unwrap();
        let pruned = GitWorkspacePrune
            .call(ToolArgs::default(), &ctx)
            .await
            .unwrap();

        assert!(matches!(listed, Value::List(items) if items.is_empty()));
        assert!(matches!(pruned, Value::List(items) if items.is_empty()));
        assert!(!repo.path().join(".atman").exists());
        assert_eq!(std::fs::read(&exclude).unwrap(), exclude_before);
        assert_eq!(
            git(
                repo.path(),
                &["status", "--porcelain=v1", "--untracked-files=all"]
            ),
            status_before
        );
    }

    #[tokio::test]
    async fn relative_external_root_is_resolved_once_for_authorization_and_manager() {
        let root = tempfile::tempdir().unwrap();
        let seed = tempfile::tempdir().unwrap();
        init_repo(seed.path());
        let bare = root.path().join("repo.git");
        git(
            root.path(),
            &[
                "clone",
                "-q",
                "--bare",
                seed.path().to_str().unwrap(),
                bare.file_name().unwrap().to_str().unwrap(),
            ],
        );
        std::fs::create_dir(root.path().join("storage")).unwrap();
        let ctx = managed_ctx(root.path());
        let args = ToolArgs {
            named: vec![
                ("cwd".into(), Value::Str("repo.git".into())),
                ("external_root".into(), Value::Str("storage".into())),
                ("id".into(), Value::Str("relative-root".into())),
            ],
            ..ToolArgs::default()
        };

        let value = GitWorkspaceCreate.call(args, &ctx).await.unwrap();
        let expected = root
            .path()
            .join("storage/.atman/worktrees/relative-root")
            .canonicalize()
            .unwrap();
        let actual = match value {
            Value::Struct(fields) => fields
                .into_iter()
                .find_map(|(key, value)| {
                    (key == "worktree_path").then(|| match value {
                        Value::Str(path) => PathBuf::from(path),
                        other => panic!("unexpected worktree path value: {other:?}"),
                    })
                })
                .unwrap(),
            other => panic!("unexpected workspace value: {other:?}"),
        };

        assert_eq!(actual, expected);
        assert!(root.path().join("storage/.atman/workspaces.json").exists());
        assert!(
            !std::env::current_dir()
                .unwrap()
                .join("storage/.atman/workspaces.json")
                .exists()
        );
    }
}
