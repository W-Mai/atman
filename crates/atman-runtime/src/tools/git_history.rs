use std::path::PathBuf;

use crate::error::RuntimeError;
use crate::git::GitCli;
use crate::tool::{ApprovalLevel, BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

pub struct GitRestore;
pub struct GitRevert;
pub struct GitTagList;
pub struct GitTagCreate;

fn cwd(args: &ToolArgs) -> PathBuf {
    match args.named("cwd") {
        Some(Value::Str(path)) => PathBuf::from(path),
        _ => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    }
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
fn paths(args: &ToolArgs) -> Result<Vec<String>, RuntimeError> {
    match args.named("paths") {
        Some(Value::List(values)) => values
            .iter()
            .map(|value| match value {
                Value::Str(path) if !path.is_empty() && !path.starts_with('-') => Ok(path.clone()),
                Value::Str(_) => Err(RuntimeError::ToolFailed("git.restore: invalid path".into())),
                other => Err(RuntimeError::TypeMismatch {
                    expected: "list<string>".into(),
                    actual: other.kind_name().into(),
                }),
            })
            .collect(),
        Some(other) => Err(RuntimeError::TypeMismatch {
            expected: "list<string>".into(),
            actual: other.kind_name().into(),
        }),
        None => Err(RuntimeError::MissingArg("paths".into())),
    }
}

impl Tool for GitRestore {
    fn name(&self) -> &str {
        "git.restore"
    }
    fn tier(&self) -> Tier {
        Tier::Two
    }
    fn approval_level(&self, _args: &ToolArgs, _ctx: &ToolCtx) -> ApprovalLevel {
        ApprovalLevel::Approve
    }
    fn description(&self) -> Option<&str> {
        Some(
            "Restore explicit paths from HEAD or the index without allowing arbitrary reset/clean operations.",
        )
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","required":["paths"],"properties":{"paths":{"type":"array","items":{"type":"string"}},"mode":{"type":"string","enum":["worktree","staged","both"],"default":"worktree"},"source":{"type":"string"},"cwd":{"type":"string"}}})
    }
    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let cwd = cwd(&args);
            let values = paths(&args)?;
            let mode = match args.named("mode") {
                Some(Value::Str(mode)) => mode.as_str(),
                Some(other) => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "string".into(),
                        actual: other.kind_name().into(),
                    });
                }
                None => "worktree",
            };
            if !matches!(mode, "worktree" | "staged" | "both") {
                return Err(RuntimeError::ToolFailed(
                    "git.restore: mode must be worktree, staged, or both".into(),
                ));
            }
            let source = match args.named("source") {
                Some(Value::Str(source)) => source.as_str(),
                Some(other) => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "string".into(),
                        actual: other.kind_name().into(),
                    });
                }
                None => "HEAD",
            };
            let cli = GitCli::at(&cwd);
            let mut command = vec!["restore"];
            if mode == "staged" || mode == "both" {
                command.push("--staged");
            }
            if mode == "worktree" || mode == "both" {
                command.push("--worktree");
            }
            command.push("--source");
            command.push(source);
            command.push("--");
            let refs: Vec<&str> = values.iter().map(String::as_str).collect();
            command.extend(refs);
            cli.run(&command)
                .map_err(|e| RuntimeError::ToolFailed(format!("git.restore: {e}")))?;
            Ok(Value::Struct(vec![
                ("mode".into(), Value::Str(mode.into())),
                (
                    "paths".into(),
                    Value::List(values.into_iter().map(Value::Str).collect()),
                ),
            ]))
        })
    }
}

impl Tool for GitRevert {
    fn name(&self) -> &str {
        "git.revert"
    }
    fn tier(&self) -> Tier {
        Tier::Three
    }
    fn approval_level(&self, _args: &ToolArgs, _ctx: &ToolCtx) -> ApprovalLevel {
        ApprovalLevel::Dangerous
    }
    fn description(&self) -> Option<&str> {
        Some("Create a signed/hook-aware revert commit for an explicit revision.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","required":["revision"],"properties":{"revision":{"type":"string"},"cwd":{"type":"string"}}})
    }
    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let revision = string_arg(&args, "revision")?;
            if revision.is_empty() || revision.starts_with('-') || revision.contains(' ') {
                return Err(RuntimeError::ToolFailed(
                    "git.revert: invalid revision".into(),
                ));
            }
            GitCli::at(cwd(&args))
                .run(&["revert", "--no-edit", revision])
                .map_err(|e| RuntimeError::ToolFailed(format!("git.revert: {e}")))?;
            Ok(Value::Struct(vec![(
                "revision".into(),
                Value::Str(revision.into()),
            )]))
        })
    }
}

impl Tool for GitTagList {
    fn name(&self) -> &str {
        "git.tag.list"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some("List local Git tags.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"cwd":{"type":"string"}}})
    }
    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let output = GitCli::at(cwd(&args))
                .run(&["tag", "--list", "--format=%(refname:short)"])
                .map_err(|e| RuntimeError::ToolFailed(format!("git.tag.list: {e}")))?;
            Ok(Value::List(
                output
                    .lines()
                    .filter(|line| !line.is_empty())
                    .map(|line| Value::Str(line.to_owned()))
                    .collect(),
            ))
        })
    }
}

impl Tool for GitTagCreate {
    fn name(&self) -> &str {
        "git.tag.create"
    }
    fn tier(&self) -> Tier {
        Tier::Three
    }
    fn approval_level(&self, _args: &ToolArgs, _ctx: &ToolCtx) -> ApprovalLevel {
        ApprovalLevel::Dangerous
    }
    fn description(&self) -> Option<&str> {
        Some("Create a local tag for an explicit revision.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","required":["name"],"properties":{"name":{"type":"string"},"revision":{"type":"string","default":"HEAD"},"cwd":{"type":"string"}}})
    }
    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let name = string_arg(&args, "name")?;
            let revision = match args.named("revision") {
                Some(Value::Str(value)) => value.as_str(),
                Some(other) => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "string".into(),
                        actual: other.kind_name().into(),
                    });
                }
                None => "HEAD",
            };
            if name.is_empty()
                || name.starts_with('-')
                || name.contains("..")
                || name.contains(' ')
                || revision.starts_with('-')
            {
                return Err(RuntimeError::ToolFailed(
                    "git.tag.create: invalid tag or revision".into(),
                ));
            }
            GitCli::at(cwd(&args))
                .run(&["tag", name, revision])
                .map_err(|e| RuntimeError::ToolFailed(format!("git.tag.create: {e}")))?;
            Ok(Value::Struct(vec![
                ("name".into(), Value::Str(name.into())),
                ("revision".into(), Value::Str(revision.into())),
            ]))
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
        run(dir.path(), &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        run(dir.path(), &["add", "."]);
        run(dir.path(), &["commit", "-qm", "initial"]);
        dir
    }

    #[tokio::test]
    async fn restore_tag_and_revert_use_explicit_safe_inputs() {
        let dir = repo();
        std::fs::write(dir.path().join("a.txt"), "changed\n").unwrap();
        let ctx = ToolCtx::default();
        let restore_args = ToolArgs {
            named: vec![
                ("cwd".into(), Value::Str(dir.path().display().to_string())),
                (
                    "paths".into(),
                    Value::List(vec![Value::Str("a.txt".into())]),
                ),
            ],
            ..ToolArgs::default()
        };
        GitRestore.call(restore_args, &ctx).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "one\n"
        );
        let tag_args = ToolArgs {
            named: vec![
                ("cwd".into(), Value::Str(dir.path().display().to_string())),
                ("name".into(), Value::Str("v1".into())),
            ],
            ..ToolArgs::default()
        };
        GitTagCreate.call(tag_args, &ctx).await.unwrap();
        let tags = GitTagList
            .call(
                ToolArgs {
                    named: vec![("cwd".into(), Value::Str(dir.path().display().to_string()))],
                    ..ToolArgs::default()
                },
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            matches!(tags, Value::List(items) if items.iter().any(|item| matches!(item, Value::Str(value) if value == "v1")))
        );
        std::fs::write(dir.path().join("b.txt"), "two\n").unwrap();
        run(dir.path(), &["add", "b.txt"]);
        run(dir.path(), &["commit", "-qm", "second"]);
        let revision = run(dir.path(), &["rev-parse", "HEAD"]);
        GitRevert
            .call(
                ToolArgs {
                    named: vec![
                        ("cwd".into(), Value::Str(dir.path().display().to_string())),
                        ("revision".into(), Value::Str(revision)),
                    ],
                    ..ToolArgs::default()
                },
                &ctx,
            )
            .await
            .unwrap();
        assert!(!dir.path().join("b.txt").exists());
    }

    #[test]
    fn high_risk_operations_require_dangerous_approval() {
        let ctx = ToolCtx::default();
        assert_eq!(
            GitRevert.approval_level(&ToolArgs::default(), &ctx),
            ApprovalLevel::Dangerous
        );
        assert_eq!(
            GitTagCreate.approval_level(&ToolArgs::default(), &ctx),
            ApprovalLevel::Dangerous
        );
    }
}
