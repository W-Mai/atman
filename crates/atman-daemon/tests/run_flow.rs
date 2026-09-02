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
    let launcher = RunLauncher::new(project_root.clone(), Some(config_dir), None).unwrap();

    let spawned = launcher
        .spawn(
            state.clone(),
            repo_root().join("examples/hello.at").to_str().unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while state.has_live_runs(&spawned.session_id) {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for daemon run to finish"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

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

    let launcher =
        Arc::new(RunLauncher::new(std::env::current_dir().unwrap(), None, None).unwrap());
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
        serde_json::json!({
            "flow_path": flow_path.to_string_lossy(),
            "reasoning": "high@pro",
            "images": [{
                "data_base64": "iVBORw0KGgo=",
                "name": "daemon-input.png"
            }]
        }),
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
            let persisted = std::fs::read_to_string(&events_path).unwrap_or_default();
            panic!(
                "timed out waiting for flow_end in {}; persisted events:\n{}",
                events_path.display(),
                persisted
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let events = std::fs::read_to_string(&events_path).unwrap();
    let image_source = events
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|event| event["type"] == "user_msg")
        .and_then(|event| {
            event["message"]["parts"]
                .as_array()?
                .iter()
                .find(|part| part["type"] == "image")
                .map(|part| part["source"].clone())
        })
        .expect("daemon user message image");
    assert_eq!(image_source["data"]["kind"], "artifact");
    let artifact_path = image_source["data"]["path"].as_str().unwrap();
    assert!(std::path::Path::new(artifact_path).is_file());

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
