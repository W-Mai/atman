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

    fn invocation_provenance(
        &self,
        args: &ToolArgs,
        ctx: &ToolCtx,
    ) -> Result<crate::permission::ResourceProvenance, RuntimeError> {
        Ok(crate::permission::ResourceProvenance::for_ctx(ctx)
            .with_cwd(ctx, extract_optional_path(args, "cwd").as_deref())?
            .with_risk(crate::trust::RiskKind::ProcessSpawn))
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let explicit_cwd = extract_optional_path(&args, "cwd");
            let cwd = ctx.resolve_cwd(explicit_cwd.as_deref())?;
            crate::fs_access::authorize_write(ctx, &cwd, self.name(), false).await?;
            let framework_override = extract_optional_string(&args, "framework");
            let scope = extract_optional_string(&args, "scope");
            let timeout_ms = extract_optional_int(&args, "timeout_ms").unwrap_or(300_000) as u64;
            let tail_lines = extract_optional_int(&args, "tail_lines").unwrap_or(80) as usize;

            let framework = match framework_override {
                Some(name) => name,
                None => detect_framework(&cwd)?,
            };
            let cmd = build_command(&framework, scope.as_deref())?;
            let authorization = ctx.invocation_authorization_for("test.run")?;
            let cmd_refs = cmd.iter().map(String::as_str).collect::<Vec<_>>();

            let start = Instant::now();
            let spawn_start = Instant::now();
            let output_fut = async {
                match authorization.execution_boundary() {
                    crate::permission::ExecutionBoundary::Sandboxed => {
                        let sandbox = ctx.sandbox.as_ref().ok_or_else(|| {
                            RuntimeError::ToolFailed(
                                "test.run: sandbox unavailable for controlled execution".into(),
                            )
                        })?;
                        sandbox.spawn(&cmd_refs, &[], &cwd, authorization).await
                    }
                    crate::permission::ExecutionBoundary::Direct => {
                        let mut child = tokio::process::Command::new(&cmd[0]);
                        child.args(&cmd[1..]).current_dir(&cwd).kill_on_drop(true);
                        child.output().await.map_err(|error| {
                            RuntimeError::ToolFailed(format!(
                                "test.run spawn `{}`: {error}",
                                cmd.join(" ")
                            ))
                        })
                    }
                }
            };
            let output = match tokio::time::timeout(Duration::from_millis(timeout_ms), output_fut)
                .await
            {
                Ok(Ok(out)) => out,
                Ok(Err(error)) => return Err(error),
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

    struct RecordingTestSandbox {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl crate::sandbox::Sandbox for RecordingTestSandbox {
        fn spawn<'a>(
            &'a self,
            _cmd: &'a [&'a str],
            _env: &'a [(String, String)],
            _cwd: &'a Path,
            _authorization: &'a crate::permission::InvocationAuthorization,
        ) -> crate::tool::BoxFut<'a, Result<std::process::Output, RuntimeError>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async {
                std::process::Command::new("true")
                    .output()
                    .map_err(|error| RuntimeError::ToolFailed(error.to_string()))
            })
        }

        fn prepare_background(
            &self,
            _cmd: &[&str],
            _env: &[(String, String)],
            _cwd: &Path,
            _authorization: &crate::permission::InvocationAuthorization,
        ) -> Result<Box<dyn crate::sandbox::BackgroundLauncher>, crate::sandbox::SandboxLaunchError>
        {
            Err(crate::sandbox::SandboxLaunchError::Runtime(
                RuntimeError::ToolFailed("unsupported".into()),
            ))
        }

        fn spawn_pty<'a>(
            &'a self,
            _cmd: &'a [&'a str],
            _env: &'a [(String, String)],
            _cwd: &'a Path,
            _pty_size: portable_pty::PtySize,
            _authorization: &'a crate::permission::InvocationAuthorization,
        ) -> crate::tool::BoxFut<
            'a,
            Result<crate::sandbox::PtySpawnResult, crate::sandbox::SandboxLaunchError>,
        > {
            Box::pin(async {
                Err(crate::sandbox::SandboxLaunchError::Runtime(
                    RuntimeError::ToolFailed("unsupported".into()),
                ))
            })
        }

        fn is_available(&self) -> bool {
            true
        }

        fn kind(&self) -> &'static str {
            "test"
        }
    }

    fn authorize(
        ctx: ToolCtx,
        execution_boundary: crate::permission::ExecutionBoundary,
    ) -> ToolCtx {
        ctx.authorized_for(crate::permission::InvocationAuthorization::new(
            crate::permission::PermissionRequestId::now(),
            "test-call",
            "test.run",
            crate::permission::ResourceProvenance::none(),
            execution_boundary,
        ))
    }

    #[test]
    fn provenance_uses_cwd_and_reports_process_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolCtx::default();
        let args = ToolArgs {
            named: vec![
                ("cwd".into(), Value::Str(dir.path().display().to_string())),
                ("scope".into(), Value::Str("integration".into())),
            ],
            ..ToolArgs::default()
        };
        let provenance = TestRun.invocation_provenance(&args, &ctx).unwrap();
        let cwd = provenance.cwd.expect("cwd recorded");
        assert_eq!(
            std::fs::canonicalize(&cwd).unwrap(),
            std::fs::canonicalize(dir.path()).unwrap()
        );
        assert_eq!(provenance.path, None);
        assert!(
            provenance
                .risks
                .contains(&crate::trust::RiskKind::ProcessSpawn)
        );
    }

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
        let ctx = authorize(ToolCtx::new(), crate::permission::ExecutionBoundary::Direct);
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

    fn managed_ctx(workspace: &Path) -> ToolCtx {
        authorize(
            ToolCtx::new()
                .with_fs_access(crate::fs_access::FsAccessPolicy::workspace_write(
                    workspace.to_path_buf(),
                ))
                .with_workspace(crate::git_workspace::WorkspaceBinding {
                    workspace_id: "test".into(),
                    repository_root: workspace.to_path_buf(),
                    path: workspace.to_path_buf(),
                    branch: None,
                }),
            crate::permission::ExecutionBoundary::Direct,
        )
    }

    fn run_args(cwd: &Path, framework: &str) -> ToolArgs {
        ToolArgs {
            positional: vec![],
            named: vec![
                ("framework".into(), Value::Str(framework.into())),
                ("cwd".into(), Value::Path(cwd.to_path_buf())),
                ("timeout_ms".into(), Value::Int(30_000)),
            ],
        }
    }

    #[tokio::test]
    async fn managed_test_run_rejects_external_cwd_before_command_construction() {
        let workspace = tempfile::tempdir().unwrap();
        let external = Path::new(env!("CARGO_MANIFEST_DIR"));
        let error = TestRun
            .call(
                run_args(external, "must-not-be-parsed"),
                &managed_ctx(workspace.path()),
            )
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("outside workspace"), "{message}");
        assert!(!message.contains("unknown framework"), "{message}");
    }

    #[tokio::test]
    async fn managed_test_run_allows_temp_cwd() {
        let workspace = tempfile::tempdir().unwrap();
        let external_temp = tempfile::tempdir().unwrap();
        std::fs::write(
            external_temp.path().join("Cargo.toml"),
            "[package]\nname='r4-temp'\nversion='0.0.0'\n",
        )
        .unwrap();
        std::fs::create_dir(external_temp.path().join("src")).unwrap();
        std::fs::write(external_temp.path().join("src/lib.rs"), "").unwrap();
        let result = TestRun
            .call(
                run_args(external_temp.path(), "cargo"),
                &managed_ctx(workspace.path()),
            )
            .await
            .unwrap();
        assert!(matches!(result.field("exit"), Some(Value::Int(0))));
    }

    #[tokio::test]
    async fn enforced_authorization_uses_sandbox_runner() {
        let workspace = tempfile::tempdir().unwrap();
        let sandbox = std::sync::Arc::new(RecordingTestSandbox {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let ctx = authorize(
            managed_ctx(workspace.path()).with_sandbox(sandbox.clone()),
            crate::permission::ExecutionBoundary::Sandboxed,
        );

        let result = TestRun
            .call(run_args(workspace.path(), "cargo"), &ctx)
            .await
            .unwrap();

        assert!(matches!(result.field("exit"), Some(Value::Int(0))));
        assert_eq!(sandbox.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn managed_test_run_allows_external_cwd_under_full_access() {
        let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        std::fs::create_dir_all(&fixture_root).unwrap();
        let fixture = tempfile::Builder::new()
            .prefix("r4-test-run-")
            .tempdir_in(fixture_root)
            .unwrap();
        std::fs::create_dir_all(fixture.path().join("src")).unwrap();
        std::fs::write(
            fixture.path().join("Cargo.toml"),
            "[package]\nname='r4-full'\nversion='0.0.0'\n\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(fixture.path().join("src/lib.rs"), "").unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let ctx = managed_ctx(workspace.path())
            .with_fs_access(crate::fs_access::FsAccessPolicy::danger_full_access());
        let result = TestRun
            .call(run_args(fixture.path(), "cargo"), &ctx)
            .await
            .unwrap();
        assert!(matches!(result.field("exit"), Some(Value::Int(0))));
    }

    #[test]
    fn tail_of_returns_last_n_lines() {
        let s = "a\nb\nc\nd\ne";
        assert_eq!(tail_of(s, 3), "c\nd\ne");
        assert_eq!(tail_of(s, 10), s);
        assert_eq!(tail_of("", 3), "");
    }
}
