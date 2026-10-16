use std::{sync::Arc, time::Duration};

use atman_daemon::{
    DaemonState, dispatch,
    http::{HttpState, router},
    run::RunLauncher,
};
use atman_proto::{JsonRpcRequest, methods};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
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
