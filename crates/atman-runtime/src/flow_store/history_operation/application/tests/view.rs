use super::*;
use crate::flow_runtime::{FlowView, FlowWorker};

fn messages(view: &FlowView) -> Vec<Message> {
    view.transcript()
        .entries()
        .iter()
        .flat_map(|entry| entry.record.shared_messages())
        .cloned()
        .collect()
}

async fn select(
    worker: &FlowWorker,
    operation: &HistoryOperationRequest,
) -> HistoryOperationRecord {
    worker
        .begin_history_operation(operation.clone(), |_| Ok(()))
        .await
        .unwrap();
    worker
        .stop_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
        .await
        .unwrap();
    worker
        .apply_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
        .await
        .unwrap()
}

#[tokio::test]
async fn history_view_pages_capture_one_head_and_restore_original_record_identities() {
    let (_directory, mut flow, operation) = reviewed();
    let original = flow.history_messages().unwrap();
    let worker = FlowWorker::start(flow).await.unwrap();
    let limit = std::num::NonZeroUsize::new(1).unwrap();
    let mut reader = worker.subscribe_view(limit).await.unwrap();
    let view = reader.next().await.unwrap();
    assert_eq!(messages(view), original);
    let original_records: Vec<_> = view
        .transcript()
        .entries()
        .iter()
        .filter(|entry| !entry.record.shared_messages().is_empty())
        .map(|entry| entry.id)
        .collect();
    let applied = select(&worker, &operation).await;
    let HistoryOperationStatus::Applied { head, .. } = applied.status else {
        unreachable!()
    };
    let mut frozen = worker.subscribe_view(limit).await.unwrap();
    let HistoryOperationTarget::Revert { cutoff, .. } = operation.target.clone() else {
        unreachable!()
    };
    let original_branch = operation.expected_head.branch;
    let restore = worker
        .request(move |store| {
            let mut request = request(store, *cutoff);
            request.target = HistoryOperationTarget::Restore {
                branch: original_branch,
            };
            Ok(request)
        })
        .await
        .unwrap();
    select(&worker, &restore).await;

    // The captured state and page limit retain the selected boundary despite a newer Restore.
    let first = frozen.next().await.unwrap();
    assert_eq!(first.transcript().history_head(), Some(&head));
    assert_eq!(first.snapshot().history.as_ref().unwrap().head(), &head);
    assert_eq!(messages(first), original[..1]);
    assert_eq!(
        first.transcript().cursor(),
        Some(&first.snapshot().status.cursor)
    );
    assert_eq!(
        first
            .transcript()
            .entries()
            .iter()
            .find(|entry| !entry.record.shared_messages().is_empty())
            .unwrap()
            .id,
        original_records[0]
    );

    let restored = frozen.next().await.unwrap();
    assert_eq!(messages(restored), original);
    assert_eq!(
        restored.transcript().history_head().unwrap().version.get(),
        2
    );
    let restored_head = restored.transcript().history_head().cloned();
    let restored_ids: Vec<_> = restored
        .transcript()
        .entries()
        .iter()
        .filter(|entry| !entry.record.shared_messages().is_empty())
        .map(|entry| entry.id)
        .collect();
    assert_eq!(restored_ids, original_records);
    let restored = reader.refresh().await.unwrap();
    assert_eq!(messages(restored), original);
    assert_eq!(restored.transcript().history_head(), restored_head.as_ref());
    let snapshot = restored.snapshot().clone();
    let refreshed = reader.refresh().await.unwrap();
    assert_eq!(refreshed.snapshot().status.cursor, snapshot.status.cursor);
    assert_eq!(messages(refreshed), original);
    drop(reader);
    drop(frozen);
    let flow = worker.shutdown().await.unwrap();
    assert!(flow.pending_inputs_at_head().next().is_none());
}

#[tokio::test]
async fn cancelled_history_view_refresh_keeps_the_previous_complete_view_until_retry() {
    let (_directory, flow, operation) = reviewed();
    let worker = FlowWorker::start(flow).await.unwrap();
    let mut reader = worker
        .subscribe_view(std::num::NonZeroUsize::new(1).unwrap())
        .await
        .unwrap();
    let old = reader.next().await.unwrap().snapshot().clone();
    select(&worker, &operation).await;
    let blocking = worker.clone();
    let (entered, admitted) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let blocker = tokio::spawn(async move {
        blocking
            .request(move |_| {
                entered.send(()).unwrap();
                released
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .map_err(io::Error::other)
            })
            .await
    });
    admitted.await.unwrap();
    {
        let read = reader.refresh();
        tokio::pin!(read);
        assert!(futures::poll!(read).is_pending());
    }
    assert_eq!(
        reader.view().unwrap().snapshot().status.cursor,
        old.status.cursor
    );
    release.send(()).unwrap();
    blocker.await.unwrap().unwrap();
    let changed = reader.refresh().await.unwrap();
    assert_ne!(changed.snapshot().status.cursor, old.status.cursor);
    assert_eq!(
        changed.transcript().history_head().unwrap().version.get(),
        1
    );
    assert_eq!(messages(changed).len(), 1);
    drop(reader);
    worker.shutdown().await.unwrap();
}
