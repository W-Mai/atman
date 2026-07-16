use std::path::PathBuf;
use std::time::Duration;

use git2::{BranchType, DiffFormat, Repository, Status, StatusOptions};

use crate::error::RuntimeError;
use crate::stream::StreamFrame;
use crate::tool::{ApprovalLevel, BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

pub struct GitStatus;

pub struct GitShow;

impl Tool for GitShow {
    fn name(&self) -> &str {
        "git.show"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some("Show the patch introduced by one commit.")
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "sha": {"type": "string", "description": "Commit SHA or rev."},
                "cwd": {"type": "string", "description": "Optional working dir; defaults to current process directory."}
            },
            "required": ["sha"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let sha = extract_string(&args, "sha", 0)?;
            let cwd = extract_cwd(&args, "git.show cwd")?;
            let repo = Repository::open(&cwd)
                .map_err(|e| RuntimeError::ToolFailed(format!("git.show: {e}")))?;
            let object = repo
                .revparse_single(&sha)
                .map_err(|e| RuntimeError::ToolFailed(format!("git.show rev: {e}")))?;
            let commit = object
                .peel_to_commit()
                .map_err(|e| RuntimeError::ToolFailed(format!("git.show commit: {e}")))?;
            let new_tree = commit
                .tree()
                .map_err(|e| RuntimeError::ToolFailed(format!("git.show tree: {e}")))?;
            let old_tree = if commit.parent_count() == 0 {
                None
            } else {
                Some(
                    commit
                        .parent(0)
                        .and_then(|p| p.tree())
                        .map_err(|e| RuntimeError::ToolFailed(format!("git.show parent: {e}")))?,
                )
            };
            let diff = repo
                .diff_tree_to_tree(old_tree.as_ref(), Some(&new_tree), None)
                .map_err(|e| RuntimeError::ToolFailed(format!("git.show diff: {e}")))?;
            let mut files = Vec::new();
            diff.foreach(
                &mut |delta, _| {
                    let path = delta
                        .new_file()
                        .path()
                        .or_else(|| delta.old_file().path())
                        .map(|p| p.to_string_lossy().into_owned());
                    if let Some(path) = path
                        && !files.contains(&path)
                    {
                        files.push(path);
                    }
                    true
                },
                None,
                None,
                None,
            )
            .map_err(|e| RuntimeError::ToolFailed(format!("git.show files: {e}")))?;
            let mut body = String::new();
            diff.print(DiffFormat::Patch, |_delta, _hunk, line| {
                match line.origin() {
                    'F' | 'H' => body.push_str(&String::from_utf8_lossy(line.content())),
                    '+' | '-' | ' ' => {
                        body.push(line.origin());
                        body.push_str(&String::from_utf8_lossy(line.content()));
                    }
                    _ => body.push_str(&String::from_utf8_lossy(line.content())),
                }
                true
            })
            .map_err(|e| RuntimeError::ToolFailed(format!("git.show patch: {e}")))?;
            let resolved = commit.id().to_string();
            if let Some(tx) = &ctx.stream_tx {
                let _ = tx.send(StreamFrame::DiffPreview {
                    title: format!("git show {sha}"),
                    old_content: None,
                    new_content: None,
                    unified_diff: Some(body.clone()),
                });
            }
            Ok(Value::Struct(vec![
                ("sha".into(), Value::Str(resolved)),
                ("diff".into(), Value::Str(body)),
                (
                    "files".into(),
                    Value::List(files.into_iter().map(Value::Str).collect()),
                ),
            ]))
        })
    }
}

impl Tool for GitStatus {
    fn name(&self) -> &str {
        "git.status"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some("Show working tree status: staged, unstaged, and untracked files.")
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "cwd": {"type": "string", "description": "Optional working dir; defaults to current process directory."}
            }
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let cwd = extract_cwd(&args, "git.status cwd")?;
            let repo = Repository::open(&cwd)
                .map_err(|e| RuntimeError::ToolFailed(format!("git.status: {e}")))?;
            let mut opts = StatusOptions::new();
            opts.include_untracked(true)
                .renames_head_to_index(true)
                .renames_index_to_workdir(true);
            let statuses = repo
                .statuses(Some(&mut opts))
                .map_err(|e| RuntimeError::ToolFailed(format!("git.status: {e}")))?;
            let mut staged = Vec::new();
            let mut unstaged = Vec::new();
            let mut untracked = Vec::new();
            for entry in statuses.iter() {
                let status = entry.status();
                let Some(path) = entry.path().map(str::to_string) else {
                    continue;
                };
                if status.is_wt_new() {
                    untracked.push(Value::Str(path.clone()));
                }
                if let Some(label) = index_status(status) {
                    staged.push(status_entry(path.clone(), label));
                }
                if let Some(label) = worktree_status(status) {
                    unstaged.push(status_entry(path, label));
                }
            }
            Ok(Value::Struct(vec![
                ("staged".into(), Value::List(staged)),
                ("unstaged".into(), Value::List(unstaged)),
                ("untracked".into(), Value::List(untracked)),
            ]))
        })
    }
}

pub struct GitAdd;

impl Tool for GitAdd {
    fn name(&self) -> &str {
        "git.add"
    }

    fn tier(&self) -> Tier {
        Tier::Two
    }

    fn description(&self) -> Option<&str> {
        Some("Stage files for commit. Pass specific paths — do NOT stage everything blindly.")
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "paths": {"type": "array", "items": {"type": "string"}, "description": "File paths to stage."},
                "cwd": {"type": "string", "description": "Optional working dir."}
            },
            "required": ["paths"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let paths = extract_string_list(&args, "paths")?;
            let cwd = extract_cwd(&args, "git.add cwd")?;
            let repo = Repository::open(&cwd)
                .map_err(|e| RuntimeError::ToolFailed(format!("git.add: {e}")))?;
            let mut index = repo
                .index()
                .map_err(|e| RuntimeError::ToolFailed(format!("git.add index: {e}")))?;
            for p in &paths {
                index
                    .add_path(std::path::Path::new(p))
                    .map_err(|e| RuntimeError::ToolFailed(format!("git.add {p}: {e}")))?;
            }
            index
                .write()
                .map_err(|e| RuntimeError::ToolFailed(format!("git.add write: {e}")))?;
            Ok(Value::Struct(vec![(
                "staged".into(),
                Value::List(paths.into_iter().map(Value::Str).collect()),
            )]))
        })
    }
}

pub struct GitCommit;

impl Tool for GitCommit {
    fn name(&self) -> &str {
        "git.commit"
    }

    fn tier(&self) -> Tier {
        Tier::Two
    }

    fn description(&self) -> Option<&str> {
        Some("Commit staged changes. Use 'amend: true' to amend the last commit.")
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "message": {"type": "string", "description": "Commit message."},
                "amend": {"type": "boolean", "default": false, "description": "Amend the last commit instead of creating a new commit."},
                "cwd": {"type": "string", "description": "Optional working dir; defaults to current process directory."}
            },
            "required": ["message"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let message = extract_string(&args, "message", 0)?;
            let amend = extract_optional_bool(&args, "amend").unwrap_or(false);
            let cwd = extract_cwd(&args, "git.commit cwd")?;
            let repo = Repository::open(&cwd)
                .map_err(|e| RuntimeError::ToolFailed(format!("git.commit: {e}")))?;
            let files_count = staged_count(&repo, "git.commit")?;
            let mut index = repo
                .index()
                .map_err(|e| RuntimeError::ToolFailed(format!("git.commit: {e}")))?;
            let tree_id = index
                .write_tree()
                .map_err(|e| RuntimeError::ToolFailed(format!("git.commit: {e}")))?;
            let tree = repo
                .find_tree(tree_id)
                .map_err(|e| RuntimeError::ToolFailed(format!("git.commit: {e}")))?;
            let sig = repo
                .signature()
                .map_err(|e| RuntimeError::ToolFailed(format!("git.commit signature: {e}")))?;
            let head = repo
                .head()
                .map_err(|e| RuntimeError::ToolFailed(format!("git.commit head: {e}")))?;
            let parent = head
                .peel_to_commit()
                .map_err(|e| RuntimeError::ToolFailed(format!("git.commit head: {e}")))?;
            let oid = if amend {
                parent
                    .amend(
                        Some("HEAD"),
                        Some(&sig),
                        Some(&sig),
                        None,
                        Some(&message),
                        Some(&tree),
                    )
                    .map_err(|e| RuntimeError::ToolFailed(format!("git.commit amend: {e}")))?
            } else {
                repo.commit(Some("HEAD"), &sig, &sig, &message, &tree, &[&parent])
                    .map_err(|e| RuntimeError::ToolFailed(format!("git.commit: {e}")))?
            };
            index
                .write()
                .map_err(|e| RuntimeError::ToolFailed(format!("git.commit index: {e}")))?;
            Ok(Value::Struct(vec![
                ("sha".into(), Value::Str(oid.to_string())),
                ("message".into(), Value::Str(message)),
                ("files_count".into(), Value::Int(files_count)),
            ]))
        })
    }
}

pub struct GitBranch;

impl Tool for GitBranch {
    fn name(&self) -> &str {
        "git.branch"
    }

    fn tier(&self) -> Tier {
        Tier::Two
    }

    fn description(&self) -> Option<&str> {
        Some("Create and/or checkout a git branch.")
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Branch name."},
                "create": {"type": "boolean", "default": true, "description": "Create the branch before checkout."},
                "checkout": {"type": "boolean", "default": true, "description": "Checkout the branch."},
                "cwd": {"type": "string", "description": "Optional working dir; defaults to current process directory."}
            },
            "required": ["name"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let name = extract_string(&args, "name", 0)?;
            let create = extract_optional_bool(&args, "create").unwrap_or(true);
            let checkout = extract_optional_bool(&args, "checkout").unwrap_or(true);
            let cwd = extract_cwd(&args, "git.branch cwd")?;
            let repo = Repository::open(&cwd)
                .map_err(|e| RuntimeError::ToolFailed(format!("git.branch: {e}")))?;
            if create {
                let head = repo
                    .head()
                    .and_then(|h| h.peel_to_commit())
                    .map_err(|e| RuntimeError::ToolFailed(format!("git.branch head: {e}")))?;
                repo.branch(&name, &head, false)
                    .map_err(|e| RuntimeError::ToolFailed(format!("git.branch: {e}")))?;
            } else {
                repo.find_branch(&name, BranchType::Local)
                    .map_err(|e| RuntimeError::ToolFailed(format!("git.branch: {e}")))?;
            }
            if checkout {
                repo.set_head(&format!("refs/heads/{name}"))
                    .map_err(|e| RuntimeError::ToolFailed(format!("git.branch checkout: {e}")))?;
            }
            Ok(Value::Struct(vec![
                ("branch".into(), Value::Str(name)),
                ("created".into(), Value::Bool(create)),
                ("checked_out".into(), Value::Bool(checkout)),
            ]))
        })
    }
}

pub struct GitPush;

impl Tool for GitPush {
    fn name(&self) -> &str {
        "git.push"
    }

    fn tier(&self) -> Tier {
        Tier::Three
    }

    fn approval_level(&self, _args: &ToolArgs, _ctx: &ToolCtx) -> ApprovalLevel {
        ApprovalLevel::Dangerous
    }

    fn description(&self) -> Option<&str> {
        Some("Push current branch to remote. Requires approval.")
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "remote": {"type": "string", "default": "origin", "description": "Remote name."},
                "branch": {"type": "string", "description": "Branch name; defaults to current branch."},
                "cwd": {"type": "string", "description": "Optional working dir; defaults to current process directory."}
            }
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let remote =
                extract_optional_string(&args, "remote").unwrap_or_else(|| "origin".into());
            let cwd = extract_cwd(&args, "git.push cwd")?;
            let branch = match extract_optional_string(&args, "branch") {
                Some(branch) => branch,
                None => current_branch(&cwd)?,
            };
            let mut child = tokio::process::Command::new("git");
            child.args(["push", &remote, &branch]).current_dir(&cwd);
            let output = tokio::time::timeout(Duration::from_secs(300), child.output())
                .await
                .map_err(|_| RuntimeError::ToolFailed("git.push timeout after 300s".into()))?
                .map_err(|e| RuntimeError::ToolFailed(format!("git.push spawn: {e}")))?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let combined = match (stdout.is_empty(), stderr.is_empty()) {
                (true, true) => String::new(),
                (false, true) => stdout.into_owned(),
                (true, false) => stderr.into_owned(),
                (false, false) => format!("{stdout}\n{stderr}"),
            };
            Ok(Value::Struct(vec![
                ("ok".into(), Value::Bool(output.status.success())),
                ("remote".into(), Value::Str(remote)),
                ("branch".into(), Value::Str(branch)),
                ("output".into(), Value::Str(combined)),
            ]))
        })
    }
}

fn extract_cwd(args: &ToolArgs, label: &str) -> Result<PathBuf, RuntimeError> {
    match args.named("cwd") {
        Some(Value::Path(p)) => Ok(p.clone()),
        Some(Value::Str(s)) => Ok(PathBuf::from(s)),
        Some(other) => Err(RuntimeError::TypeMismatch {
            expected: "string".into(),
            actual: other.kind_name().into(),
        }),
        None => {
            std::env::current_dir().map_err(|e| RuntimeError::ToolFailed(format!("{label}: {e}")))
        }
    }
}

fn extract_string(args: &ToolArgs, name: &str, pos: usize) -> Result<String, RuntimeError> {
    let value = match args.named(name) {
        Some(v) => v,
        None => args.positional(pos)?,
    };
    match value {
        Value::Str(s) => Ok(s.clone()),
        other => Err(RuntimeError::TypeMismatch {
            expected: "string".into(),
            actual: other.kind_name().into(),
        }),
    }
}

fn extract_string_list(args: &ToolArgs, name: &str) -> Result<Vec<String>, RuntimeError> {
    match args.named(name) {
        Some(Value::List(items)) => items
            .iter()
            .map(|v| match v {
                Value::Str(s) => Ok(s.clone()),
                other => Err(RuntimeError::TypeMismatch {
                    expected: "string".into(),
                    actual: other.kind_name().into(),
                }),
            })
            .collect(),
        Some(other) => Err(RuntimeError::TypeMismatch {
            expected: "list<string>".into(),
            actual: other.kind_name().into(),
        }),
        None => Err(RuntimeError::MissingArg(name.into())),
    }
}

fn extract_optional_string(args: &ToolArgs, name: &str) -> Option<String> {
    match args.named(name)? {
        Value::Str(s) => Some(s.clone()),
        _ => None,
    }
}

fn extract_optional_bool(args: &ToolArgs, name: &str) -> Option<bool> {
    match args.named(name)? {
        Value::Bool(b) => Some(*b),
        _ => None,
    }
}

fn index_status(status: Status) -> Option<&'static str> {
    if status.is_index_new() {
        Some("new")
    } else if status.is_index_modified() {
        Some("modified")
    } else if status.is_index_deleted() {
        Some("deleted")
    } else if status.is_index_renamed() {
        Some("renamed")
    } else {
        None
    }
}

fn worktree_status(status: Status) -> Option<&'static str> {
    if status.is_wt_modified() {
        Some("modified")
    } else if status.is_wt_deleted() {
        Some("deleted")
    } else if status.is_wt_renamed() {
        Some("renamed")
    } else {
        None
    }
}

fn status_entry(path: String, status: &str) -> Value {
    Value::Struct(vec![
        ("path".into(), Value::Str(path)),
        ("status".into(), Value::Str(status.into())),
    ])
}

fn staged_count(repo: &Repository, tool: &str) -> Result<i64, RuntimeError> {
    let mut opts = StatusOptions::new();
    opts.include_untracked(false).renames_head_to_index(true);
    let statuses = repo
        .statuses(Some(&mut opts))
        .map_err(|e| RuntimeError::ToolFailed(format!("{tool}: {e}")))?;
    Ok(statuses
        .iter()
        .filter(|entry| index_status(entry.status()).is_some())
        .count() as i64)
}

fn current_branch(cwd: &std::path::Path) -> Result<String, RuntimeError> {
    let repo =
        Repository::open(cwd).map_err(|e| RuntimeError::ToolFailed(format!("git.push: {e}")))?;
    let head = repo
        .head()
        .map_err(|e| RuntimeError::ToolFailed(format!("git.push head: {e}")))?;
    head.shorthand()
        .map(str::to_string)
        .ok_or_else(|| RuntimeError::ToolFailed("git.push: detached HEAD has no branch".into()))
}
