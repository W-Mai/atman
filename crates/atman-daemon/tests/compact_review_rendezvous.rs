use std::sync::Arc;

use atman_daemon::{DaemonState, dispatch_as};
use atman_proto::{
    CompactReviewDecision, CompactReviewResolutionStatus, JsonRpcRequest, RequestId,
    ResolveCompactReviewRequest, ResolveCompactReviewResponse, SessionId, rpc,
};

fn pending_review(review_id: &str) -> atman_runtime::session::PendingCompactReview {
    atman_runtime::session::PendingCompactReview {
        review_id: review_id.into(),
        summary: "summary".into(),
        slice_preview: "preview".into(),
        slice_count: 3,
        range_start: 2,
        range_end: 5,
        tokens_before: 1_024,
        emitted_at: chrono::Utc::now(),
    }
}

async fn wait_for_review(state: &DaemonState, session_id: &SessionId, review_id: &str) {
    loop {
        let snapshot = state.session_snapshot(session_id, "alice").await.unwrap();
        if snapshot
            .projection
            .interactions
            .compact_review
            .as_ref()
            .is_some_and(|review| review.id == review_id)
        {
            return;
        }
        tokio::task::yield_now().await;
    }
}

async fn resolve(
    state: Arc<DaemonState>,
    principal: &str,
    rpc_id: u64,
    request: &ResolveCompactReviewRequest,
) -> atman_proto::JsonRpcResponse {
    dispatch_as(
        state,
        JsonRpcRequest::for_method::<rpc::ResolveCompactReview>(rpc_id, request).unwrap(),
        principal,
    )
    .await
}

#[tokio::test]
async fn compact_review_resolution_is_scoped_idempotent_and_convergent() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let session = Arc::new(atman_runtime::Session::open_ephemeral());
    let session_id = SessionId(session.id().0);
    state
        .register_session(session_id.clone(), session.clone(), "alice")
        .await
        .unwrap();

    let review_id = "review-1";
    let response = session.compact_reviews().request(pending_review(review_id));
    wait_for_review(&state, &session_id, review_id).await;
    let request = ResolveCompactReviewRequest {
        request_id: Some(RequestId::now()),
        session_id: session_id.clone(),
        review_id: review_id.into(),
        decision: CompactReviewDecision::AcceptEdited {
            summary: "edited summary".into(),
        },
    };

    let denied = resolve(state.clone(), "mallory", 1, &request).await;
    assert!(denied.error.is_some());
    assert!(session.compact_reviews().list_pending().is_some());

    let resolved = resolve(state.clone(), "alice", 2, &request).await;
    let resolved: ResolveCompactReviewResponse =
        serde_json::from_value(resolved.result.unwrap()).unwrap();
    assert!(resolved.resolved);
    assert_eq!(resolved.status, CompactReviewResolutionStatus::Resolved);
    assert_eq!(resolved.session_id, session_id);
    assert_eq!(resolved.review_id, review_id);
    assert!(matches!(
        response.await.unwrap(),
        atman_runtime::session::CompactReviewDecision::AcceptEdited { summary }
            if summary == "edited summary"
    ));
    let snapshot = state.session_snapshot(&session_id, "alice").await.unwrap();
    assert!(snapshot.projection.interactions.compact_review.is_none());
    assert_eq!(resolved.revision, snapshot.projection.revision);
    assert_eq!(resolved.cursor, snapshot.cursor);

    let retry = resolve(state.clone(), "alice", 3, &request).await;
    let retry: ResolveCompactReviewResponse =
        serde_json::from_value(retry.result.unwrap()).unwrap();
    assert_eq!(retry.status, CompactReviewResolutionStatus::Resolved);
    assert_eq!(retry.revision, resolved.revision);
    assert_eq!(retry.cursor, resolved.cursor);

    let competing = ResolveCompactReviewRequest {
        request_id: Some(RequestId::now()),
        ..request
    };
    let competing = resolve(state, "alice", 4, &competing).await;
    let competing: ResolveCompactReviewResponse =
        serde_json::from_value(competing.result.unwrap()).unwrap();
    assert!(!competing.resolved);
    assert_eq!(
        competing.status,
        CompactReviewResolutionStatus::AlreadyResolved
    );
}

#[tokio::test]
async fn replaced_compact_review_reports_abandoned() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let session = Arc::new(atman_runtime::Session::open_ephemeral());
    let session_id = SessionId(session.id().0);
    state
        .register_session(session_id.clone(), session.clone(), "alice")
        .await
        .unwrap();

    let first = session.compact_reviews().request(pending_review("first"));
    let _second = session.compact_reviews().request(pending_review("second"));
    assert!(matches!(
        first.await.unwrap(),
        atman_runtime::session::CompactReviewDecision::Reject
    ));
    wait_for_review(&state, &session_id, "second").await;

    let response = resolve(
        state,
        "alice",
        1,
        &ResolveCompactReviewRequest {
            request_id: Some(RequestId::now()),
            session_id,
            review_id: "first".into(),
            decision: CompactReviewDecision::Reject,
        },
    )
    .await;
    let response: ResolveCompactReviewResponse =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(response.status, CompactReviewResolutionStatus::Abandoned);
}
