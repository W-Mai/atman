use std::sync::Arc;
use std::time::Duration;

use atman_daemon::LiveRun;
use atman_daemon::{
    DaemonState,
    http::{EventTicketResponse, HttpState, router},
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uuid::Uuid;

fn build_state(tmp: &tempfile::TempDir) -> Arc<HttpState> {
    let daemon = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    Arc::new(HttpState {
        daemon,
        auth_token: "secret".to_string(),
    })
}

async fn issue_ticket(app: &axum::Router, session_id: Uuid) -> EventTicketResponse {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/event-ticket")
                .header("Authorization", "Bearer secret")
                .header("Content-Type", "application/json")
                .body(Body::from(format!(r#"{{"session_id":"{session_id}"}}"#)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn sse_accepts_a_short_lived_session_ticket() {
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
    let session = Arc::new(atman_runtime::Session::open_ephemeral());
    state
        .daemon
        .register_session_run(
            atman_proto::SessionId(sid),
            session,
            LiveRun {
                turn_id: atman_runtime::event::TurnId::now(),
                run_id: atman_proto::FlowRunId(Uuid::now_v7()),
                flow_name: "sse-ticket-test".into(),
                cancel: CancellationToken::new(),
                started_at: Utc::now(),
            },
            "authenticated-daemon-client",
        )
        .await
        .unwrap();
    let app = router(state);
    let ticket = issue_ticket(&app, sid).await;
    assert_eq!(ticket.session_id, atman_proto::SessionId(sid));
    assert!(ticket.expires_at_unix > 0);

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/events?session_id={sid}&ticket={}", ticket.ticket))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let mut body = response.into_body().into_data_stream();
    let mut buffer = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(300), body.next()).await {
            Ok(Some(Ok(chunk))) => buffer.extend_from_slice(&chunk),
            _ => break,
        }
    }
    let text = String::from_utf8_lossy(&buffer);
    assert!(
        text.contains("flow_start"),
        "expected flow_start in SSE ticket stream, got: {text}"
    );
}

#[tokio::test]
async fn sse_rejects_long_lived_query_tokens_and_cross_session_tickets() {
    let tmp = tempfile::tempdir().unwrap();
    let sid = Uuid::now_v7();
    let other_sid = Uuid::now_v7();
    let state = build_state(&tmp);
    let sessions_root = tmp.path().join("sessions");
    std::fs::create_dir_all(sessions_root.join(sid.to_string())).unwrap();
    std::fs::create_dir_all(sessions_root.join(other_sid.to_string())).unwrap();
    std::fs::write(sessions_root.join(sid.to_string()).join("events.jsonl"), "").unwrap();
    std::fs::write(
        sessions_root
            .join(other_sid.to_string())
            .join("events.jsonl"),
        "",
    )
    .unwrap();
    let app = router(state);
    let ticket = issue_ticket(&app, sid).await;

    for uri in [
        format!("/events?session_id={sid}&token=secret"),
        format!("/events?session_id={other_sid}&ticket={}", ticket.ticket),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
