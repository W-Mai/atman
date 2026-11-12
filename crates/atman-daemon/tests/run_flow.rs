use atman_daemon::{DaemonState, dispatch, run::RunLauncher};
use atman_proto::{JsonRpcRequest, methods};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use uuid::Uuid;

async fn rpc(state: &Arc<DaemonState>, method: &str, params: Value) -> Value {
    let response = dispatch(state.clone(), JsonRpcRequest::new(1, method, params)).await;
    assert!(response.error.is_none(), "{method}: {:?}", response.error);
    response.result.unwrap()
}

fn address(run: &atman_daemon::run::SpawnedRun) -> Value {
    json!({"session_id": run.session_id, "instance_id": run.instance_id})
}

async fn completed(state: &Arc<DaemonState>, flow: &Value, previous: Option<&Value>) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
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
    .expect("persistent entry should finish")
}

#[tokio::test(flavor = "multi_thread")]
async fn goal_run_uses_explicit_continuations_and_resumes_the_retained_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config");
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("config.toml"), "[trust]\nmode = \"reckless\"\n").unwrap();
    atman_runtime::templates::ensure_managed_agent_at(&config).unwrap();
    let path = tmp.path().join("goal.at");
    std::fs::write(&path, r#"use "user:agent-delivery.at"::deliver
    flow main(prompt: string, model: string = "unused") -> string {
        contract { invocation { goal: true, user_message: prompt } }
        snapshot = memory.goal.get()
        when snapshot.goal.objective == "first" {
            memory.goal.set(text: "second", goal_id: snapshot.goal.id, expected_revision: snapshot.goal.revision)
            return deliver("first turn").await
        }
        memory.goal.pause(goal_id: snapshot.goal.id, expected_revision: snapshot.goal.revision, reason: "completed", summary: "Finished")
        return deliver("finished").await
    }"#).unwrap();
    let state = Arc::new(DaemonState::new(data.clone()));
    state.set_launcher(Arc::new(
        RunLauncher::new(tmp.path().into(), Some(config.clone()), None).unwrap(),
    ));
    let launched = rpc(
        &state,
        methods::GOAL_RUN,
        json!({"flow_path": path, "args": {"model": "not-the-goal", "prompt": "first"}}),
    )
    .await;
    assert!(launched["goal_id"].is_string());
    let flow =
        json!({"session_id": launched["session_id"], "instance_id": launched["instance_id"]});
    let first = completed(&state, &flow, None).await;
    let second = if first["goal"]["status"] == "paused" {
        first
    } else {
        completed(
            &state,
            &flow,
            Some(&first["last_completion"]["lease"]["turn"]),
        )
        .await
    };
    assert_eq!(second["goal"]["objective"], "second");
    assert_eq!(second["goal"]["pause"]["summary"], "Finished");
    assert_eq!(second["goal"]["id"], launched["goal_id"]);
    let resume = json!({"session_id": flow["session_id"], "instance_id": flow["instance_id"], "request_id": Uuid::now_v7(),
        "goal_id": second["goal"]["id"], "expected_revision": second["goal"]["revision"]});
    let accepted = rpc(&state, methods::GOAL_RESUME, resume.clone()).await;
    let third = completed(
        &state,
        &flow,
        Some(&second["last_completion"]["lease"]["turn"]),
    )
    .await;
    assert_eq!(third["goal"]["status"], "paused");
    assert_eq!(
        rpc(&state, methods::GOAL_RESUME, resume).await["committed"],
        accepted["committed"]
    );
    let output = rpc(&state, methods::FLOW_OUTPUT, json!({"flow": flow})).await;
    let page: atman_proto::responses::FlowOutputPage =
        serde_json::from_value(output.clone()).expect("typed flow.output page");
    assert!(!page.has_more);
    assert!(page.next.is_some());
    assert_eq!(output["messages"].as_array().unwrap().len(), 3);
    let turns: std::collections::HashSet<_> = output["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|record| {
            record["publication"]["message"]["turn_id"]
                .as_str()
                .unwrap()
        })
        .collect();
    assert_eq!(turns.len(), 3);
    state.shutdown().await.unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_file(config.join("lib/agent-delivery.at")).unwrap();
    let reopened = Arc::new(DaemonState::new(data));
    rpc(
        &reopened,
        methods::SESSION_OPEN,
        json!({"session_id": flow["session_id"]}),
    )
    .await;
    assert_eq!(
        rpc(&reopened, methods::FLOW_OUTPUT, json!({"flow": flow})).await,
        output
    );
    rpc(&reopened, methods::FLOW_BIND, json!({"flow": flow})).await;
    rpc(&reopened, methods::GOAL_RESUME, json!({"session_id": flow["session_id"], "instance_id": flow["instance_id"],
        "request_id": Uuid::now_v7(), "goal_id": third["goal"]["id"], "expected_revision": third["goal"]["revision"]})).await;
    let restored = completed(
        &reopened,
        &flow,
        Some(&third["last_completion"]["lease"]["turn"]),
    )
    .await;
    assert_eq!(restored["goal"]["status"], "paused");
    assert_eq!(
        rpc(&reopened, methods::FLOW_OUTPUT, json!({"flow": flow})).await["messages"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    reopened.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn launcher_uses_injected_config_data_and_home_without_registration() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config");
    let data = tmp.path().join("data");
    let project = tmp.path().join("project");
    let home = tmp.path().join("home");
    for path in [&config, &project, &home.join(".claude")] {
        std::fs::create_dir_all(path).unwrap();
    }
    std::fs::write(
        config.join("config.toml"),
        "[storage]\nscope = \"global\"\n",
    )
    .unwrap();
    std::fs::write(
        home.join(".claude/CLAUDE.md"),
        "# injected-home\nRetained home rules\n",
    )
    .unwrap();
    std::fs::write(config.join("hooks.at"), r#"
        on session.start { memory.todo.set(where: "startup", why: "container", how: "hook", expected_result: "once") }
        on session.end { memory.todo.set(where: "unexpected shutdown", why: "container", how: "hook", expected_result: "never") }
    "#).unwrap();
    std::fs::write(
        project.join("main.at"),
        "flow main() -> string { return rule.fetch(name: \"injected-home\") }",
    )
    .unwrap();
    let state = Arc::new(DaemonState::new(data.clone()));
    let expected = atman_runtime::storage::ProjectContext::resolve(
        &atman_runtime::config_hub::ConfigHub::from_config_dir(&config),
        &project,
        &data,
    )
    .unwrap();
    let launcher = RunLauncher::new(project.clone(), Some(config), Some(home)).unwrap();
    let run = launcher
        .spawn(state.clone(), "main.at", vec![])
        .await
        .unwrap();
    let view = completed(&state, &address(&run), None).await;
    assert!(
        view["last_completion"]["result"]["value"]["value"]
            .as_str()
            .unwrap()
            .contains("Retained home rules")
    );
    assert!(state.launcher().is_none());
    assert!(!project.join(".atman").exists());
    state.shutdown().await.unwrap();
    let root = atman_runtime::flow_store::RuntimeStore::open(&data).unwrap();
    let session = root.open_session(run.session_id.0).unwrap();
    assert_eq!(
        session.projection().descriptor.project.as_ref(),
        Some(&expected)
    );
    let flow = session
        .open_flow(&atman_rt::FlowInstanceId(run.instance_id.into()))
        .unwrap();
    assert_eq!(flow.planning().todos.len(), 1);
    assert_eq!(flow.planning().todos[0].where_, "startup");
}

#[tokio::test(flavor = "multi_thread")]
async fn launcher_retains_used_sources_after_return_and_rejects_unknown_principals() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    let library = project.join(".atman/lib");
    std::fs::create_dir_all(&library).unwrap();
    let helper = library.join("helper.at");
    std::fs::write(&helper, "pub flow helper() -> string { return \"ok\" }\n").unwrap();
    let entry = project.join("main.at");
    std::fs::write(
        &entry,
        "use \"project:helper.at\"::helper\nflow main() -> string { return helper().await }\n",
    )
    .unwrap();
    let data = tmp.path().join("data");
    let state = Arc::new(DaemonState::new(data.clone()));
    let launcher = RunLauncher::new(project, Some(tmp.path().join("config")), None).unwrap();
    assert!(
        launcher
            .spawn_as(
                state.clone(),
                entry.to_str().unwrap(),
                vec![],
                "foreign-user"
            )
            .await
            .is_err()
    );
    assert!(
        atman_runtime::flow_store::RuntimeStore::session_ids(&data)
            .unwrap()
            .is_empty()
    );
    let run = launcher
        .spawn(state.clone(), entry.to_str().unwrap(), vec![])
        .await
        .unwrap();
    let flow = address(&run);
    let first = completed(&state, &flow, None).await;
    std::fs::remove_file(entry).unwrap();
    std::fs::remove_file(helper).unwrap();
    rpc(
        &state,
        methods::FLOW_INVOKE,
        json!({"flow": flow, "request_id": Uuid::now_v7()}),
    )
    .await;
    let second = completed(
        &state,
        &flow,
        Some(&first["last_completion"]["lease"]["turn"]),
    )
    .await;
    assert_eq!(second["last_completion"]["result"]["value"]["value"], "ok");
    assert_eq!(
        rpc(&state, methods::FLOW_OUTPUT, json!({"flow": flow})).await["messages"],
        json!([])
    );
    assert_eq!(
        rpc(&state, methods::CANCEL_RUN, json!({"run_id": run.run_id})).await["cancelled"],
        false
    );
    state.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn run_flow_rpc_retains_images_effort_history_and_default_flow() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().join("data")));
    state.set_launcher(Arc::new(
        RunLauncher::new(tmp.path().into(), Some(tmp.path().join("config")), None).unwrap(),
    ));
    let source = tmp.path().join("main.at");
    std::fs::write(
        &source,
        "flow main() -> string { return final.answer(message: env(\"effort\")) }",
    )
    .unwrap();
    let run = rpc(
        &state,
        methods::RUN_FLOW,
        json!({"flow_path": source, "reasoning": "high@pro",
        "images": [{"data_base64": "iVBORw0KGgo=", "name": "daemon-input.png"}]}),
    )
    .await;
    let flow = json!({"session_id": run["session_id"], "instance_id": run["instance_id"]});
    let view = completed(&state, &flow, None).await;
    assert_eq!(view["last_completion"]["lease"]["run"], run["run_id"]);
    assert_eq!(
        view["last_completion"]["result"]["value"]["value"],
        "high@pro"
    );
    let history = rpc(
        &state,
        methods::FLOW_EVENTS,
        json!({"flow": flow, "limit": 128}),
    )
    .await;
    let inputs: Vec<_> = history["batches"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|b| b["facts"].as_array().unwrap())
        .filter(|fact| fact["type"] == "input_committed")
        .map(|fact| &fact["data"]["content"])
        .collect();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0]["invocation"]["effort"], "high@pro");
    let message: atman_runtime::message::Message =
        serde_json::from_value(inputs[0]["messages"][0].clone()).unwrap();
    assert_eq!(message.origin, atman_runtime::message::MessageOrigin::User);
    let image = atman_runtime::attachment_store::AttachmentStore::at(std::path::Path::new(""))
        .import_base64("iVBORw0KGgo=", Some("daemon-input.png"))
        .unwrap();
    assert!(
        matches!(&message.parts[1], atman_runtime::message::MessagePart::Image { source } if source == &image)
    );
    let sessions = rpc(&state, methods::LIST_SESSIONS, json!({})).await;
    assert!(
        sessions
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == run["session_id"])
    );
    let catalog = rpc(
        &state,
        methods::SESSION_OPEN,
        json!({"session_id": run["session_id"]}),
    )
    .await;
    assert_eq!(catalog["default_flow"], run["instance_id"]);
    state.shutdown().await.unwrap();
}
