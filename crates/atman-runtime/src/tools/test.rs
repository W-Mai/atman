use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::error::RuntimeError;
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

pub struct TestRun;

impl Tool for TestRun {
    fn name(&self) -> &str {
        "test.run"
    }

    fn tier(&self) -> Tier {
        Tier::Two
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Run tests with auto-detected framework (cargo/npm/pytest/go). Returns exit code, stdout/stderr tail, duration, timed_out flag. Use scope to filter (e.g. scope: 'integration' for cargo).",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "cwd": {"type": "string", "description": "Working directory to run tests in. Defaults to the current process directory."},
                "framework": {"type": "string", "enum": ["cargo", "npm", "pytest", "go"], "description": "Optional framework override. Auto-detected from project files when omitted."},
                "scope": {"type": "string", "description": "Optional framework-specific test filter or path, such as 'integration' for cargo."},
                "timeout_ms": {"type": "integer", "default": 300000, "description": "Maximum runtime in milliseconds before returning timed_out=true."},
                "tail_lines": {"type": "integer", "default": 80, "description": "Number of stdout/stderr lines to include from the end of each stream."}
            }
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let cwd = extract_optional_path(&args, "cwd")
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            let framework_override = extract_optional_string(&args, "framework");
            let scope = extract_optional_string(&args, "scope");
            let timeout_ms = extract_optional_int(&args, "timeout_ms").unwrap_or(300_000) as u64;
            let tail_lines = extract_optional_int(&args, "tail_lines").unwrap_or(80) as usize;

            let framework = match framework_override {
                Some(name) => name,
                None => detect_framework(&cwd)?,
            };
            let cmd = build_command(&framework, scope.as_deref())?;

            let start = Instant::now();
            let mut child = tokio::process::Command::new(&cmd[0]);
            child.args(&cmd[1..]).current_dir(&cwd);
            let spawn_start = Instant::now();
            let output_fut = child.output();
            let output = match tokio::time::timeout(Duration::from_millis(timeout_ms), output_fut)
                .await
            {
                Ok(Ok(out)) => out,
                Ok(Err(e)) => {
                    return Err(RuntimeError::ToolFailed(format!(
                        "test.run spawn `{}`: {e}",
                        cmd.join(" ")
                    )));
                }
                Err(_) => {
                    return Ok(Value::Struct(vec![
                        ("exit".into(), Value::Int(-1)),
                        ("framework".into(), Value::Str(framework)),
                        ("stdout_tail".into(), Value::Str(String::new())),
                        (
                            "stderr_tail".into(),
                            Value::Str(format!("[atman] test.run timeout after {timeout_ms}ms")),
                        ),
                        (
                            "duration_ms".into(),
                            Value::Int(spawn_start.elapsed().as_millis() as i64),
                        ),
                        ("timed_out".into(), Value::Bool(true)),
                        ("cmd".into(), Value::Str(cmd.join(" "))),
                    ]));
                }
            };
            let duration_ms = start.elapsed().as_millis() as i64;
            let exit = output.status.code().unwrap_or(-1) as i64;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            Ok(Value::Struct(vec![
                ("exit".into(), Value::Int(exit)),
                ("framework".into(), Value::Str(framework)),
                (
                    "stdout_tail".into(),
                    Value::Str(tail_of(&stdout, tail_lines)),
                ),
                (
                    "stderr_tail".into(),
                    Value::Str(tail_of(&stderr, tail_lines)),
                ),
                ("duration_ms".into(), Value::Int(duration_ms)),
                ("timed_out".into(), Value::Bool(false)),
                ("cmd".into(), Value::Str(cmd.join(" "))),
            ]))
        })
    }
}

fn detect_framework(cwd: &Path) -> Result<String, RuntimeError> {
    if cwd.join("Cargo.toml").exists() {
        return Ok("cargo".into());
    }
    if cwd.join("package.json").exists() {
        return Ok("npm".into());
    }
    if cwd.join("pyproject.toml").exists() || cwd.join("pytest.ini").exists() {
        return Ok("pytest".into());
    }
    if cwd.join("go.mod").exists() {
        return Ok("go".into());
    }
    Err(RuntimeError::ToolFailed(format!(
        "test.run: no known test framework detected in {} (looked for Cargo.toml / package.json / pyproject.toml / go.mod). Pass `framework:` to override.",
        cwd.display()
    )))
}

fn build_command(framework: &str, scope: Option<&str>) -> Result<Vec<String>, RuntimeError> {
    Ok(match framework {
        "cargo" => match scope {
            Some(s) => vec!["cargo".into(), "test".into(), "--".into(), s.into()],
            None => vec!["cargo".into(), "test".into()],
        },
        "npm" => match scope {
            Some(s) => vec!["npm".into(), "test".into(), "--".into(), s.into()],
            None => vec!["npm".into(), "test".into()],
        },
        "pytest" => match scope {
            Some(s) => vec!["pytest".into(), s.into()],
            None => vec!["pytest".into()],
        },
        "go" => match scope {
            Some(s) => vec!["go".into(), "test".into(), s.into()],
            None => vec!["go".into(), "test".into(), "./...".into()],
        },
        other => {
            return Err(RuntimeError::ToolFailed(format!(
                "test.run: unknown framework `{other}` (want cargo | npm | pytest | go)"
            )));
        }
    })
}

fn tail_of(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

fn extract_optional_string(args: &ToolArgs, name: &str) -> Option<String> {
    match args.named(name)? {
        Value::Str(s) => Some(s.clone()),
        _ => None,
    }
}

fn extract_optional_int(args: &ToolArgs, name: &str) -> Option<i64> {
    match args.named(name)? {
        Value::Int(n) => Some(*n),
        _ => None,
    }
}

fn extract_optional_path(args: &ToolArgs, name: &str) -> Option<PathBuf> {
    match args.named(name)? {
        Value::Path(p) => Some(p.clone()),
        Value::Str(s) => Some(PathBuf::from(s)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_cargo_from_cargo_toml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        assert_eq!(detect_framework(dir.path()).unwrap(), "cargo");
    }

    #[test]
    fn detect_npm_from_package_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();
        assert_eq!(detect_framework(dir.path()).unwrap(), "npm");
    }

    #[test]
    fn detect_pytest_from_pyproject_toml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pyproject.toml"), "").unwrap();
        assert_eq!(detect_framework(dir.path()).unwrap(), "pytest");
    }

    #[test]
    fn detect_pytest_from_pytest_ini() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pytest.ini"), "").unwrap();
        assert_eq!(detect_framework(dir.path()).unwrap(), "pytest");
    }

    #[test]
    fn detect_go_from_go_mod() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("go.mod"), "module x").unwrap();
        assert_eq!(detect_framework(dir.path()).unwrap(), "go");
    }

    #[test]
    fn detect_cargo_wins_over_go_when_both_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(dir.path().join("go.mod"), "module x").unwrap();
        assert_eq!(detect_framework(dir.path()).unwrap(), "cargo");
    }

    #[test]
    fn detect_errors_when_no_markers() {
        let dir = tempfile::tempdir().unwrap();
        let err = detect_framework(dir.path()).unwrap_err();
        assert!(format!("{err}").contains("no known test framework"));
    }

    #[test]
    fn build_cargo_command_with_scope_appends_after_dash_dash() {
        let cmd = build_command("cargo", Some("integration")).unwrap();
        assert_eq!(cmd, vec!["cargo", "test", "--", "integration"]);
    }

    #[test]
    fn build_go_command_defaults_to_all_packages() {
        let cmd = build_command("go", None).unwrap();
        assert_eq!(cmd, vec!["go", "test", "./..."]);
    }

    #[test]
    fn build_unknown_framework_errors() {
        let err = build_command("mocha", None).unwrap_err();
        assert!(format!("{err}").contains("unknown framework"));
    }

    #[tokio::test]
    async fn test_run_returns_all_structured_fields() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        let tool = TestRun;
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![],
            named: vec![
                ("framework".into(), Value::Str("cargo".into())),
                ("cwd".into(), Value::Path(dir.path().to_path_buf())),
                ("timeout_ms".into(), Value::Int(30_000)),
            ],
        };
        let v = tool.call(args, &ctx).await.unwrap();
        let Value::Struct(fields) = v else {
            panic!("expected struct");
        };
        let f = |k: &str| fields.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert!(matches!(f("framework"), Some(Value::Str(s)) if s == "cargo"));
        assert!(matches!(f("exit"), Some(Value::Int(_))));
        assert!(matches!(f("duration_ms"), Some(Value::Int(_))));
        assert!(matches!(f("timed_out"), Some(Value::Bool(_))));
        assert!(matches!(f("cmd"), Some(Value::Str(s)) if s.starts_with("cargo test")));
    }

    #[test]
    fn tail_of_returns_last_n_lines() {
        let s = "a\nb\nc\nd\ne";
        assert_eq!(tail_of(s, 3), "c\nd\ne");
        assert_eq!(tail_of(s, 10), s);
        assert_eq!(tail_of("", 3), "");
    }
}
