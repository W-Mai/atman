use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use atman_daemon::{DaemonState, run::RunLauncher};
use atman_proto::SessionStatus;
use atman_runtime::git_workspace::{WorkspaceManager, WorkspaceState};

const DAEMON_GENERATION: &str = "daemon-s7-generation";

#[tokio::test(flavor = "multi_thread")]
async fn launcher_runs_child_flow_in_dirty_managed_workspace() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&project_root).unwrap();
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "[trust]\nmode = \"reckless\"\n",
    )
    .unwrap();
    init_repo(&project_root);

    let child_path = project_root.join("child.at");
    std::fs::write(
        &child_path,
        r#"flow child() -> string {
    fs.write(path: "child.txt", content: "written in managed workspace\n")
    return "done"
}
"#,
    )
    .unwrap();
    let child_ref = format!("{}@child", child_path.display());
    let root_path = project_root.join("root.at");
    std::fs::write(
        &root_path,
        format!(
            r#"flow root() -> string {{
    inventory = flow.instances()
    return flow.spawn(flow: {child_ref:?}, spawn_token: inventory.spawn_token, async: false, workspace: "auto")
}}
"#
        ),
    )
    .unwrap();

    let state = Arc::new(DaemonState::new_with_generation(
        data_dir,
        DAEMON_GENERATION.into(),
    ));
    let launcher = RunLauncher::new(project_root.clone(), Some(config_dir), None).unwrap();
    let spawned = launcher
        .spawn(state.clone(), root_path.to_str().unwrap(), Vec::new())
        .await
        .unwrap();

    wait_for_finished_session(&state, &spawned.session_id).await;

    assert!(
        !project_root.join("child.txt").exists(),
        "relative child write escaped into the main checkout"
    );
    let records = WorkspaceManager::at(&project_root, None)
        .unwrap()
        .list()
        .unwrap();
    assert_eq!(records.len(), 1, "expected exactly one child workspace");
    let record = &records[0];
    assert_eq!(
        record.owner_session.as_deref(),
        Some(spawned.session_id.to_string().as_str())
    );
    let child_run_id = record.owner_flow.as_deref().expect("workspace owner flow");
    uuid::Uuid::parse_str(child_run_id).expect("owner flow is a runtime-generated FlowRunId");
    assert_ne!(child_run_id, spawned.run_id.to_string());
    assert_eq!(record.lifecycle_state(), WorkspaceState::Dirty);
    assert!(
        record.lease.is_none(),
        "terminal workspace kept an active lease"
    );
    assert!(record.worktree_path.exists(), "dirty worktree was removed");
    assert_eq!(
        std::fs::read_to_string(record.worktree_path.join("child.txt")).unwrap(),
        "written in managed workspace\n"
    );

    let events_path = state
        .sessions_root()
        .join(spawned.session_id.to_string())
        .join("events.jsonl");
    let events = read_events(&events_path);
    assert!(events.iter().any(|event| {
        event["type"] == "flow_end"
            && event["run_id"] == spawned.run_id.to_string()
            && event["status"]["kind"] == "ok"
    }));
    assert!(events.iter().any(|event| {
        event["type"] == "flow_start"
            && event["run_id"] == child_run_id
            && event["parent_run_id"] == spawned.run_id.to_string()
            && event["spawned"] == true
    }));
    assert!(events.iter().any(|event| {
        event["type"] == "flow_end"
            && event["run_id"] == child_run_id
            && event["status"]["kind"] == "ok"
    }));
    assert!(events.iter().any(|event| {
        event["type"] == "workspace_lifecycle"
            && event["run_id"] == child_run_id
            && event["workspace_id"] == record.id
            && event["path"] == record.worktree_path.display().to_string()
            && event["state"] == "dirty"
            && event.get("cleanup_error").is_none()
    }));
}

async fn wait_for_finished_session(state: &DaemonState, session_id: &atman_proto::SessionId) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let finished = state
            .list_sessions()
            .unwrap()
            .into_iter()
            .find(|session| &session.id == session_id)
            .is_some_and(|session| session.status == SessionStatus::Finished);
        if finished {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for launcher");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn read_events(path: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn init_repo(path: &Path) {
    git(path, &["init", "-q"]);
    git(path, &["config", "user.name", "Atman Test"]);
    git(path, &["config", "user.email", "atman@example.invalid"]);
    git(path, &["config", "commit.gpgsign", "false"]);
    std::fs::write(path.join("README.md"), "committed\n").unwrap();
    git(path, &["add", "README.md"]);
    git(path, &["commit", "-q", "-m", "initial"]);
}

fn git(cwd: &Path, args: &[&str]) {
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
}
