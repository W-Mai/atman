use std::sync::Arc;

use atman_daemon::{DaemonState, dispatch_as};
use atman_proto::{
    CompactReviewDecision, CompactReviewResolutionStatus, JsonRpcRequest, RequestId,
    ResolveCompactReviewRequest, ResolveCompactReviewResponse, SessionId, rpc,
};

fn pending_review(review_id: &str) -> atman_runtime::session::PendingCompactReview {
    atman_runtime::session::PendingCompactReview {
        review_id: review_id.into(),
        context_id: None,
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
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let snapshot = state.session_snapshot(session_id, "alice").await.unwrap();
            if snapshot
                .projection
                .interactions
                .compact_reviews
                .iter()
                .any(|review| review.id == review_id)
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("review was not projected");
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
    assert_eq!(session.compact_reviews().list_pending().len(), 1);

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
    assert!(snapshot.projection.interactions.compact_reviews.is_empty());
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
async fn cancelled_compact_review_reports_abandoned_without_replacing_others() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let session = Arc::new(atman_runtime::Session::open_ephemeral());
    let session_id = SessionId(session.id().0);
    state
        .register_session(session_id.clone(), session.clone(), "alice")
        .await
        .unwrap();

    let first_context = session
        .context()
        .fork(atman_runtime::event::ContextInheritance::Full)
        .unwrap();
    let second_context = session
        .context()
        .fork(atman_runtime::event::ContextInheritance::Full)
        .unwrap();
    let before = state.session_snapshot(&session_id, "alice").await.unwrap();
    let mut first_review = pending_review("first");
    first_review.context_id = first_context.context_id().cloned();
    let mut second_review = pending_review("second");
    second_review.context_id = second_context.context_id().cloned();
    let first = session.compact_reviews().request(first_review);
    let _second = session.compact_reviews().request(second_review);
    wait_for_review(&state, &session_id, "first").await;
    wait_for_review(&state, &session_id, "second").await;
    drop(first);
    assert_eq!(session.compact_reviews().list_pending().len(), 1);

    let response = resolve(
        state.clone(),
        "alice",
        1,
        &ResolveCompactReviewRequest {
            request_id: Some(RequestId::now()),
            session_id: session_id.clone(),
            review_id: "first".into(),
            decision: CompactReviewDecision::Reject,
        },
    )
    .await;
    let response: ResolveCompactReviewResponse =
        serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(response.status, CompactReviewResolutionStatus::Abandoned);
    let snapshot = state.session_snapshot(&session_id, "alice").await.unwrap();
    assert_eq!(snapshot.projection.interactions.compact_reviews.len(), 1);
    assert_eq!(
        snapshot.projection.interactions.compact_reviews[0].id,
        "second"
    );
    assert_eq!(
        snapshot.projection.interactions.compact_reviews[0].context_id,
        second_context
            .context_id()
            .map(|id| atman_proto::ContextId(id.0))
    );
    let updates = state
        .session_updates(&session_id, "alice", before.cursor, None)
        .await
        .unwrap();
    let pending_states = updates
        .events
        .iter()
        .flat_map(|event| match &event.event {
            atman_proto::ServerEvent::ProjectionDelta { delta } => delta.changes.as_slice(),
            _ => &[],
        })
        .filter_map(|change| match change {
            atman_proto::ProjectionChange::InteractionsSet { interactions } => Some(
                interactions
                    .compact_reviews
                    .iter()
                    .map(|review| review.id.as_str())
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        pending_states,
        [vec!["first"], vec!["first", "second"], vec!["second"]]
    );
}
