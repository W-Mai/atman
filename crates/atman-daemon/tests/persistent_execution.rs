use std::{sync::Arc, time::Duration};

use atman_daemon::{DaemonState, dispatch, dispatch_as, run::RunLauncher};
use atman_proto::{JsonRpcRequest, methods};
use serde_json::{Value, json};
use uuid::Uuid;

async fn rpc(state: &Arc<DaemonState>, method: &str, params: Value) -> Value {
    let response = dispatch(state.clone(), JsonRpcRequest::new(1, method, params)).await;
    assert!(response.error.is_none(), "{method}: {:?}", response.error);
    response.result.unwrap()
}

async fn setup(dir: &tempfile::TempDir, source: &str) -> (Arc<DaemonState>, Value, Value) {
    let config = dir.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("config.toml"), "[trust]\nmode = \"reckless\"\n").unwrap();
    let state = Arc::new(DaemonState::new(dir.path().join("data")));
    state.set_launcher(Arc::new(
        RunLauncher::new(dir.path().to_path_buf(), Some(config), None).unwrap(),
    ));
    let id = Uuid::now_v7();
    let session = rpc(
        &state,
        methods::SESSION_CREATE,
        json!({"session_id": id, "title": "RPC execution", "project_root": dir.path()}),
    )
    .await;
    let path = dir.path().join("main.at");
    std::fs::write(&path, source).unwrap();
    let flow = json!({"session_id": id, "instance_id": Uuid::now_v7()});
    let launch = json!({"flow": flow, "request_id": Uuid::now_v7(), "expected_catalog_sequence": session["catalog"]["sequence"],
        "path": path, "entry": "main", "arguments": {"label": "first"}, "invocation": {"effort": "high"}});
    (state, flow, launch)
}

async fn wait_completion(
    state: &Arc<DaemonState>,
    flow: &Value,
    previous: Option<&Value>,
) -> Value {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let view = rpc(state, methods::FLOW_GET, json!({"flow": flow})).await;
            let completion = &view["last_completion"];
            if !completion.is_null() && previous != Some(&completion["lease"]["turn"]) {
                assert_eq!(completion["result"]["kind"], "returned", "{view}");
                return view;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("entry should complete")
}

fn answer(page: &Value, index: usize) -> String {
    let message: atman_runtime::message::Message =
        serde_json::from_value(page["messages"][index]["publication"]["message"].clone()).unwrap();
    message.text_concat()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn waiting_launch_records_configuration_without_input_and_preserves_gates_on_restore() {
    let dir = tempfile::tempdir().unwrap();
    let (flow, launch, accepted) = runtime().block_on(async {
        let (state, flow, mut launch) = setup(&dir, "flow main(label: string) -> string { return final.answer(message: label) }").await;
        launch["start"] = json!("waiting");
        let mut invalid = launch.clone();
        invalid["user"] = json!({"text": "must not be discarded"});
        assert!(dispatch(state.clone(), JsonRpcRequest::new(1, methods::FLOW_LAUNCH, invalid)).await.error.is_some());
        let accepted = rpc(&state, methods::FLOW_LAUNCH, launch.clone()).await;
        assert!(accepted["acceptance"].is_null());
        assert!(accepted["receipt"].is_object());
        assert_eq!(rpc(&state, methods::FLOW_LAUNCH, launch.clone()).await, accepted);
        let view = rpc(&state, methods::FLOW_GET, json!({"flow": flow})).await;
        assert!(view["last_completion"].is_null());
        assert_eq!(view["execution"]["state"]["open"], "idle");
        assert_eq!(rpc(&state, methods::FLOW_OUTPUT, json!({"flow": flow})).await["messages"], json!([]));
        let mut conflict = launch.clone();
        conflict["start"] = json!("invoke");
        assert!(dispatch(state.clone(), JsonRpcRequest::new(1, methods::FLOW_LAUNCH, conflict)).await.error.is_some());
        rpc(&state, methods::FLOW_CONTROL, json!({"request_id": Uuid::now_v7(), "flow": flow, "action": "pause", "expected_revision": view["execution"]["revision"]})).await;
        std::fs::remove_file(dir.path().join("main.at")).unwrap();
        state.shutdown().await.unwrap();
        (flow, launch, accepted)
    });
    runtime().block_on(async {
        let state = Arc::new(DaemonState::new(dir.path().join("data")));
        rpc(&state, methods::SESSION_OPEN, json!({"session_id": flow["session_id"]})).await;
        assert_eq!(rpc(&state, methods::FLOW_LAUNCH, launch).await, accepted);
        rpc(&state, methods::FLOW_BIND, json!({"flow": flow})).await;
        rpc(&state, methods::FLOW_INVOKE, json!({"flow": flow, "request_id": Uuid::now_v7(), "arguments": {"label": "real input"}, "user": {"text": "real input"}})).await;
        let paused = rpc(&state, methods::FLOW_GET, json!({"flow": flow})).await;
        assert!(paused["last_completion"].is_null());
        assert_eq!(paused["execution"]["pause_count"], 1);
        rpc(&state, methods::FLOW_CONTROL, json!({"request_id": Uuid::now_v7(), "flow": flow, "action": "resume", "expected_revision": paused["execution"]["revision"]})).await;
        wait_completion(&state, &flow, None).await;
        let output = rpc(&state, methods::FLOW_OUTPUT, json!({"flow": flow})).await;
        assert_eq!(output["messages"].as_array().unwrap().len(), 1);
        assert_eq!(answer(&output, 0), "real input");
        state.shutdown().await.unwrap();
    });
}

#[tokio::test]
async fn close_rpc_stops_paused_execution_and_preserves_history_across_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (state, flow, launch) = setup(
        &dir,
        r#"flow main(label: string) -> string {
        when label == "hold" { loop { yield } }
        return final.answer(message: label)
    }"#,
    )
    .await;
    rpc(&state, methods::FLOW_LAUNCH, launch).await;
    wait_completion(&state, &flow, None).await;
    let output = rpc(&state, methods::FLOW_OUTPUT, json!({"flow": flow})).await;
    assert_eq!(answer(&output, 0), "first");
    rpc(
        &state,
        methods::FLOW_INVOKE,
        json!({"flow": flow, "request_id": Uuid::now_v7(), "arguments": {"label": "hold"}}),
    )
    .await;
    let active = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let view = rpc(&state, methods::FLOW_GET, json!({"flow": flow})).await;
            if view["execution"]["state"]["open"]["active"].is_object() {
                break view;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    rpc(&state, methods::FLOW_CONTROL, json!({"request_id": Uuid::now_v7(), "flow": flow, "action": "pause", "expected_revision": active["execution"]["revision"]})).await;
    let queued = json!({"flow": flow, "request_id": Uuid::now_v7(), "arguments": {"label": "cancelled input"}});
    let acceptance = rpc(&state, methods::FLOW_INVOKE, queued.clone()).await;
    let paused = rpc(&state, methods::FLOW_GET, json!({"flow": flow})).await;
    let close = json!({"request_id": Uuid::now_v7(), "flow": flow, "action": "close", "expected_revision": paused["execution"]["revision"]});
    let denied = dispatch_as(
        state.clone(),
        JsonRpcRequest::new(1, methods::FLOW_CONTROL, close.clone()),
        "unknown",
    )
    .await;
    assert!(denied.error.is_some());
    assert_eq!(
        rpc(&state, methods::FLOW_GET, json!({"flow": flow})).await["execution"],
        paused["execution"]
    );
    rpc(&state, methods::FLOW_CONTROL, close).await;
    let closed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let view = rpc(&state, methods::FLOW_GET, json!({"flow": flow})).await;
            if view["execution"]["state"] == "closed" {
                break view;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(closed["execution"]["can_start_turn"], false);
    assert_eq!(
        rpc(&state, methods::FLOW_OUTPUT, json!({"flow": flow})).await,
        output
    );
    let retried = rpc(&state, methods::FLOW_INVOKE, queued).await;
    assert_eq!(
        retried["acceptance"]["message_id"],
        acceptance["acceptance"]["message_id"]
    );
    assert_eq!(
        retried["acceptance"]["receipt"],
        acceptance["acceptance"]["receipt"]
    );
    assert_eq!(retried["acceptance"]["disposition"]["state"], "cancelled");
    let rejected = dispatch(state.clone(), JsonRpcRequest::new(1, methods::FLOW_INVOKE, json!({"flow": flow, "request_id": Uuid::now_v7(), "arguments": {"label": "new input"}}))).await;
    assert!(rejected.error.is_some());
    let history = rpc(
        &state,
        methods::FLOW_EVENTS,
        json!({"flow": flow, "limit": 128}),
    )
    .await;
    state.shutdown().await.unwrap();
    let reopened = Arc::new(DaemonState::new(dir.path().join("data")));
    rpc(
        &reopened,
        methods::SESSION_OPEN,
        json!({"session_id": flow["session_id"]}),
    )
    .await;
    assert_eq!(
        rpc(&reopened, methods::FLOW_GET, json!({"flow": flow})).await["execution"],
        closed["execution"]
    );
    assert_eq!(
        rpc(&reopened, methods::FLOW_OUTPUT, json!({"flow": flow})).await,
        output
    );
    assert_eq!(
        rpc(
            &reopened,
            methods::FLOW_EVENTS,
            json!({"flow": flow, "limit": 128})
        )
        .await,
        history
    );
    reopened.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_shutdown_drains_execution_and_ends_connected_streams_without_losing_queued_input() {
    use axum::{body::Body, http::Request};
    use futures::StreamExt;
    use tower::ServiceExt;
    let dir = tempfile::tempdir().unwrap();
    let (state, flow, launch) =
        setup(&dir, "flow main(label: string) -> any { loop { yield } }").await;
    rpc(&state, methods::FLOW_LAUNCH, launch.clone()).await;
    let active = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let view = rpc(&state, methods::FLOW_GET, json!({"flow": flow})).await;
            if view["execution"]["state"]["open"]["active"].is_object() {
                break view;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    rpc(&state, methods::FLOW_CONTROL, json!({"request_id": Uuid::now_v7(), "flow": flow, "action": "pause", "expected_revision": active["execution"]["revision"]})).await;
    let queued = rpc(
        &state,
        methods::FLOW_INVOKE,
        json!({"flow": flow, "request_id": Uuid::now_v7(), "arguments": {"label": "queued"}}),
    )
    .await;
    let app = atman_daemon::http::router(Arc::new(atman_daemon::http::HttpState {
        daemon: state.clone(),
        auth_token: "secret".into(),
    }));
    let response = app
        .oneshot(
            Request::get(format!(
                "/events?session_id={}&instance_id={}&token=secret",
                flow["session_id"].as_str().unwrap(),
                flow["instance_id"].as_str().unwrap()
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let mut stream = response.into_body().into_data_stream();
    assert!(stream.next().await.unwrap().is_ok());

    let path = dir.path().join("direct.at");
    std::fs::write(&path, "flow main() -> any { loop { yield } }").unwrap();
    let direct = state
        .launcher()
        .unwrap()
        .spawn(state.clone(), path.to_str().unwrap(), vec![])
        .await
        .unwrap();
    let direct_flow = json!({"session_id": direct.session_id, "instance_id": direct.instance_id});
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let view = rpc(&state, methods::FLOW_GET, json!({"flow": direct_flow})).await;
            if view["execution"]["state"]["open"]["active"].is_object() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();

    tokio::time::timeout(Duration::from_secs(10), state.shutdown())
        .await
        .unwrap()
        .unwrap();
    state.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(chunk) = stream.next().await {
            chunk.unwrap();
        }
    })
    .await
    .unwrap();
    let rejected = dispatch(
        state.clone(),
        JsonRpcRequest::new(1, methods::FLOW_LAUNCH, launch),
    )
    .await;
    assert!(rejected.error.unwrap().message.contains("stopping"));
    let root = atman_runtime::flow_store::RuntimeStore::open(&dir.path().join("data")).unwrap();
    let direct_session = root.open_session(direct.session_id.0).unwrap();
    let direct_worker = direct_session
        .open_flow(&atman_rt::FlowInstanceId(direct.instance_id.into()))
        .unwrap();
    assert_eq!(
        direct_worker.completion(None).unwrap().completion.outcome,
        atman_rt::TurnOutcome::Interrupted(atman_rt::Interruption::Cancelled)
    );
    let session = root
        .open_session(serde_json::from_value(flow["session_id"].clone()).unwrap())
        .unwrap();
    let address: atman_runtime::flow_store::FlowRef = serde_json::from_value(flow).unwrap();
    let recovered = session.open_flow(&address.instance_id).unwrap();
    assert_eq!(recovered.state().pauses().len(), 1);
    assert_eq!(
        recovered.completion(None).unwrap().completion.outcome,
        atman_rt::TurnOutcome::Interrupted(atman_rt::Interruption::Cancelled)
    );
    assert_eq!(
        serde_json::to_value(recovered.pending_inputs().next().unwrap().id).unwrap(),
        queued["acceptance"]["message_id"]
    );
    assert!(recovered.published_messages().is_empty());
}

async fn wait_prompt(state: &Arc<DaemonState>) -> atman_proto::PromptId {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(prompt) = state.pending_prompt_ids().first() {
                return prompt.clone();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap()
}

async fn wait_permissions(state: &Arc<DaemonState>, flow: &Value, count: usize) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let listed = rpc(state, methods::LIST_PERMISSION_REQUESTS, flow.clone()).await;
        if listed["requests"].as_array().unwrap().len() == count {
            return listed;
        }
        if tokio::time::Instant::now() >= deadline {
            let status = rpc(state, methods::FLOW_GET, json!({"flow": flow})).await;
            panic!("expected {count} permissions, got {listed}; flow: {status}");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn rpc_entries_retain_source_and_identity_across_turns_and_pauses() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("helper.at"),
        "pub flow echo(text: string) -> string { return text }",
    )
    .unwrap();
    let (state, flow, launch) = setup(
        &dir,
        r#"
        use "./helper.at"::echo
        flow main(label: string) -> string {
            when env("effort") != "high" { return final.answer(message: "wrong invocation") }
            text = echo(label).await
            return final.answer(message: text)
        }
    "#,
    )
    .await;
    let (accepted, concurrent) = tokio::join!(
        rpc(&state, methods::FLOW_LAUNCH, launch.clone()),
        rpc(&state, methods::FLOW_LAUNCH, launch.clone())
    );
    assert_eq!(
        accepted["acceptance"]["receipt"],
        concurrent["acceptance"]["receipt"]
    );
    assert_eq!(
        accepted["acceptance"]["message_id"],
        concurrent["acceptance"]["message_id"]
    );
    let first = wait_completion(&state, &flow, None).await;
    assert_eq!(first["scheduler"]["status"], "running");
    assert_eq!(first["execution"]["can_start_turn"], true);
    let page = rpc(
        &state,
        methods::FLOW_OUTPUT,
        json!({"flow": flow, "limit": 1}),
    )
    .await;
    assert_eq!(answer(&page, 0), "first");
    std::fs::remove_file(dir.path().join("main.at")).unwrap();
    std::fs::remove_file(dir.path().join("helper.at")).unwrap();
    let retried = rpc(&state, methods::FLOW_LAUNCH, launch.clone()).await;
    assert_eq!(
        retried["acceptance"]["receipt"],
        accepted["acceptance"]["receipt"]
    );
    assert_eq!(
        retried["acceptance"]["message_id"],
        accepted["acceptance"]["message_id"]
    );
    assert_eq!(
        rpc(&state, methods::FLOW_GET, json!({"flow": flow})).await["last_completion"],
        first["last_completion"]
    );
    let mut conflicting = launch;
    conflicting["arguments"]["label"] = json!("changed");
    assert!(
        dispatch(
            state.clone(),
            JsonRpcRequest::new(1, methods::FLOW_LAUNCH, conflicting)
        )
        .await
        .error
        .is_some()
    );

    rpc(&state, methods::FLOW_CONTROL, json!({"request_id": Uuid::now_v7(), "flow": flow, "expected_revision": first["execution"]["revision"], "action": "pause"})).await;
    let invocation = json!({"flow": flow, "request_id": Uuid::now_v7(), "arguments": {"label": "second"}, "invocation": {"effort": "high"}});
    let accepted = rpc(&state, methods::FLOW_INVOKE, invocation.clone()).await;
    assert_eq!(
        rpc(&state, methods::FLOW_INVOKE, invocation.clone()).await,
        accepted
    );
    let paused = rpc(&state, methods::FLOW_GET, json!({"flow": flow})).await;
    assert_eq!(paused["last_completion"], first["last_completion"]);
    let empty = rpc(
        &state,
        methods::FLOW_OUTPUT,
        json!({"flow": flow, "after": page["next"]}),
    )
    .await;
    assert_eq!(empty["messages"], json!([]));
    assert_eq!(empty["next"], page["next"]);
    rpc(&state, methods::FLOW_CONTROL, json!({"request_id": Uuid::now_v7(), "flow": flow, "expected_revision": paused["execution"]["revision"], "action": "resume"})).await;
    let second = wait_completion(
        &state,
        &flow,
        Some(&first["last_completion"]["lease"]["turn"]),
    )
    .await;
    assert_ne!(
        second["last_completion"]["lease"]["run"],
        first["last_completion"]["lease"]["run"]
    );
    let page = rpc(
        &state,
        methods::FLOW_OUTPUT,
        json!({"flow": flow, "after": page["next"], "limit": 1}),
    )
    .await;
    assert_eq!(answer(&page, 0), "second");
    assert_eq!(page["has_more"], false);
    let duplicate = rpc(&state, methods::FLOW_INVOKE, invocation).await;
    assert_eq!(
        duplicate["acceptance"]["receipt"],
        accepted["acceptance"]["receipt"]
    );
    assert_eq!(
        rpc(&state, methods::FLOW_GET, json!({"flow": flow})).await["last_completion"],
        second["last_completion"]
    );
    let weak = Arc::downgrade(&state);
    drop(state);
    assert!(
        weak.upgrade().is_none(),
        "retained runtime must not keep its daemon transport alive"
    );
}

#[tokio::test]
async fn rpc_entry_waits_for_the_original_daemon_prompt_transport() {
    let dir = tempfile::tempdir().unwrap();
    let (state, flow, launch) = setup(
        &dir,
        r#"flow main(label: string) -> string {
        confirmed = user_confirm(label)
        when confirmed { return final.answer(message: "confirmed") }
        return final.answer(message: "declined")
    }"#,
    )
    .await;
    rpc(&state, methods::FLOW_LAUNCH, launch).await;
    let prompt = wait_prompt(&state).await;
    let resolved = rpc(
        &state,
        methods::RESOLVE_PROMPT,
        json!({"prompt_id": prompt, "session_id": flow["session_id"], "answer": atman_runtime::form::FormSubmission::Submitted {
            answers: vec![atman_runtime::form::FormAnswer::Confirmed { value: true }],
        }}),
    )
    .await;
    assert_eq!(resolved["resolved"], true);
    wait_completion(&state, &flow, None).await;
    assert!(state.pending_prompt_ids().is_empty());
    assert_eq!(
        answer(
            &rpc(&state, methods::FLOW_OUTPUT, json!({"flow": flow})).await,
            0
        ),
        "confirmed"
    );
}

#[tokio::test]
async fn initial_goal_is_visible_to_the_first_turn_and_retries_do_not_restore_it() {
    let dir = tempfile::tempdir().unwrap();
    let (state, flow, mut launch) = setup(&dir, r#"flow main(label: string) -> string {
        snapshot = memory.goal.get()
        memory.goal.pause(goal_id: snapshot.goal.id, expected_revision: snapshot.goal.revision, reason: "completed", summary: "finished")
        return final.answer(message: snapshot.goal.objective)
    }"#).await;
    launch["initial_goal"] = json!("Explicit objective");
    let accepted = rpc(&state, methods::FLOW_LAUNCH, launch.clone()).await;
    let finished = wait_completion(&state, &flow, None).await;
    assert_eq!(finished["goal"]["objective"], "Explicit objective");
    assert_eq!(finished["goal"]["status"], "paused");
    assert_eq!(finished["goal"]["revision"], 2);
    assert_eq!(
        answer(
            &rpc(&state, methods::FLOW_OUTPUT, json!({"flow": flow})).await,
            0
        ),
        "Explicit objective"
    );
    let duplicate = rpc(&state, methods::FLOW_LAUNCH, launch.clone()).await;
    assert_eq!(
        duplicate["acceptance"]["receipt"],
        accepted["acceptance"]["receipt"]
    );
    let mut conflict = launch;
    conflict["initial_goal"] = json!("Replaced objective");
    assert!(
        dispatch(
            state.clone(),
            JsonRpcRequest::new(1, methods::FLOW_LAUNCH, conflict)
        )
        .await
        .error
        .is_some()
    );
    assert_eq!(
        rpc(&state, methods::FLOW_GET, json!({"flow": flow})).await["goal"],
        finished["goal"]
    );
    state.shutdown().await.unwrap();
    let reopened = Arc::new(DaemonState::new(dir.path().join("data")));
    rpc(
        &reopened,
        methods::SESSION_OPEN,
        json!({"session_id": flow["session_id"]}),
    )
    .await;
    let restored = rpc(&reopened, methods::FLOW_GET, json!({"flow": flow})).await;
    assert_eq!(restored["goal"], finished["goal"]);
    assert_eq!(restored["last_completion"], finished["last_completion"]);
    assert!(restored["scheduler"].is_null());
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn persistent_goal_controls_resume_once_without_bypassing_execution_or_replaying_user_input()
{
    let dir = tempfile::tempdir().unwrap();
    let (state, flow, mut launch) = setup(&dir, r#"flow main(user_message: string, label: string) -> string {
        contract { invocation { user_message: user_message } }
        snapshot = memory.goal.get()
        memory.goal.pause(goal_id: snapshot.goal.id, expected_revision: snapshot.goal.revision, reason: "completed", summary: "finished")
        return final.answer(message: label)
    }"#).await;
    launch["arguments"]["user_message"] = json!("hello");
    launch["user"] = json!({"text": "hello", "images": []});
    launch["initial_goal"] = json!("Explicit objective");
    rpc(&state, methods::FLOW_LAUNCH, launch).await;
    let first = wait_completion(&state, &flow, None).await;
    let default_goal = rpc(
        &state,
        methods::GOAL_GET,
        json!({"session_id": flow["session_id"]}),
    )
    .await;
    assert_eq!(default_goal["flow"], flow);
    assert_eq!(default_goal["goal"], first["goal"]);
    let request = |goal: &Value| {
        json!({
            "session_id": flow["session_id"], "instance_id": flow["instance_id"],
            "request_id": Uuid::now_v7(), "goal_id": goal["id"], "expected_revision": goal["revision"],
        })
    };
    let resume = request(&first["goal"]);
    let denied = dispatch_as(
        state.clone(),
        JsonRpcRequest::new(1, methods::GOAL_RESUME, resume.clone()),
        "another-client",
    )
    .await;
    assert!(denied.error.unwrap().message.contains("permission denied"));
    let mut unbound = resume.clone();
    unbound.as_object_mut().unwrap().remove("instance_id");
    assert!(
        dispatch(
            state.clone(),
            JsonRpcRequest::new(1, methods::GOAL_RESUME, unbound)
        )
        .await
        .error
        .is_some()
    );
    // Pausing execution does not change the objective; resuming the objective does not remove that gate.
    rpc(&state, methods::FLOW_CONTROL, json!({"request_id": Uuid::now_v7(), "flow": flow, "action": "pause", "expected_revision": first["execution"]["revision"]})).await;
    let accepted = rpc(&state, methods::GOAL_RESUME, resume.clone()).await;
    assert_eq!(accepted["goal"]["status"], "active");
    assert_eq!(accepted["execution"]["pause_count"], 1);
    assert_eq!(accepted["execution"]["can_start_turn"], false);
    assert_eq!(
        accepted["committed"]["update"]["input"]["request"]["payload"]["kind"],
        "goal_resume"
    );
    assert_eq!(
        rpc(&state, methods::GOAL_RESUME, resume.clone()).await["committed"],
        accepted["committed"]
    );
    assert!(
        dispatch(
            state.clone(),
            JsonRpcRequest::new(1, methods::GOAL_RESUME, request(&accepted["goal"]))
        )
        .await
        .error
        .is_some()
    );
    state.shutdown().await.unwrap();

    let reopened = Arc::new(DaemonState::new(dir.path().join("data")));
    rpc(
        &reopened,
        methods::SESSION_OPEN,
        json!({"session_id": flow["session_id"]}),
    )
    .await;
    let restored = rpc(&reopened, methods::GOAL_GET, flow.clone()).await;
    assert_eq!(restored["goal"], accepted["goal"]);
    assert_eq!(restored["execution"]["pause_count"], 1);
    assert_eq!(
        rpc(&reopened, methods::GOAL_RESUME, resume.clone()).await["committed"],
        accepted["committed"]
    );
    rpc(&reopened, methods::FLOW_BIND, json!({"flow": flow})).await;
    let bound = rpc(&reopened, methods::FLOW_GET, json!({"flow": flow})).await;
    assert_eq!(bound["last_completion"], first["last_completion"]);
    rpc(&reopened, methods::FLOW_CONTROL, json!({"request_id": Uuid::now_v7(), "flow": flow, "action": "resume", "expected_revision": bound["execution"]["revision"]})).await;
    let second = wait_completion(
        &reopened,
        &flow,
        Some(&first["last_completion"]["lease"]["turn"]),
    )
    .await;
    assert_eq!(second["goal"]["status"], "paused");
    assert_eq!(second["goal"]["revision"], 4);
    assert_eq!(
        rpc(&reopened, methods::GOAL_RESUME, resume.clone()).await["committed"],
        accepted["committed"]
    );
    let page = rpc(&reopened, methods::FLOW_OUTPUT, json!({"flow": flow})).await;
    assert_eq!(page["messages"].as_array().unwrap().len(), 2);
    assert_eq!(answer(&page, 0), "first");
    assert_eq!(answer(&page, 1), "first");
    let mut stale = resume;
    stale["request_id"] = json!(Uuid::now_v7());
    assert!(
        dispatch(
            reopened.clone(),
            JsonRpcRequest::new(1, methods::GOAL_RESUME, stale)
        )
        .await
        .error
        .is_some()
    );
    // Mutations use the same CAS path; editing and clearing never restart the entry.
    let mut edit = request(&second["goal"]);
    edit["text"] = json!("Edited objective");
    let edited = rpc(&reopened, methods::GOAL_SET, edit).await;
    assert_eq!(edited["goal"]["status"], "paused");
    let cleared = rpc(&reopened, methods::GOAL_CLEAR, request(&edited["goal"])).await;
    assert!(cleared["goal"].is_null());
    reopened.shutdown().await.unwrap();
    let root = atman_runtime::flow_store::RuntimeStore::open(&dir.path().join("data")).unwrap();
    let session = root
        .open_session(serde_json::from_value(flow["session_id"].clone()).unwrap())
        .unwrap();
    let address: atman_runtime::flow_store::FlowRef = serde_json::from_value(flow).unwrap();
    let mut stored = session.open_flow(&address.instance_id).unwrap();
    assert_eq!(stored.completions().len(), 2);
    assert_eq!(stored.pending_inputs().count(), 0);
    let contexts = stored.context_messages().unwrap();
    let users = contexts
        .iter()
        .filter(|message| message.role == atman_runtime::message::MessageRole::User)
        .collect::<Vec<_>>();
    assert_eq!(users.len(), 2);
    assert_eq!(users[0].text_concat(), "hello");
    assert_eq!(
        users[1].origin,
        atman_runtime::message::MessageOrigin::Internal
    );
    assert!(users[1].text_concat().contains("goal_resume"));
    let atman_runtime::flow_store::inbox::InputContent::InvocationInput {
        arguments,
        invocation,
        ..
    } = &stored
        .last_invocation_input()
        .unwrap()
        .unwrap()
        .input
        .content
    else {
        panic!("resume must have a committed invocation");
    };
    assert_eq!(arguments["label"], json!("first"));
    assert!(
        arguments["user_message"]
            .as_str()
            .unwrap()
            .contains("goal_resume")
    );
    assert_eq!(invocation["effort"], json!("high"));
}

#[tokio::test]
async fn cancel_run_targets_only_the_current_invocation_and_requires_its_owner() {
    let dir = tempfile::tempdir().unwrap();
    let (state, flow, launch) = setup(
        &dir,
        r#"flow main(label: string) -> string {
        when label == "hold" { loop { yield } }
        return final.answer(message: label)
    }"#,
    )
    .await;
    rpc(&state, methods::FLOW_LAUNCH, launch).await;
    let first = wait_completion(&state, &flow, None).await;
    rpc(
        &state,
        methods::FLOW_INVOKE,
        json!({"flow": flow, "request_id": Uuid::now_v7(), "arguments": {"label": "hold"}}),
    )
    .await;
    let running = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let view = rpc(&state, methods::FLOW_GET, json!({"flow": flow})).await;
            if view["execution"]["state"]["open"]["active"].is_object() {
                break view;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let old_run = &first["last_completion"]["lease"]["run"];
    assert_eq!(
        rpc(&state, methods::CANCEL_RUN, json!({"run_id": old_run})).await["cancelled"],
        false
    );
    let run = &running["execution"]["state"]["open"]["active"]["lease"]["run"];
    assert_ne!(run, old_run);
    assert!(!run.is_null());
    let denied = dispatch_as(
        state.clone(),
        JsonRpcRequest::new(1, methods::CANCEL_RUN, json!({"run_id": run})),
        "another-client",
    )
    .await;
    assert!(denied.error.unwrap().message.contains("permission denied"));
    assert_eq!(
        rpc(&state, methods::FLOW_GET, json!({"flow": flow})).await["execution"]["state"]["open"]["active"]
            ["lease"]["run"],
        *run
    );
    assert_eq!(
        rpc(&state, methods::CANCEL_RUN, json!({"run_id": run})).await["cancelled"],
        true
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let view = rpc(&state, methods::FLOW_GET, json!({"flow": flow})).await;
            if view["execution"]["state"]["open"]["interrupted"].is_object() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        rpc(&state, methods::CANCEL_RUN, json!({"run_id": run})).await["cancelled"],
        false
    );
    state.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_preparation_and_stale_inventory_do_not_register_executable_flows() {
    let dir = tempfile::tempdir().unwrap();
    let (state, flow, mut launch) =
        setup(&dir, "flow main(label: string) -> string { return label }").await;
    launch["expected_catalog_sequence"] = json!(0);
    assert!(
        dispatch(
            state.clone(),
            JsonRpcRequest::new(1, methods::FLOW_LAUNCH, launch.clone())
        )
        .await
        .error
        .is_some()
    );
    let listed = rpc(
        &state,
        methods::SESSION_FLOWS,
        json!({"session_id": flow["session_id"]}),
    )
    .await;
    assert_eq!(listed["total"], 0);
    launch["expected_catalog_sequence"] = listed["catalog"]["sequence"].clone();
    launch["entry"] = json!("missing");
    assert!(
        dispatch(
            state.clone(),
            JsonRpcRequest::new(1, methods::FLOW_LAUNCH, launch.clone())
        )
        .await
        .error
        .is_some()
    );
    assert_eq!(
        rpc(
            &state,
            methods::SESSION_FLOWS,
            json!({"session_id": flow["session_id"]})
        )
        .await["total"],
        0
    );
    launch["entry"] = json!("main");
    rpc(&state, methods::FLOW_LAUNCH, launch).await;
    let view = wait_completion(&state, &flow, None).await;
    assert_eq!(view["last_completion"]["result"]["value"]["value"], "first");
    assert_eq!(
        rpc(&state, methods::FLOW_OUTPUT, json!({"flow": flow})).await["messages"],
        json!([]),
        "ordinary returns must not become final answers"
    );
}

#[test]
fn reopening_requires_explicit_binding_and_preserves_paused_input() {
    let dir = tempfile::tempdir().unwrap();
    let (flow, launch, accepted, first, invocation) = runtime().block_on(async {
        let (state, flow, mut launch) = setup(&dir, "flow main(label: string) -> string { return final.answer(message: label) }").await;
        launch["user"] = json!({"text": "first human request", "images": [{"data_base64": "iVBORw0KGgo=", "name": "image.png"}]});
        let mut invalid = launch.clone();
        invalid["user"]["images"][0]["data_base64"] = json!("invalid base64");
        assert!(dispatch(state.clone(), JsonRpcRequest::new(1, methods::FLOW_LAUNCH, invalid)).await.error.is_some());
        let catalog = rpc(&state, methods::SESSION_FLOWS, json!({"session_id": flow["session_id"]})).await;
        assert_eq!(catalog["catalog"]["sequence"], launch["expected_catalog_sequence"]);
        let accepted = rpc(&state, methods::FLOW_LAUNCH, launch.clone()).await;
        let first = wait_completion(&state, &flow, None).await;
        rpc(&state, methods::FLOW_CONTROL, json!({"request_id": Uuid::now_v7(), "flow": flow, "expected_revision": first["execution"]["revision"], "action": "pause"})).await;
        let invocation = json!({"flow": flow, "request_id": Uuid::now_v7(), "arguments": {"label": "second"}, "user": {"text": "second human request", "images": []}});
        rpc(&state, methods::FLOW_INVOKE, invocation.clone()).await;
        std::fs::remove_file(dir.path().join("main.at")).unwrap();
        (flow, launch, accepted, first, invocation)
    });
    runtime().block_on(async {
        // No launcher project or configuration is supplied on reopening.
        let state = Arc::new(DaemonState::new(dir.path().join("data")));
        rpc(&state, methods::SESSION_OPEN, json!({"session_id": flow["session_id"]})).await;
        let request = json!({"flow": flow});
        let opened = rpc(&state, methods::FLOW_GET, request.clone()).await;
        assert!(opened["scheduler"].is_null());
        assert_eq!(opened["execution"]["pause_count"], 1);
        assert_eq!(opened["last_completion"], first["last_completion"]);
        std::fs::write(dir.path().join("config/config.toml"), "[invalid").unwrap();
        let retry = rpc(&state, methods::FLOW_LAUNCH, launch).await;
        assert_eq!(retry["acceptance"]["message_id"], accepted["acceptance"]["message_id"]);
        assert_eq!(retry["acceptance"]["receipt"], accepted["acceptance"]["receipt"]);
        let invalid = dispatch(state.clone(), JsonRpcRequest::new(1, methods::FLOW_BIND, request.clone())).await;
        assert!(invalid.error.is_some());
        assert_eq!(rpc(&state, methods::FLOW_GET, request.clone()).await, opened);
        std::fs::write(dir.path().join("config/config.toml"), "[trust]\nmode = \"calm\"\n").unwrap();
        let bound = rpc(&state, methods::FLOW_BIND, request.clone()).await;
        assert_eq!(bound["scheduler"]["status"], "running");
        let duplicate = dispatch(state.clone(), JsonRpcRequest::new(1, methods::FLOW_BIND, request.clone())).await;
        assert!(duplicate.error.is_some());
        let paused = rpc(&state, methods::FLOW_GET, request.clone()).await;
        assert_eq!(paused["execution"], opened["execution"]);
        assert_eq!(paused["last_completion"], first["last_completion"]);
        rpc(&state, methods::FLOW_CONTROL, json!({"request_id": Uuid::now_v7(), "flow": flow, "expected_revision": paused["execution"]["revision"], "action": "resume"})).await;
        let second = wait_completion(&state, &flow, Some(&first["last_completion"]["lease"]["turn"])).await;
        let page = rpc(&state, methods::FLOW_OUTPUT, request.clone()).await;
        assert_eq!(page["messages"].as_array().unwrap().len(), 2);
        assert_eq!(answer(&page, 1), "second");
        rpc(&state, methods::FLOW_INVOKE, invocation.clone()).await;
        let mut conflict = invocation;
        conflict["user"]["text"] = json!("changed request");
        assert!(dispatch(state.clone(), JsonRpcRequest::new(1, methods::FLOW_INVOKE, conflict)).await.error.is_some());
        let history = rpc(&state, methods::FLOW_EVENTS, json!({"flow": flow, "limit": 128})).await;
        assert_eq!(history["has_more"], false);
        let inputs: Vec<_> = history["batches"].as_array().unwrap().iter()
            .flat_map(|batch| batch["facts"].as_array().unwrap())
            .filter(|fact| fact["type"] == "input_committed")
            .map(|fact| &fact["data"]["content"])
            .collect();
        assert_eq!(inputs.len(), 2);
        for (index, expected) in ["first human request", "second human request"].iter().enumerate() {
            let message: atman_runtime::message::Message = serde_json::from_value(inputs[index]["messages"][0].clone()).unwrap();
            assert_eq!(message.text_concat(), *expected);
            assert_eq!(message.origin, atman_runtime::message::MessageOrigin::User);
            assert_eq!(message.parts.len(), if index == 0 { 2 } else { 1 });
            if index == 0 {
                let image = atman_runtime::attachment_store::AttachmentStore::at(std::path::Path::new(""))
                    .import_base64("iVBORw0KGgo=", Some("image.png")).unwrap();
                assert!(matches!(&message.parts[1], atman_runtime::message::MessagePart::Image { source } if source == &image));
            }
        }
        assert_eq!(inputs[0]["arguments"]["label"], "first");
        assert_eq!(inputs[1]["arguments"]["label"], "second");
        assert_eq!(rpc(&state, methods::FLOW_GET, request).await["last_completion"], second["last_completion"]);
    });
}

#[tokio::test]
async fn permission_rpc_targets_instances_through_real_tool_execution() {
    let dir = tempfile::tempdir().unwrap();
    let outputs = tempfile::tempdir_in(dir.path()).unwrap();
    let (state, first, mut launch) = setup(
        &dir,
        r#"flow main(label: string) -> string {
        writes = fanout [
            fs.write(path: label + "-a", content: "approved"),
            fs.write(path: label + "-b", content: "approved")
        ]
        return final.answer(message: label)
    }"#,
    )
    .await;
    std::fs::write(
        dir.path().join("config/config.toml"),
        "[trust]\nmode = \"calm\"\n",
    )
    .unwrap();
    launch["arguments"]["label"] = json!(outputs.path().join("first"));
    rpc(&state, methods::FLOW_LAUNCH, launch.clone()).await;
    let first_requests = wait_permissions(&state, &first, 2).await;

    let catalog = rpc(
        &state,
        methods::SESSION_FLOWS,
        json!({"session_id": first["session_id"]}),
    )
    .await;
    let second = json!({"session_id": first["session_id"], "instance_id": Uuid::now_v7()});
    launch["flow"] = second.clone();
    launch["request_id"] = json!(Uuid::now_v7());
    launch["expected_catalog_sequence"] = catalog["catalog"]["sequence"].clone();
    launch["arguments"]["label"] = json!(outputs.path().join("second"));
    rpc(&state, methods::FLOW_LAUNCH, launch).await;
    let second_requests = wait_permissions(&state, &second, 2).await;
    rpc(
        &state,
        methods::SESSION_SELECT_FLOW,
        json!({"flow": second}),
    )
    .await;
    for (flow, requests) in [(&first, &first_requests), (&second, &second_requests)] {
        assert!(
            requests["requests"]
                .as_array()
                .unwrap()
                .iter()
                .all(|request| request["instance_id"] == flow["instance_id"])
        );
        assert_eq!(
            rpc(&state, methods::LIST_PERMISSION_REQUESTS, flow.clone()).await,
            *requests
        );
    }
    assert_eq!(
        rpc(
            &state,
            methods::LIST_PERMISSION_REQUESTS,
            json!({"session_id": first["session_id"]})
        )
        .await,
        second_requests
    );
    for params in [
        json!({"session_id": first["session_id"], "instance_id": Uuid::now_v7()}),
        json!({"session_id": first["session_id"], "instance_id": Uuid::nil()}),
    ] {
        assert!(
            dispatch(
                state.clone(),
                JsonRpcRequest::new(1, methods::LIST_PERMISSION_REQUESTS, params)
            )
            .await
            .error
            .is_some()
        );
    }
    assert!(
        dispatch_as(
            state.clone(),
            JsonRpcRequest::new(1, methods::LIST_PERMISSION_REQUESTS, first.clone()),
            "another-principal"
        )
        .await
        .error
        .is_some()
    );

    let ids_and_revisions = |requests: &Value| {
        let requests = requests["requests"].as_array().unwrap();
        let ids: Vec<_> = requests
            .iter()
            .map(|request| request["request_id"].clone())
            .collect();
        let revisions: serde_json::Map<_, _> = requests
            .iter()
            .map(|request| {
                (
                    request["request_id"].as_str().unwrap().to_owned(),
                    request["revision"].clone(),
                )
            })
            .collect();
        (ids, revisions)
    };
    let (first_ids, first_revisions) = ids_and_revisions(&first_requests);
    let (second_ids, second_revisions) = ids_and_revisions(&second_requests);
    let mut group = first.clone();
    group["request_ids"] = json!(second_ids);
    group["expected_request_revisions"] = json!(second_revisions);
    group["label"] = json!("file writes");
    assert!(
        dispatch(
            state.clone(),
            JsonRpcRequest::new(1, methods::CREATE_PERMISSION_GROUP, group.clone())
        )
        .await
        .error
        .is_some()
    );
    group["request_ids"] = json!(first_ids);
    group["expected_request_revisions"] = json!(first_revisions);
    let grouped = rpc(&state, methods::CREATE_PERMISSION_GROUP, group).await;
    let mut resolution = first.clone();
    resolution["selector"] =
        json!({"group_id": grouped["group_id"], "expected_group_revision": grouped["revision"]});
    resolution["action"] = json!("approve");
    resolution["scope"] = json!({"child_run_same_tool": {"run_id": second_requests["requests"][0]["requesting_run_id"], "tool_name": "fs.write"}});
    assert!(
        dispatch(
            state.clone(),
            JsonRpcRequest::new(1, methods::RESOLVE_PERMISSION_REQUESTS, resolution.clone())
        )
        .await
        .error
        .is_some()
    );
    assert!(!outputs.path().join("first-a").exists());
    resolution["scope"] = json!("current_call");
    let resolved = rpc(
        &state,
        methods::RESOLVE_PERMISSION_REQUESTS,
        resolution.clone(),
    )
    .await;
    assert_eq!(resolved["resolutions"].as_array().unwrap().len(), 2);
    wait_completion(&state, &first, None).await;
    for suffix in ["a", "b"] {
        assert_eq!(
            std::fs::read_to_string(outputs.path().join(format!("first-{suffix}"))).unwrap(),
            "approved"
        );
        assert!(!outputs.path().join(format!("second-{suffix}")).exists());
    }
    assert!(
        dispatch(
            state.clone(),
            JsonRpcRequest::new(1, methods::RESOLVE_PERMISSION_REQUESTS, resolution)
        )
        .await
        .error
        .is_some()
    );
    assert_eq!(
        rpc(&state, methods::LIST_PERMISSION_REQUESTS, second.clone()).await,
        second_requests
    );
    let status = rpc(&state, methods::FLOW_GET, json!({"flow": second})).await;
    rpc(&state, methods::FLOW_CONTROL, json!({"request_id": Uuid::now_v7(), "flow": second, "expected_revision": status["execution"]["revision"], "action": "cancel_turn"})).await;
    wait_permissions(&state, &second, 0).await;
    let mut late = second;
    late["selector"] =
        json!({"request_ids": second_ids, "expected_request_revisions": second_revisions});
    late["action"] = json!("approve");
    assert!(
        dispatch(
            state,
            JsonRpcRequest::new(1, methods::RESOLVE_PERMISSION_REQUESTS, late)
        )
        .await
        .error
        .is_some()
    );
    assert!(!outputs.path().join("second-a").exists());
    assert!(!outputs.path().join("second-b").exists());
}

#[test]
fn rebinding_does_not_recover_lost_turns_or_replay_their_consumed_input() {
    let dir = tempfile::tempdir().unwrap();
    let flow = runtime().block_on(async {
        let (state, flow, mut launch) = setup(&dir, r#"flow main(label: string) -> string {
            when label == "hold" { user_confirm("waiting") }
            return final.answer(message: label)
        }"#).await;
        launch["arguments"]["label"] = json!("hold");
        rpc(&state, methods::FLOW_LAUNCH, launch).await;
        wait_prompt(&state).await;
        let active = rpc(&state, methods::FLOW_GET, json!({"flow": flow})).await;
        rpc(&state, methods::FLOW_CONTROL, json!({"request_id": Uuid::now_v7(), "flow": flow, "expected_revision": active["execution"]["revision"], "action": "pause"})).await;
        rpc(&state, methods::FLOW_INVOKE, json!({"flow": flow, "request_id": Uuid::now_v7(), "arguments": {"label": "after recovery"}})).await;
        flow
    });
    runtime().block_on(async {
        let state = Arc::new(DaemonState::new(dir.path().join("data")));
        rpc(&state, methods::SESSION_OPEN, json!({"session_id": flow["session_id"]})).await;
        let request = json!({"flow": flow});
        let opened = rpc(&state, methods::FLOW_GET, request.clone()).await;
        assert_eq!(opened["execution"]["state"]["open"]["interrupted"]["reason"], "host_lost");
        assert_eq!(opened["execution"]["pause_count"], 1);
        rpc(&state, methods::FLOW_BIND, request.clone()).await;
        let bound = rpc(&state, methods::FLOW_GET, request.clone()).await;
        assert_eq!(bound["execution"], opened["execution"]);
        assert_eq!(bound["last_completion"], opened["last_completion"]);
        rpc(&state, methods::FLOW_CONTROL, json!({"request_id": Uuid::now_v7(), "flow": flow, "expected_revision": bound["execution"]["revision"], "action": "recover"})).await;
        let recovered = rpc(&state, methods::FLOW_GET, request.clone()).await;
        assert_eq!(recovered["execution"]["state"]["open"], "idle");
        assert_eq!(recovered["execution"]["pause_count"], 1);
        assert_eq!(recovered["execution"]["can_start_turn"], false);
        let stale = dispatch(state.clone(), JsonRpcRequest::new(1, methods::FLOW_CONTROL, json!({"request_id": Uuid::now_v7(), "flow": flow, "expected_revision": bound["execution"]["revision"], "action": "recover"}))).await;
        assert!(stale.error.is_some());
        rpc(&state, methods::FLOW_CONTROL, json!({"request_id": Uuid::now_v7(), "flow": flow, "expected_revision": recovered["execution"]["revision"], "action": "resume"})).await;
        wait_completion(&state, &flow, Some(&opened["last_completion"]["lease"]["turn"])).await;
        let output = rpc(&state, methods::FLOW_OUTPUT, request).await;
        assert_eq!(output["messages"].as_array().unwrap().len(), 1);
        assert_eq!(answer(&output, 0), "after recovery");
        assert!(state.pending_prompt_ids().is_empty(), "lost prompt must not be replayed");
    });
}
