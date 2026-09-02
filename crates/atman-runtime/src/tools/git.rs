use crate::error::RuntimeError;
use crate::git;
use crate::stream::StreamFrame;
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

pub struct GitDiff;

pub struct GitInit;

impl Tool for GitInit {
    fn name(&self) -> &str {
        "git.init"
    }

    fn tier(&self) -> Tier {
        Tier::Two
    }

    fn description(&self) -> Option<&str> {
        Some("Initialize a Git repository using libgit2, without spawning git.")
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "cwd": {"type": "string", "description": "Directory to initialize; defaults to the current directory."},
                "initial_branch": {"type": "string", "description": "Initial branch name; uses libgit2's configured default when omitted."},
                "bare": {"type": "boolean", "description": "Create a bare repository."}
            }
        })
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
            let explicit = match args.named("cwd") {
                Some(Value::Path(path)) => Some(path.as_path()),
                Some(Value::Str(path)) => Some(std::path::Path::new(path)),
                Some(other) => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "string".into(),
                        actual: other.kind_name().into(),
                    });
                }
                None => None,
            };
            let path = ctx.resolve_cwd(explicit)?;
            crate::fs_access::authorize_write(ctx, &path, self.name(), true).await?;
            let bare = match args.named("bare") {
                Some(Value::Bool(bare)) => *bare,
                Some(other) => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "boolean".into(),
                        actual: other.kind_name().into(),
                    });
                }
                None => false,
            };
            let initial_branch = match args.named("initial_branch") {
                Some(Value::Str(branch)) => Some(branch.as_str()),
                Some(other) => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "string".into(),
                        actual: other.kind_name().into(),
                    });
                }
                None => None,
            };
            let info = git::init_repository(&path, bare, initial_branch)
                .map_err(|e| RuntimeError::ToolFailed(format!("git.init: {e}")))?;
            Ok(Value::Struct(vec![
                ("path".into(), Value::Path(info.path)),
                (
                    "workdir".into(),
                    info.workdir.map(Value::Path).unwrap_or(Value::Unit),
                ),
                ("git_dir".into(), Value::Path(info.git_dir)),
                ("bare".into(), Value::Bool(info.bare)),
            ]))
        })
    }
}

impl Tool for GitDiff {
    fn name(&self) -> &str {
        "git.diff"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Return { diff, files } for the given git ref range so an LLM can receive only the \
             changed text, not entire files. Optional `paths` narrows the diff to those \
             pathspecs. Backed by libgit2 (read-only, no shell spawn).",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "range": {"type": "string", "description": "git ref range, e.g. HEAD~3..HEAD"},
                "paths": {"type": "array", "items": {"type": "string"}, "description": "optional path filter"},
                "cwd": {"type": "string", "description": "optional working dir; defaults to atman's cwd"}
            },
            "required": ["range"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let range = extract_string(&args, "range", 0)?;
            let paths = extract_string_list(&args, "paths", 1).unwrap_or_default();
            let explicit = args.named("cwd").and_then(|value| match value {
                Value::Str(path) => Some(std::path::Path::new(path)),
                _ => None,
            });
            let cwd = ctx.resolve_cwd(explicit)?;
            let out = git::diff_range(&cwd, &range, &paths)
                .map_err(|e| RuntimeError::ToolFailed(format!("git.diff: {e}")))?;
            if let Some(tx) = &ctx.stream_tx {
                let _ = tx.send(StreamFrame::DiffPreview {
                    title: format!("git diff {range}"),
                    tool_use_id: ctx.tool_use_id.clone(),
                    old_content: None,
                    new_content: None,
                    unified_diff: Some(out.body.clone()),
                    run_id: ctx.flow_run_id.as_ref().map(|r| r.0.to_string()),
                });
            }
            Ok(Value::Struct(vec![
                ("diff".into(), Value::Str(out.body)),
                (
                    "files".into(),
                    Value::List(out.files.into_iter().map(Value::Str).collect()),
                ),
            ]))
        })
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

fn extract_string_list(
    args: &ToolArgs,
    name: &str,
    pos: usize,
) -> Result<Vec<String>, RuntimeError> {
    let value = match args.named(name) {
        Some(v) => v,
        None => match args.positional(pos) {
            Ok(v) => v,
            Err(_) => return Ok(Vec::new()),
        },
    };
    match value {
        Value::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                match it {
                    Value::Str(s) => out.push(s.clone()),
                    other => {
                        return Err(RuntimeError::TypeMismatch {
                            expected: "list of string".into(),
                            actual: other.kind_name().into(),
                        });
                    }
                }
            }
            Ok(out)
        }
        Value::Unit => Ok(Vec::new()),
        other => Err(RuntimeError::TypeMismatch {
            expected: "list of string".into(),
            actual: other.kind_name().into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::GitCli;
    use std::path::Path;

    fn have_git() -> bool {
        GitCli::ensure_available().is_ok()
    }

    fn seed_two_commits(dir: &Path) {
        let cli = GitCli::at(dir);
        cli.init("main").unwrap();
        for (k, v) in [
            ("user.email", "t@atman.local"),
            ("user.name", "atman test"),
            ("commit.gpgsign", "false"),
        ] {
            cli.run(&["config", k, v]).unwrap();
        }
        std::fs::write(dir.join("a.txt"), "line one\n").unwrap();
        std::fs::write(dir.join("b.txt"), "b\n").unwrap();
        cli.add_all().unwrap();
        cli.commit("initial").unwrap();
        std::fs::write(dir.join("a.txt"), "line one\nline two\n").unwrap();
        std::fs::write(dir.join("c.txt"), "new file\n").unwrap();
        cli.add_all().unwrap();
        cli.commit("second").unwrap();
    }

    #[tokio::test]
    async fn diff_returns_body_and_files() {
        if !have_git() {
            eprintln!("skip: git not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        seed_two_commits(tmp.path());
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![Value::Str("HEAD~1..HEAD".into())],
            named: vec![(
                "cwd".into(),
                Value::Str(tmp.path().to_string_lossy().into()),
            )],
        };
        let v = GitDiff.call(args, &ctx).await.unwrap();
        let Value::Struct(fields) = v else {
            panic!("expected struct, got {v:?}");
        };
        let diff = fields
            .iter()
            .find(|(k, _)| k == "diff")
            .and_then(|(_, v)| {
                if let Value::Str(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .unwrap();
        assert!(diff.contains("+line two"), "want addition, got:\n{diff}");
        assert!(diff.contains("+new file"), "want new file:\n{diff}");
        let files = fields
            .iter()
            .find(|(k, _)| k == "files")
            .and_then(|(_, v)| {
                if let Value::List(xs) = v {
                    Some(xs.clone())
                } else {
                    None
                }
            })
            .unwrap();
        let names: Vec<String> = files
            .into_iter()
            .filter_map(|v| if let Value::Str(s) = v { Some(s) } else { None })
            .collect();
        assert!(names.contains(&"a.txt".to_string()), "files={names:?}");
        assert!(names.contains(&"c.txt".to_string()), "files={names:?}");
    }

    #[tokio::test]
    async fn diff_paths_filter_narrows() {
        if !have_git() {
            eprintln!("skip");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        seed_two_commits(tmp.path());
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![Value::Str("HEAD~1..HEAD".into())],
            named: vec![
                (
                    "cwd".into(),
                    Value::Str(tmp.path().to_string_lossy().into()),
                ),
                (
                    "paths".into(),
                    Value::List(vec![Value::Str("a.txt".into())]),
                ),
            ],
        };
        let v = GitDiff.call(args, &ctx).await.unwrap();
        let Value::Struct(fields) = v else {
            panic!("struct");
        };
        let files = fields
            .iter()
            .find(|(k, _)| k == "files")
            .and_then(|(_, v)| {
                if let Value::List(xs) = v {
                    Some(xs.clone())
                } else {
                    None
                }
            })
            .unwrap();
        let names: Vec<String> = files
            .into_iter()
            .filter_map(|v| if let Value::Str(s) = v { Some(s) } else { None })
            .collect();
        assert_eq!(names, vec!["a.txt".to_string()], "files={names:?}");
    }

    #[tokio::test]
    async fn diff_outside_git_repo_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![Value::Str("HEAD".into())],
            named: vec![(
                "cwd".into(),
                Value::Str(tmp.path().to_string_lossy().into()),
            )],
        };
        let err = GitDiff.call(args, &ctx).await.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("not a git repository"),
            "want repo error: {msg}"
        );
    }

    #[tokio::test]
    async fn diff_invalid_range_errors() {
        if !have_git() {
            eprintln!("skip");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        seed_two_commits(tmp.path());
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![Value::Str("nope_ref..other_nope".into())],
            named: vec![(
                "cwd".into(),
                Value::Str(tmp.path().to_string_lossy().into()),
            )],
        };
        let err = GitDiff.call(args, &ctx).await.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("libgit2") || msg.contains("revspec"),
            "err={msg}"
        );
    }
}
