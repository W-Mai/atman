use std::path::Path;
use std::process::Stdio;

use crate::error::RuntimeError;
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

pub struct GitDiff;

impl Tool for GitDiff {
    fn name(&self) -> &str {
        "git.diff"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Shell out to `git diff <range>` and return { diff, files } so an LLM can \
             receive only the changed text, not entire files. Optional `paths` filter maps \
             to `-- <paths...>`. Read-only, runs in the current working directory.",
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

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let range = extract_string(&args, "range", 0)?;
            let paths = extract_string_list(&args, "paths", 1).unwrap_or_default();
            let cwd = match args.named("cwd") {
                Some(Value::Str(s)) => std::path::PathBuf::from(s),
                _ => std::env::current_dir()
                    .map_err(|e| RuntimeError::ToolFailed(format!("git.diff cwd: {e}")))?,
            };
            run_git_diff(&range, &paths, &cwd).await
        })
    }
}

async fn run_git_diff(range: &str, paths: &[String], cwd: &Path) -> ToolResult {
    let files = git_command(cwd, &["diff", "--name-only", range], paths).await?;
    let diff = git_command(cwd, &["diff", range], paths).await?;
    let files_list: Vec<Value> = files
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| Value::Str(l.to_string()))
        .collect();
    Ok(Value::Struct(vec![
        ("diff".into(), Value::Str(diff)),
        ("files".into(), Value::List(files_list)),
    ]))
}

async fn git_command(cwd: &Path, args: &[&str], paths: &[String]) -> Result<String, RuntimeError> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args).current_dir(cwd);
    if !paths.is_empty() {
        cmd.arg("--");
        for p in paths {
            cmd.arg(p);
        }
    }
    let output = cmd
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| RuntimeError::ToolFailed(format!("git.diff spawn: {e}")))?;
    if !output.status.success() {
        return Err(RuntimeError::ToolFailed(format!(
            "git {} exit {}: {}",
            args.join(" "),
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
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
    use std::path::Path;
    use std::process::Command;

    fn have_git() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn seed_repo_with_two_commits(dir: &Path) {
        for (args, expect_ok) in [
            (vec!["init", "--initial-branch=main"], true),
            (vec!["config", "user.email", "t@atman.local"], true),
            (vec!["config", "user.name", "atman test"], true),
            (vec!["config", "commit.gpgsign", "false"], true),
        ] {
            let out = Command::new("git")
                .args(&args)
                .current_dir(dir)
                .output()
                .expect("git");
            assert!(
                out.status.success() || !expect_ok,
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        std::fs::write(dir.join("a.txt"), "line one\n").unwrap();
        std::fs::write(dir.join("b.txt"), "b\n").unwrap();
        run_ok(dir, &["add", "."]);
        run_ok(dir, &["commit", "-m", "initial"]);
        std::fs::write(dir.join("a.txt"), "line one\nline two\n").unwrap();
        std::fs::write(dir.join("c.txt"), "new file\n").unwrap();
        run_ok(dir, &["add", "."]);
        run_ok(dir, &["commit", "-m", "second"]);
    }

    fn run_ok(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[tokio::test]
    async fn diff_between_two_commits_reports_touched_files_and_body() {
        if !have_git() {
            eprintln!("skip: git not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        seed_repo_with_two_commits(tmp.path());
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
            .expect("want diff field");
        assert!(diff.contains("+line two"), "want addition, got:\n{diff}");
        assert!(diff.contains("+new file"), "want new file body:\n{diff}");
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
            .expect("want files field");
        let names: Vec<String> = files
            .into_iter()
            .filter_map(|v| if let Value::Str(s) = v { Some(s) } else { None })
            .collect();
        assert!(
            names.contains(&"a.txt".to_string()),
            "want a.txt: {names:?}"
        );
        assert!(
            names.contains(&"c.txt".to_string()),
            "want c.txt: {names:?}"
        );
    }

    #[tokio::test]
    async fn diff_paths_filter_narrows_to_named_files() {
        if !have_git() {
            eprintln!("skip: git not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        seed_repo_with_two_commits(tmp.path());
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
            panic!("expected struct");
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
        assert_eq!(
            names,
            vec!["a.txt".to_string()],
            "path filter should scope: {names:?}"
        );
    }

    #[tokio::test]
    async fn diff_invalid_range_surfaces_stderr() {
        if !have_git() {
            eprintln!("skip: git not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        seed_repo_with_two_commits(tmp.path());
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
            msg.contains("git diff") || msg.contains("unknown"),
            "want git error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn diff_outside_git_repo_reports_error() {
        if !have_git() {
            eprintln!("skip");
            return;
        }
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
            msg.contains("git diff") && msg.contains("exit"),
            "want git diff exit error, got: {msg}"
        );
    }
}
