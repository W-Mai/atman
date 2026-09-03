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
    let config_dir = tmp.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "[storage]\nscope = \"global\"\n",
    )
    .unwrap();

    let launcher = Arc::new(
        RunLauncher::new(std::env::current_dir().unwrap(), Some(config_dir), None).unwrap(),
    );
    state.set_launcher(launcher);

    let flow_path = repo_root().join("examples/hello.at");
    assert!(
        flow_path.exists(),
        "expected {} to exist",
        flow_path.display()
    );

    let command = atman_proto::RunFlowRequest {
        request_id: Some(atman_proto::RequestId::now()),
        flow_path: flow_path.to_string_lossy().into_owned(),
        args: serde_json::Map::new(),
        reasoning: Some("high@pro".into()),
        images: vec![atman_proto::InlineImage {
            data_base64: "iVBORw0KGgo=".into(),
            name: Some("daemon-input.png".into()),
        }],
    };
    let req = JsonRpcRequest::for_method::<atman_proto::rpc::RunFlow>(1, &command).unwrap();
    let resp = dispatch(state.clone(), req).await;
    let result = resp
        .into_method_output::<atman_proto::rpc::RunFlow>()
        .expect("run_flow ok");
    let retry = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::RunFlow>(2, &command).unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::RunFlow>()
    .expect("run_flow retry ok");
    assert_eq!(retry.session_id, result.session_id);
    assert_eq!(retry.run_id, result.run_id);
    let sid_val = result.session_id.to_string();
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

#[tokio::test(flavor = "multi_thread")]
async fn start_run_reuses_the_session_and_cancels_the_registered_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&project_root).unwrap();
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "[storage]\nscope = \"global\"\n",
    )
    .unwrap();
    let flow_path = project_root.join("wait.at");
    std::fs::write(
        &flow_path,
        "flow wait() -> string {\n    sleep(ms: 500)\n    return \"done\"\n}\n",
    )
    .unwrap();
    let state = Arc::new(DaemonState::new(data_dir));
    state.set_launcher(Arc::new(
        RunLauncher::new(project_root.clone(), Some(config_dir), None).unwrap(),
    ));
    let created = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::CreateSession>(
            1,
            &atman_proto::CreateSessionRequest {
                request_id: Some(atman_proto::RequestId::now()),
                project_root: Some(project_root.to_string_lossy().into_owned()),
                title: None,
            },
        )
        .unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::CreateSession>()
    .unwrap();
    let start = atman_proto::StartRunRequest {
        request_id: Some(atman_proto::RequestId::now()),
        session_id: created.projection.metadata.id.clone(),
        flow_path: flow_path.to_string_lossy().into_owned(),
        args: serde_json::Map::new(),
        reasoning: None,
        images: Vec::new(),
    };
    let running = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::StartRun>(2, &start).unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::StartRun>()
    .unwrap();
    let retry = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::StartRun>(3, &start).unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::StartRun>()
    .unwrap();
    assert_eq!(retry.run_id, running.run_id);
    assert_eq!(retry.session_id, created.projection.metadata.id);

    let concurrent = atman_proto::StartRunRequest {
        request_id: Some(atman_proto::RequestId::now()),
        ..start.clone()
    };
    let error = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::StartRun>(4, &concurrent).unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::StartRun>()
    .unwrap_err();
    assert!(error.message.contains("active root run"));

    let cancelled = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::CancelRun>(
            5,
            &atman_proto::CancelRunRequest {
                request_id: Some(atman_proto::RequestId::now()),
                run_id: running.run_id.clone(),
            },
        )
        .unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::CancelRun>()
    .unwrap();
    assert!(cancelled.cancelled);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while state.has_live_runs(&running.session_id) {
        assert!(std::time::Instant::now() < deadline, "run did not cancel");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let events = std::fs::read_to_string(
        state
            .sessions_root()
            .join(running.session_id.to_string())
            .join("events.jsonl"),
    )
    .unwrap();
    assert!(
        events.contains("\"status\":{\"kind\":\"cancelled\"}"),
        "{events}"
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
