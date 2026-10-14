use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use atman_daemon::{DaemonState, dispatch, run::RunLauncher};
use atman_proto::{JsonRpcRequest, methods};

#[tokio::test(flavor = "multi_thread")]
async fn goal_driver_finishes_turns_and_resumes_without_replacing_the_session() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config");
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("config.toml"), "[trust]\nmode = \"reckless\"\n").unwrap();
    let path = tmp.path().join("goal.at");
    std::fs::write(&path, r#"flow main(prompt: string, model: string = "unused") -> string {
        contract { invocation { goal: true, user_message: prompt } }
        snapshot = memory.goal.get()
        when snapshot.goal.objective == "first" {
            memory.goal.set(text: "second", goal_id: snapshot.goal.id, expected_revision: snapshot.goal.revision)
            return "first turn"
        }
        memory.goal.pause(goal_id: snapshot.goal.id, expected_revision: snapshot.goal.revision, reason: "completed", summary: "Finished")
        return "finished"
    }"#).unwrap();
    let state = Arc::new(DaemonState::new(data.clone()));
    let launcher = RunLauncher::new(tmp.path().into(), Some(config), None).unwrap();
    let spawned = launcher
        .spawn_as_with_options(
            state.clone(),
            path.to_str().unwrap(),
            vec![
                (
                    "model".into(),
                    atman_runtime::Value::Str("not-the-goal".into()),
                ),
                ("prompt".into(), atman_runtime::Value::Str("first".into())),
            ],
            "goal-owner",
            atman_daemon::run::RunOptions {
                goal: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(spawned.goal_id.is_some());
    let session = state
        .authorized_live_session(&spawned.session_id, "goal-owner")
        .unwrap();
    let mut changes = session.goal_control.subscribe();
    for count in [2, 3, 4, 5] {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let ended = session.sink().snapshot().iter().filter(|e| matches!(e, atman_runtime::Event::FlowEnd { .. })).count();
                if ended == count && !session.goal_control.should_continue() && !session.goal_control.turn_running() { break; }
                tokio::select! { _ = changes.changed() => {}, _ = tokio::time::sleep(Duration::from_millis(10)) => {} }
            }
        }).await.unwrap_or_else(|e| panic!("{e}: {:?}", session.goal_control.snapshot()));
        assert_eq!(
            session
                .goal_control
                .record()
                .unwrap()
                .pause
                .unwrap()
                .summary,
            "Finished"
        );
        if count == 2 {
            session
                .control_goal(None, atman_runtime::goal::GoalAction::Resume, None)
                .unwrap();
        }
        if count == 3 {
            // A retired watcher still starts a separate turn after an agent completion pause.
            let source = atman_runtime::watch::WatchSource::Bash {
                handle: "job".into(),
            };
            let watcher_id = session.watch_hub.register(
                source.clone(),
                "ready".into(),
                atman_runtime::watch::WatchMode::Once,
                Duration::from_secs(1),
            );
            session
                .watch_hub
                .enqueue_event(atman_runtime::watch::WatchEvent {
                    watcher_id,
                    source,
                    pattern: "ready".into(),
                    row: None,
                    col: None,
                    text: "ready".into(),
                    timestamp: chrono::Utc::now(),
                    timed_out: false,
                    exited: false,
                    timeout: Duration::from_secs(1),
                });
        }
        if count == 4 {
            let route = session
                .goal_control
                .child_results
                .route("delayed-child".into(), true);
            route.publish(
                atman_rt::RunId::now(),
                atman_rt::TurnId::now(),
                "delivered after completion".into(),
            );
        }
    }
    let runs: Vec<_> = session
        .sink()
        .snapshot()
        .into_iter()
        .filter_map(|e| match e {
            atman_runtime::Event::FlowStart { run_id, .. } => Some(run_id),
            _ => None,
        })
        .collect();
    assert_eq!(runs.len(), 5);
    assert!(runs.windows(2).all(|p| p[0] != p[1]));
    let messages = session.messages();
    assert_eq!(
        messages
            .iter()
            .filter(|m| m.origin == atman_runtime::message::MessageOrigin::Watcher)
            .count(),
        1
    );
    assert_eq!(
        messages
            .iter()
            .filter(|m| m.origin == atman_runtime::message::MessageOrigin::ChildResult)
            .count(),
        1
    );
    assert!(state.cancel_run(&spawned.run_id));
}

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
async fn launcher_runs_flow_with_used_source() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    let library = project_root.join(".atman/lib");
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&library).unwrap();
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        library.join("helper.at"),
        "pub flow helper() -> string { return \"ok\" }\n",
    )
    .unwrap();
    let entry = project_root.join("main.at");
    std::fs::write(
        &entry,
        "use \"project:helper.at\"::helper\nflow main() -> string { return helper().await }\n",
    )
    .unwrap();

    let state = Arc::new(DaemonState::new(data_dir.clone()));
    let launcher = RunLauncher::new(project_root, Some(config_dir), None).unwrap();
    let spawned = launcher
        .spawn(state, entry.to_str().unwrap(), Vec::new())
        .await
        .unwrap();
    let events_path = data_dir
        .join("sessions")
        .join(spawned.session_id.to_string())
        .join("events.jsonl");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(events) = std::fs::read_to_string(&events_path) {
            if let Some(end) = events
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .find(|event| event["type"] == "flow_end" && event["flow_name"] == "main")
            {
                assert_eq!(end["status"]["kind"], "ok", "{events}");
                break;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for flow_end in {}",
            events_path.display()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn run_flow_end_to_end_writes_events_and_appears_in_list_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let config_dir = tmp.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();

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
            panic!(
                "timed out waiting for flow_end in {}",
                events_path.display()
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
