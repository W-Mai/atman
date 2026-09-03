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

#[tokio::test(flavor = "multi_thread")]
async fn interjection_targets_one_run_and_retries_without_duplication() {
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
        "flow wait() -> string {\n    sleep(ms: 2000)\n    return \"done\"\n}\n",
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
    let session_id = created.projection.metadata.id;
    let running = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::StartRun>(
            2,
            &atman_proto::StartRunRequest {
                request_id: Some(atman_proto::RequestId::now()),
                session_id: session_id.clone(),
                flow_path: flow_path.to_string_lossy().into_owned(),
                args: serde_json::Map::new(),
                reasoning: None,
                images: Vec::new(),
            },
        )
        .unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::StartRun>()
    .unwrap();
    let events_path = state
        .sessions_root()
        .join(session_id.to_string())
        .join("events.jsonl");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if std::fs::read_to_string(&events_path)
            .unwrap_or_default()
            .contains("\"type\":\"turn_start\"")
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "run did not start its turn"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let request = atman_proto::InterjectSessionRequest {
        request_id: Some(atman_proto::RequestId::now()),
        session_id: session_id.clone(),
        run_id: running.run_id.clone(),
        text: "stop this run".into(),
        level: atman_proto::InterjectionLevel::HardStop,
        redirect_target: None,
    };
    let committed = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::InterjectSession>(3, &request).unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::InterjectSession>()
    .unwrap();
    let retried = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::InterjectSession>(4, &request).unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::InterjectSession>()
    .unwrap();
    assert_eq!(retried, committed);
    assert_eq!(committed.run_id, running.run_id);
    assert_eq!(committed.state, atman_proto::InterjectionState::Pending);
    assert!(committed.revision.0 > running.revision.0);
    assert!(committed.cursor.0 > running.cursor.0);

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let projection = loop {
        let snapshot = state
            .session_snapshot(&session_id, "local-daemon")
            .await
            .unwrap();
        if snapshot
            .projection
            .interactions
            .interjections
            .iter()
            .any(|item| item.state == atman_proto::InterjectionState::Cancelled)
        {
            break snapshot.projection;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "interjection did not reach a terminal state"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(projection.interactions.interjections.len(), 1);
    let projected = &projection.interactions.interjections[0];
    assert_eq!(projected.id, committed.injection_id);
    assert_eq!(projected.run_id.as_ref(), Some(&running.run_id));
    assert_eq!(projected.state, atman_proto::InterjectionState::Cancelled);
}

#[tokio::test(flavor = "multi_thread")]
async fn start_run_reopens_persisted_session_after_daemon_restart() {
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

    let historical = atman_runtime::Session::open(&data_dir).unwrap();
    let session_id = atman_proto::SessionId(historical.id().0);
    let mut metadata = historical.meta().unwrap_or_default();
    metadata.rebase(&project_root);
    metadata.save(historical.dir()).unwrap();
    historical.append_message(
        atman_runtime::message::Message::user_text(
            atman_runtime::event::TurnId::now(),
            "persisted before restart",
        ),
        None,
    );
    historical.flush_writer().await;
    historical.shutdown().await;
    drop(historical);

    let state = Arc::new(DaemonState::new_with_generation(
        data_dir,
        "after-restart".into(),
    ));
    state.set_launcher(Arc::new(
        RunLauncher::new(project_root, Some(config_dir), None).unwrap(),
    ));
    let started = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::StartRun>(
            1,
            &atman_proto::StartRunRequest {
                request_id: Some(atman_proto::RequestId::now()),
                session_id: session_id.clone(),
                flow_path: repo_root()
                    .join("examples/hello.at")
                    .to_string_lossy()
                    .into_owned(),
                args: serde_json::Map::new(),
                reasoning: None,
                images: Vec::new(),
            },
        )
        .unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::StartRun>()
    .unwrap();
    assert_eq!(started.session_id, session_id);

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while state.has_live_runs(&session_id) {
        assert!(
            std::time::Instant::now() < deadline,
            "reopened run did not finish"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let snapshot = state
        .session_snapshot(&session_id, "local-daemon")
        .await
        .unwrap();
    assert_eq!(snapshot.daemon_generation.0, "after-restart");
    assert!(snapshot.projection.transcript.iter().any(|item| {
        let atman_proto::TranscriptItem::Message { message, .. } = item else {
            return false;
        };
        message.parts.iter().any(|part| {
            matches!(
                part,
                atman_proto::MessagePart::Text { text }
                    if text == "persisted before restart"
            )
        })
    }));
}

#[tokio::test(flavor = "multi_thread")]
async fn send_message_routes_once_and_preserves_the_submitted_text() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&project_root).unwrap();
    std::fs::create_dir_all(config_dir.join("commands")).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "[storage]\nscope = \"global\"\n",
    )
    .unwrap();
    std::fs::write(
        config_dir.join("routes.at"),
        "default_route { flow: echo }\n",
    )
    .unwrap();
    std::fs::write(
        config_dir.join("commands/echo.at"),
        "flow helper(message: string) -> string { return \"wrong entry\" }\nflow echo(message: string) -> string { return message }\n",
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
    let request = atman_proto::SendMessageRequest {
        request_id: Some(atman_proto::RequestId::now()),
        session_id: created.projection.metadata.id,
        text: "hello from client".into(),
        reasoning: None,
        images: Vec::new(),
    };
    let sent = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::SendMessage>(2, &request).unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::SendMessage>()
    .unwrap();
    let retry = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::SendMessage>(3, &request).unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::SendMessage>()
    .unwrap();
    assert_eq!(retry.run_id, sent.run_id);

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while state.has_live_runs(&sent.session_id) {
        assert!(
            std::time::Instant::now() < deadline,
            "message run did not finish"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let events = std::fs::read_to_string(
        state
            .sessions_root()
            .join(sent.session_id.to_string())
            .join("events.jsonl"),
    )
    .unwrap()
    .lines()
    .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    .collect::<Vec<_>>();
    let user_messages = events
        .iter()
        .filter(|event| event["type"] == "user_msg")
        .collect::<Vec<_>>();
    assert_eq!(user_messages.len(), 1);
    assert_eq!(
        user_messages[0]["message"]["parts"][0]["text"],
        "hello from client"
    );
    assert!(events.iter().any(|event| {
        event["type"] == "flow_start"
            && event["run_id"] == sent.run_id.to_string()
            && event["flow_name"] == "echo"
    }));
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}
