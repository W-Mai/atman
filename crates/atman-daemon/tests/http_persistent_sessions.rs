use std::{sync::Arc, time::Duration};

use atman_daemon::{
    DaemonState, dispatch,
    http::{HttpState, router},
    run::RunLauncher,
};
use atman_proto::flows::FlowCursor;
use atman_proto::{JsonRpcRequest, methods};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use futures::StreamExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

async fn http_rpc(app: &Router, token: &str, method: &str, params: Value) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::post("/rpc")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::from(
                    serde_json::to_vec(&JsonRpcRequest::new(1, method, params)).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let json = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, json)
}

async fn rpc(app: &Router, method: &str, params: Value) -> Value {
    let (status, response) = http_rpc(app, "secret", method, params).await;
    assert_eq!(status, StatusCode::OK);
    assert!(response["error"].is_null(), "{method}: {response}");
    response["result"].clone()
}

async fn completed(app: &Router, flow: &Value, previous: Option<&Value>) -> Value {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let view = rpc(app, methods::FLOW_GET, json!({"flow": flow})).await;
            let completion = &view["last_completion"];
            if !completion.is_null() && previous != Some(&completion["lease"]["turn"]) {
                assert_eq!(completion["result"]["kind"], "returned", "{view}");
                return view;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap()
}

async fn events(app: &Router, uri: &str, after: Option<&str>) -> axum::response::Response {
    let mut request = Request::get(uri);
    if let Some(cursor) = after {
        request = request.header("Last-Event-ID", cursor);
    }
    app.clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn batches_through(response: axum::response::Response, through: &Value) -> Vec<Value> {
    assert_eq!(response.status(), StatusCode::OK);
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut stream = response.into_body().into_data_stream();
        let mut buffer = String::new();
        let mut batches = Vec::new();
        while let Some(chunk) = stream.next().await {
            buffer.push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
            while let Some(end) = buffer.find("\n\n") {
                let frame = buffer.drain(..end + 2).collect::<String>();
                if !frame.lines().any(|line| line == "event: flow_update") {
                    continue;
                }
                let data = frame
                    .lines()
                    .find_map(|line| line.strip_prefix("data: "))
                    .unwrap();
                let id = frame
                    .lines()
                    .find_map(|line| line.strip_prefix("id: "))
                    .unwrap();
                let batch: Value = serde_json::from_str(data).unwrap();
                let receipt: FlowCursor = serde_json::from_value(batch["receipt"].clone()).unwrap();
                assert_eq!(id.parse::<FlowCursor>().unwrap(), receipt);
                let finished = &batch["receipt"] == through;
                batches.push(batch);
                if finished {
                    return batches;
                }
            }
        }
        panic!("event stream ended before requested commit");
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn instance_sse_and_rpc_share_atomic_history_across_pause_and_reconnection() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config");
    std::fs::create_dir(&config).unwrap();
    let daemon = Arc::new(DaemonState::new(dir.path().join("data")));
    daemon.set_launcher(Arc::new(
        RunLauncher::new(dir.path().to_path_buf(), Some(config), None).unwrap(),
    ));
    let app = router(Arc::new(HttpState {
        daemon,
        auth_token: "secret".into(),
    }));
    let id = Uuid::now_v7();
    let created = rpc(
        &app,
        methods::SESSION_CREATE,
        json!({"session_id": id, "title": "Instance view", "project_root": dir.path()}),
    )
    .await;
    let path = dir.path().join("view.at");
    std::fs::write(
        &path,
        "flow main(label: string) -> string { return final.answer(message: label) }",
    )
    .unwrap();
    let flow = json!({"session_id": id, "instance_id": Uuid::now_v7()});
    rpc(&app, methods::FLOW_LAUNCH, json!({"flow": flow, "request_id": Uuid::now_v7(), "expected_catalog_sequence": created["catalog"]["sequence"], "path": path, "entry": "main", "arguments": {"label": "first"}})).await;
    let first = completed(&app, &flow, None).await;
    let initial: FlowCursor = serde_json::from_value(first["cursor"].clone()).unwrap();
    let uri = format!(
        "/events?session_id={id}&instance_id={}&token=secret",
        flow["instance_id"].as_str().unwrap()
    );
    let stream = events(&app, &uri, Some(&initial.to_string())).await;
    assert_eq!(stream.status(), StatusCode::OK);
    // Registration has completed; no body polling is needed to retain later commits.
    rpc(&app, methods::FLOW_CONTROL, json!({"flow": flow, "expected_revision": first["execution"]["revision"], "action": "pause"})).await;
    rpc(
        &app,
        methods::FLOW_INVOKE,
        json!({"flow": flow, "request_id": Uuid::now_v7(), "arguments": {"label": "second"}}),
    )
    .await;
    let paused = rpc(&app, methods::FLOW_GET, json!({"flow": flow})).await;
    assert_eq!(paused["last_completion"], first["last_completion"]);
    let paused_cursor: FlowCursor = serde_json::from_value(paused["cursor"].clone()).unwrap();
    // Another reader can disconnect while input is queued without cancelling it.
    let detached = events(&app, &uri, Some(&paused_cursor.to_string())).await;
    assert_eq!(detached.status(), StatusCode::OK);
    drop(detached);
    assert_eq!(
        rpc(&app, methods::FLOW_GET, json!({"flow": flow})).await["execution"],
        paused["execution"]
    );
    rpc(&app, methods::FLOW_CONTROL, json!({"flow": flow, "expected_revision": paused["execution"]["revision"], "action": "resume"})).await;
    let second = completed(
        &app,
        &flow,
        Some(&first["last_completion"]["lease"]["turn"]),
    )
    .await;
    let live = batches_through(stream, &second["cursor"]).await;
    assert!(
        live.iter()
            .any(|batch| batch["facts"].as_array().unwrap().len() > 1)
    );
    assert_eq!(
        live.iter()
            .flat_map(|batch| batch["facts"].as_array().unwrap())
            .filter(|fact| fact["type"] == "turn_completed")
            .count(),
        1
    );
    let mut history = Vec::new();
    let mut after = first["cursor"].clone();
    loop {
        let page = rpc(
            &app,
            methods::FLOW_EVENTS,
            json!({"flow": flow, "after": after, "limit": 2}),
        )
        .await;
        assert!(page["batches"].as_array().unwrap().len() <= 2);
        history.extend(page["batches"].as_array().unwrap().iter().cloned());
        after = page["next"].clone();
        if page["has_more"] == false {
            break;
        }
    }
    assert_eq!(history, live);
    assert_eq!(after, second["cursor"]);
    let empty = rpc(
        &app,
        methods::FLOW_EVENTS,
        json!({"flow": flow, "after": after}),
    )
    .await;
    assert_eq!(empty["batches"], json!([]));
    assert_eq!(empty["next"], after);
    for (index, batch) in history.iter().enumerate() {
        assert_eq!(
            batch["receipt"]["sequence"].as_u64().unwrap(),
            initial.sequence + index as u64 + 1
        );
    }
    let middle: FlowCursor = serde_json::from_value(history[0]["receipt"].clone()).unwrap();
    let resumed = events(&app, &uri, Some(&middle.to_string())).await;
    assert_eq!(
        batches_through(resumed, &second["cursor"]).await,
        history[1..]
    );
    let explicit = events(&app, &format!("{uri}&after={middle}"), None).await;
    assert_eq!(
        batches_through(explicit, &second["cursor"]).await,
        history[1..]
    );
    let automatic = events(
        &app,
        &format!("{uri}&after={initial}"),
        Some(&middle.to_string()),
    )
    .await;
    assert_eq!(
        batches_through(automatic, &second["cursor"]).await,
        history[1..]
    );

    for bad in [
        FlowCursor {
            journal_id: Uuid::new_v4(),
            ..initial.clone()
        },
        FlowCursor {
            sequence: u64::MAX,
            ..initial.clone()
        },
        FlowCursor {
            digest: "0".repeat(64),
            ..initial.clone()
        },
    ] {
        assert_eq!(
            events(&app, &uri, Some(&bad.to_string())).await.status(),
            StatusCode::BAD_REQUEST
        );
        assert!(
            !http_rpc(
                &app,
                "secret",
                methods::FLOW_EVENTS,
                json!({"flow": flow, "after": bad})
            )
            .await
            .1["error"]
                .is_null()
        );
    }
    for invalid in ["1", "", "bad-cursor"] {
        assert_eq!(
            events(&app, &uri, Some(invalid)).await.status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        events(&app, &format!("{uri}&since_seq=1"), None)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        events(&app, &uri.replace("token=secret", "token=wrong"), None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    for limit in [0, 129] {
        assert_eq!(
            http_rpc(
                &app,
                "secret",
                methods::FLOW_EVENTS,
                json!({"flow": flow, "limit": limit})
            )
            .await
            .1["error"]["code"],
            -32602
        );
    }
    // Rejected reads cannot alter execution or poison the valid reader.
    assert_eq!(
        rpc(&app, methods::FLOW_GET, json!({"flow": flow})).await["execution"],
        second["execution"]
    );
    assert_eq!(
        rpc(
            &app,
            methods::FLOW_EVENTS,
            json!({"flow": flow, "after": second["cursor"]})
        )
        .await["batches"],
        json!([])
    );
}

#[tokio::test]
async fn authenticated_transports_share_persistent_execution_ownership() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config");
    std::fs::create_dir(&config).unwrap();
    let daemon = Arc::new(DaemonState::new(dir.path().join("data")));
    daemon.set_launcher(Arc::new(
        RunLauncher::new(dir.path().to_path_buf(), Some(config), None).unwrap(),
    ));
    let app = router(Arc::new(HttpState {
        daemon: daemon.clone(),
        auth_token: "secret".into(),
    }));
    let id = Uuid::now_v7();
    let create = json!({"session_id": id, "title": "HTTP execution", "project_root": dir.path()});
    assert_eq!(
        http_rpc(&app, "wrong", methods::SESSION_CREATE, create.clone())
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert!(
        !dir.path()
            .join("data/sessions")
            .join(id.to_string())
            .exists()
    );
    let created = rpc(&app, methods::SESSION_CREATE, create).await;
    let opened = dispatch(
        daemon,
        JsonRpcRequest::new(2, methods::SESSION_OPEN, json!({"session_id": id})),
    )
    .await;
    assert!(opened.error.is_none(), "{:?}", opened.error);
    assert_eq!(opened.result.unwrap()["catalog"], created["catalog"]);

    let path = dir.path().join("main.at");
    std::fs::write(
        &path,
        "flow main() -> string { return final.answer(message: \"delivered\") }",
    )
    .unwrap();
    let flow = json!({"session_id": id, "instance_id": Uuid::now_v7()});
    rpc(&app, methods::FLOW_LAUNCH, json!({"flow": flow, "request_id": Uuid::now_v7(), "expected_catalog_sequence": created["catalog"]["sequence"], "path": path, "entry": "main"})).await;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let view = rpc(&app, methods::FLOW_GET, json!({"flow": flow})).await;
            if !view["last_completion"].is_null() {
                assert_eq!(
                    view["last_completion"]["result"]["kind"], "returned",
                    "{view}"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let output = rpc(&app, methods::FLOW_OUTPUT, json!({"flow": flow})).await;
    let message: atman_runtime::message::Message =
        serde_json::from_value(output["messages"][0]["publication"]["message"].clone()).unwrap();
    assert_eq!(message.text_concat(), "delivered");
    assert!(!output["has_more"].as_bool().unwrap());
}
