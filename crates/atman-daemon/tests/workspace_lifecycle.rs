use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use atman_daemon::{DaemonState, dispatch, run::RunLauncher};
use atman_proto::{JsonRpcRequest, SessionStatus, methods};
use atman_runtime::git_workspace::{WorkspaceManager, WorkspaceState};
use serde_json::{Value, json};

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
        data_dir.clone(),
        DAEMON_GENERATION.into(),
    ));
    let launcher = RunLauncher::new(project_root.clone(), Some(config_dir), None).unwrap();
    let spawned = launcher
        .spawn(state.clone(), root_path.to_str().unwrap(), Vec::new())
        .await
        .unwrap();

    let flow = json!({"session_id": spawned.session_id, "instance_id": spawned.instance_id});
    let completed = wait_completion(&state, &flow, None).await;
    assert_eq!(
        completed["last_completion"]["result"]["value"]["value"],
        "done"
    );
    let listed = state.list_sessions().await.unwrap();
    assert_eq!(
        listed
            .iter()
            .find(|s| s.id == spawned.session_id)
            .unwrap()
            .status,
        SessionStatus::Idle
    );

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
    let child_instance_id = record.owner_flow.as_deref().expect("workspace owner flow");
    uuid::Uuid::parse_str(child_instance_id)
        .expect("owner flow is a runtime-generated FlowInstanceId");
    assert_ne!(child_instance_id, spawned.instance_id.to_string());
    assert_eq!(record.lifecycle_state(), WorkspaceState::Bound);
    assert!(record.is_instance_bound());
    assert!(
        record.lease.is_none(),
        "persistent workspace must not depend on a process lease"
    );
    assert!(record.worktree_path.exists(), "dirty worktree was removed");
    assert_eq!(
        std::fs::read_to_string(record.worktree_path.join("child.txt")).unwrap(),
        "written in managed workspace\n"
    );

    let child = json!({"session_id": spawned.session_id, "instance_id": child_instance_id});
    let child_before = wait_completion(&state, &child, None).await;
    assert_eq!(
        child_before["last_completion"]["result"]["value"]["value"],
        "done"
    );
    assert!(
        !state
            .sessions_root()
            .join(spawned.session_id.to_string())
            .join("events.jsonl")
            .exists()
    );
    let original = rpc(
        &state,
        methods::FLOW_EVENTS,
        json!({"flow": child, "limit": 100}),
    )
    .await;
    state.shutdown().await.unwrap();
    assert!(record.worktree_path.exists());
    std::fs::remove_file(&child_path).unwrap();
    std::fs::remove_file(&root_path).unwrap();
    let reopened = Arc::new(DaemonState::new(data_dir));
    rpc(
        &reopened,
        methods::SESSION_OPEN,
        json!({"session_id": spawned.session_id}),
    )
    .await;
    assert_eq!(
        rpc(
            &reopened,
            methods::FLOW_EVENTS,
            json!({"flow": child, "limit": 100})
        )
        .await,
        original
    );
    rpc(&reopened, methods::FLOW_BIND, json!({"flow": child})).await;
    rpc(&reopened, methods::FLOW_INVOKE, json!({"flow": child, "request_id": uuid::Uuid::now_v7(), "arguments": {}, "invocation": {}})).await;
    let second = wait_completion(
        &reopened,
        &child,
        Some(&child_before["last_completion"]["lease"]["turn"]),
    )
    .await;
    assert_eq!(
        second["last_completion"]["result"]["value"]["value"],
        "done"
    );
    assert_ne!(
        second["last_completion"]["lease"]["run"],
        child_before["last_completion"]["lease"]["run"]
    );
    assert_eq!(
        WorkspaceManager::at(&project_root, None)
            .unwrap()
            .get(&record.id)
            .unwrap(),
        *record
    );
    assert!(!project_root.join("child.txt").exists());
    reopened.shutdown().await.unwrap();
}

async fn rpc(state: &Arc<DaemonState>, method: &str, params: Value) -> Value {
    let response = dispatch(state.clone(), JsonRpcRequest::new(1, method, params)).await;
    assert!(response.error.is_none(), "{method}: {:?}", response.error);
    response.result.unwrap()
}

async fn wait_completion(
    state: &Arc<DaemonState>,
    flow: &Value,
    previous: Option<&Value>,
) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let view = rpc(state, methods::FLOW_GET, json!({"flow": flow})).await;
            let completion = &view["last_completion"];
            if !completion.is_null() && previous != Some(&completion["lease"]["turn"]) {
                assert_eq!(completion["result"]["kind"], "returned", "{view}");
                return view;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("persistent entry should complete")
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
