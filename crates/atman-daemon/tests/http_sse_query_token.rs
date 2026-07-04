use std::sync::Arc;
use std::time::Duration;

use atman_daemon::{
    DaemonState,
    http::{HttpState, router},
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::StreamExt;
use tower::ServiceExt;
use uuid::Uuid;

fn build_state(tmp: &tempfile::TempDir) -> Arc<HttpState> {
    let daemon = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    Arc::new(HttpState {
        daemon,
        auth_token: "secret".to_string(),
    })
}

#[tokio::test]
async fn sse_accepts_query_string_token_when_authorization_header_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let sid = Uuid::now_v7();
    let sdir = tmp.path().join("sessions").join(sid.to_string());
    std::fs::create_dir_all(&sdir).unwrap();
    std::fs::write(
        sdir.join("events.jsonl"),
        "{\"type\":\"flow_start\",\"seq\":1}\n",
    )
    .unwrap();

    let state = build_state(&tmp);
    let app = router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/events?session_id={sid}&token=secret"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let mut body = resp.into_body().into_data_stream();
    let mut buf = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(300), body.next()).await {
            Ok(Some(Ok(chunk))) => buf.extend_from_slice(&chunk),
            _ => break,
        }
    }
    let text = String::from_utf8_lossy(&buf);
    assert!(
        text.contains("flow_start"),
        "expected flow_start in SSE (query-token path). got: {text}"
    );
}

#[tokio::test]
async fn sse_rejects_wrong_query_token() {
    let tmp = tempfile::tempdir().unwrap();
    let sid = Uuid::now_v7();
    let state = build_state(&tmp);
    let app = router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/events?session_id={sid}&token=wrong"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
