use std::sync::Arc;

use atman_runtime::sandbox::{Sandbox, SandboxExec};
use atman_runtime::tool::{Tier, Tool, ToolArgs, ToolCtx};
use atman_runtime::tools::bash::BashExec;
use atman_runtime::value::Value;

#[cfg(target_os = "macos")]
#[tokio::test]
async fn bash_exec_under_sandbox_blocks_write_outside_project_root() {
    let dir = tempfile::tempdir().unwrap();
    let home = std::env::var("HOME").expect("HOME set");
    let outside = std::path::PathBuf::from(home)
        .join(format!(".atman-sandbox-outside-{}", uuid::Uuid::new_v4()));

    let sandbox = SandboxExec::new(dir.path());
    if !sandbox.is_available() {
        eprintln!("sandbox-exec missing; skipping");
        return;
    }
    let mut ctx = ToolCtx::new();
    ctx.sandbox = Some(Arc::new(sandbox));

    let cmd = format!("touch {}", outside.display());
    let args = ToolArgs {
        positional: vec![Value::Str(cmd)],
        named: vec![],
    };
    let v = BashExec.call(args, &ctx).await.unwrap();
    let Value::Struct(fields) = v else {
        panic!("expected struct");
    };
    let exit = fields.iter().find(|(k, _)| k == "exit").unwrap();
    assert!(
        !matches!(exit.1, Value::Int(0)),
        "write outside project root must fail under sandbox"
    );
    assert!(!outside.exists(), "outside path should not exist");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn bash_exec_under_sandbox_allows_write_inside_project_root() {
    let dir = tempfile::tempdir().unwrap();
    // sandbox-exec resolves symlinks; canonicalize so /var and /private/var match.
    let root = dir.path().canonicalize().unwrap();
    let target = root.join("scratch.txt");

    let sandbox = SandboxExec::new(&root);
    if !sandbox.is_available() {
        eprintln!("sandbox-exec missing; skipping");
        return;
    }
    let mut ctx = ToolCtx::new();
    ctx.sandbox = Some(Arc::new(sandbox));

    // cwd must be inside root so sandbox-exec spawns from an allowed path.
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(&root).unwrap();
    let cmd = format!("touch {}", target.display());
    let args = ToolArgs {
        positional: vec![Value::Str(cmd)],
        named: vec![],
    };
    let v = BashExec.call(args, &ctx).await;
    std::env::set_current_dir(prev).unwrap();
    let v = v.unwrap();
    let Value::Struct(fields) = v else {
        panic!("expected struct");
    };
    let stderr = fields
        .iter()
        .find(|(k, _)| k == "stderr")
        .unwrap()
        .1
        .clone();
    let stdout = fields
        .iter()
        .find(|(k, _)| k == "stdout")
        .unwrap()
        .1
        .clone();
    let exit = fields.iter().find(|(k, _)| k == "exit").unwrap();
    assert!(
        matches!(exit.1, Value::Int(0)),
        "write inside project root should succeed under sandbox. exit={:?} stderr={stderr:?} stdout={stdout:?}",
        exit.1
    );
    assert!(target.exists(), "file should be created");
}

#[tokio::test]
async fn bash_exec_without_sandbox_runs_the_command_directly() {
    let ctx = ToolCtx::new();
    let args = ToolArgs {
        positional: vec![Value::Str("echo sandbox-off".into())],
        named: vec![],
    };
    let v = BashExec.call(args, &ctx).await.unwrap();
    let Value::Struct(fields) = v else {
        panic!("expected struct");
    };
    let stdout = fields.iter().find(|(k, _)| k == "stdout").unwrap();
    assert!(matches!(&stdout.1, Value::Str(s) if s.trim() == "sandbox-off"));
    assert_eq!(BashExec.tier(), Tier::Four);
}
