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
async fn sse_missing_auth_returns_401() {
    let tmp = tempfile::tempdir().unwrap();
    let state = build_state(&tmp);
    let app = router(state);
    let sid = Uuid::now_v7();
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/events?session_id={sid}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn sse_streams_existing_and_appended_events() {
    let tmp = tempfile::tempdir().unwrap();
    let sid = Uuid::now_v7();
    let sdir = tmp.path().join("sessions").join(sid.to_string());
    std::fs::create_dir_all(&sdir).unwrap();
    let events_path = sdir.join("events.jsonl");
    std::fs::write(
        &events_path,
        "{\"type\":\"flow_start\",\"seq\":1}\n{\"type\":\"llm_call\",\"seq\":2}\n",
    )
    .unwrap();

    let state = build_state(&tmp);
    let app = router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/events?session_id={sid}"))
                .header("Authorization", "Bearer secret")
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
            Ok(_) => break,
            Err(_) => {
                if buf.windows(2).filter(|w| w == b"\n\n").count() >= 2 {
                    break;
                }
            }
        }
    }
    let text = String::from_utf8_lossy(&buf);
    assert!(
        text.contains("flow_start"),
        "expected flow_start in SSE stream: {text}"
    );
    assert!(
        text.contains("llm_call"),
        "expected llm_call in SSE stream: {text}"
    );
    assert!(text.contains("event: event"));
}

async fn collect_sse_body(app: axum::Router, uri: String, header: Option<(&str, &str)>) -> String {
    let mut req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("Authorization", "Bearer secret");
    if let Some((k, v)) = header {
        req = req.header(k, v);
    }
    let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mut body = resp.into_body().into_data_stream();
    let mut buf = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(300), body.next()).await {
            Ok(Some(Ok(chunk))) => buf.extend_from_slice(&chunk),
            Ok(_) => break,
            Err(_) => {
                if buf.windows(2).filter(|w| w == b"\n\n").count() >= 2 {
                    break;
                }
            }
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn seed_five_events(tmp: &tempfile::TempDir) -> Uuid {
    let sid = Uuid::now_v7();
    let sdir = tmp.path().join("sessions").join(sid.to_string());
    std::fs::create_dir_all(&sdir).unwrap();
    let mut jsonl = String::new();
    for i in 1..=5 {
        jsonl.push_str(&format!("{{\"type\":\"evt{i}\",\"seq\":{i}}}\n"));
    }
    std::fs::write(sdir.join("events.jsonl"), jsonl).unwrap();
    sid
}

#[tokio::test]
async fn sse_honors_last_event_id_header_when_no_since_seq_query() {
    let tmp = tempfile::tempdir().unwrap();
    let sid = seed_five_events(&tmp);
    let app = router(build_state(&tmp));
    let text = collect_sse_body(
        app,
        format!("/events?session_id={sid}"),
        Some(("Last-Event-ID", "3")),
    )
    .await;
    assert!(!text.contains("\"evt1\""), "unexpected evt1 in {text}");
    assert!(!text.contains("\"evt3\""), "unexpected evt3 in {text}");
    assert!(text.contains("\"evt4\""), "expected evt4 in {text}");
    assert!(text.contains("\"evt5\""), "expected evt5 in {text}");
}

#[tokio::test]
async fn sse_since_seq_query_wins_over_last_event_id_header() {
    let tmp = tempfile::tempdir().unwrap();
    let sid = seed_five_events(&tmp);
    let app = router(build_state(&tmp));
    let text = collect_sse_body(
        app,
        format!("/events?session_id={sid}&since_seq=4"),
        Some(("Last-Event-ID", "1")),
    )
    .await;
    assert!(!text.contains("\"evt4\""), "since_seq=4 should skip evt4");
    assert!(text.contains("\"evt5\""), "expected evt5 in {text}");
}

#[tokio::test]
async fn sse_first_frame_advertises_retry_directive() {
    let tmp = tempfile::tempdir().unwrap();
    let sid = seed_five_events(&tmp);
    let app = router(build_state(&tmp));
    let text = collect_sse_body(app, format!("/events?session_id={sid}"), None).await;
    assert!(
        text.contains("retry: 3000"),
        "expected retry: 3000 directive in {text}"
    );
}
