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

    rpc(&state, methods::FLOW_CONTROL, json!({"flow": flow, "expected_revision": first["execution"]["revision"], "action": "pause"})).await;
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
    rpc(&state, methods::FLOW_CONTROL, json!({"flow": flow, "expected_revision": paused["execution"]["revision"], "action": "resume"})).await;
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
        rpc(&state, methods::FLOW_CONTROL, json!({"flow": flow, "expected_revision": first["execution"]["revision"], "action": "pause"})).await;
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
        rpc(&state, methods::FLOW_CONTROL, json!({"flow": flow, "expected_revision": paused["execution"]["revision"], "action": "resume"})).await;
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
    for params in [
        json!({"session_id": first["session_id"]}),
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
    rpc(&state, methods::FLOW_CONTROL, json!({"flow": second, "expected_revision": status["execution"]["revision"], "action": "cancel_turn"})).await;
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
        rpc(&state, methods::FLOW_CONTROL, json!({"flow": flow, "expected_revision": active["execution"]["revision"], "action": "pause"})).await;
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
        rpc(&state, methods::FLOW_CONTROL, json!({"flow": flow, "expected_revision": bound["execution"]["revision"], "action": "recover"})).await;
        let recovered = rpc(&state, methods::FLOW_GET, request.clone()).await;
        assert_eq!(recovered["execution"]["state"]["open"], "idle");
        assert_eq!(recovered["execution"]["pause_count"], 1);
        assert_eq!(recovered["execution"]["can_start_turn"], false);
        let stale = dispatch(state.clone(), JsonRpcRequest::new(1, methods::FLOW_CONTROL, json!({"flow": flow, "expected_revision": bound["execution"]["revision"], "action": "recover"}))).await;
        assert!(stale.error.is_some());
        rpc(&state, methods::FLOW_CONTROL, json!({"flow": flow, "expected_revision": recovered["execution"]["revision"], "action": "resume"})).await;
        wait_completion(&state, &flow, Some(&opened["last_completion"]["lease"]["turn"])).await;
        let output = rpc(&state, methods::FLOW_OUTPUT, request).await;
        assert_eq!(output["messages"].as_array().unwrap().len(), 1);
        assert_eq!(answer(&output, 0), "after recovery");
        assert!(state.pending_prompt_ids().is_empty(), "lost prompt must not be replayed");
    });
}
