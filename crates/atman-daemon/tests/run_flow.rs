use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use atman_daemon::{DaemonState, dispatch, run::RunLauncher};
use atman_proto::{JsonRpcRequest, methods};

#[tokio::test(flavor = "multi_thread")]
async fn launcher_uses_injected_config_and_data_dirs_for_project_scope() {
    let tmp = tempfile::tempdir().unwrap();
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&project_root).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "[storage]\nscope = \"global\"\n",
    )
    .unwrap();
    let state = Arc::new(DaemonState::new(data_dir.clone()));
    let launcher = RunLauncher {
        project_root: project_root.clone(),
        config_dir: Some(config_dir),
        home_dir: None,
    };

    launcher
        .spawn(
            state,
            repo_root().join("examples/hello.at").to_str().unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();

    let scope = data_dir
        .join("projects")
        .join(atman_runtime::session_meta::fingerprint_from_root(
            &project_root,
        ));
    assert!(
        scope.is_dir(),
        "expected injected scope {}",
        scope.display()
    );
    assert!(!project_root.join(".atman").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn run_flow_end_to_end_writes_events_and_appears_in_list_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));

    let launcher = Arc::new(RunLauncher {
        project_root: std::env::current_dir().unwrap(),
        config_dir: None,
        home_dir: None,
    });
    state.set_launcher(launcher);

    let flow_path = repo_root().join("examples/hello.at");
    assert!(
        flow_path.exists(),
        "expected {} to exist",
        flow_path.display()
    );

    let req = JsonRpcRequest::new(
        1,
        methods::RUN_FLOW,
        serde_json::json!({"flow_path": flow_path.to_string_lossy()}),
    );
    let resp = dispatch(state.clone(), req).await;
    let result = resp.result.expect("run_flow ok");
    let sid_val = result["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();
    assert!(!sid_val.is_empty());

    let sessions_root = tmp.path().join("sessions");
    let events_path = sessions_root.join(&sid_val).join("events.jsonl");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if events_path.exists() {
            let text = std::fs::read_to_string(&events_path).unwrap_or_default();
            if text.contains("flow_end") {
                break;
            }
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "timed out waiting for flow_end in {}",
                events_path.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let list_req = JsonRpcRequest::new(2, methods::LIST_SESSIONS, serde_json::json!({}));
    let list_resp = dispatch(state, list_req).await;
    let arr = list_resp.result.unwrap();
    let arr = arr.as_array().unwrap();
    assert!(
        arr.iter().any(|s| s["id"].as_str() == Some(&sid_val)),
        "expected session {sid_val} in list_sessions: {arr:?}"
    );
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}
