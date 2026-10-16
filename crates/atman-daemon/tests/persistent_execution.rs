use std::{sync::Arc, time::Duration};

use atman_daemon::{DaemonState, dispatch, run::RunLauncher};
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
    let prompt = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(prompt) = state.pending_prompt_ids().first() {
                return prompt.clone();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
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
