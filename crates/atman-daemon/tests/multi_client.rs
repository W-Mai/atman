use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use atman_daemon::{
    DaemonState,
    config::DaemonConfig,
    http::{HttpState, router},
    run::RunLauncher,
};

#[tokio::test(flavor = "multi_thread")]
async fn multi_client_run_and_sse_stream_share_session() {
    let tmp = tempfile::tempdir().unwrap();
    let daemon = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    daemon.set_launcher(Arc::new(RunLauncher {
        project_root: std::env::current_dir().unwrap(),
        config_dir: None,
        home_dir: None,
    }));

    let cfg_path = tmp.path().join("daemon.toml");
    let cfg = DaemonConfig::load_or_init(&cfg_path).unwrap();
    let token = cfg.auth_token.clone();

    let http_state = Arc::new(HttpState {
        daemon: daemon.clone(),
        auth_token: token.clone(),
    });
    let port = 65099 + (std::process::id() % 400) as u16;
    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    let base = format!("http://{addr}");
    let shutdown = tokio_util::sync::CancellationToken::new();
    let sh_clone = shutdown.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, router(http_state))
            .with_graceful_shutdown(async move { sh_clone.cancelled().await })
            .await
            .unwrap();
    });

    let flow_path = repo_root().join("examples/hello.at");
    let client = reqwest::Client::new();

    // Client A — RUN_FLOW via HTTP with bearer.
    let run_resp = tokio::time::timeout(
        Duration::from_secs(5),
        client
            .post(format!("{base}/rpc"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "run_flow",
                "params": {"flow_path": flow_path.to_string_lossy()}
            }))
            .send(),
    )
    .await
    .expect("run_flow request timed out")
    .unwrap();
    assert_eq!(run_resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = run_resp.json().await.unwrap();
    let session_id = body["result"]["session_id"].as_str().unwrap().to_string();

    // Client B — SSE subscribe to /events for the same session.
    let sse = client
        .get(format!("{base}/events?session_id={session_id}"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(sse.status(), reqwest::StatusCode::OK);
    let mut body_bytes = Vec::new();
    let mut stream = sse.bytes_stream();
    use futures::StreamExt;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(Ok(chunk))) =
            tokio::time::timeout(Duration::from_millis(300), stream.next()).await
        {
            body_bytes.extend_from_slice(&chunk);
            let s = String::from_utf8_lossy(&body_bytes);
            if s.contains("flow_start") && s.contains("flow_end") {
                break;
            }
        }
    }
    let text = String::from_utf8_lossy(&body_bytes);
    assert!(
        text.contains("flow_start"),
        "sse missing flow_start: {text}"
    );
    assert!(text.contains("flow_end"), "sse missing flow_end: {text}");

    // Client C — LIST_SESSIONS sees the session too.
    let list_resp = client
        .post(format!("{base}/rpc"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "list_sessions"
        }))
        .send()
        .await
        .unwrap();
    let list_body: serde_json::Value = list_resp.json().await.unwrap();
    let arr = list_body["result"].as_array().unwrap();
    assert!(
        arr.iter().any(|s| s["id"].as_str() == Some(&session_id)),
        "expected session {session_id} in list: {arr:?}"
    );

    drop(stream);
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}
