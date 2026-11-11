use super::*;
use crate::flow_runtime::FlowWorker;

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
async fn live_history_install_preserves_snapshots_retries_goal_and_reopened_epoch() {
    let (directory, mut flow, mut operation) = reviewed();
    let old_checkpoint = checkpoint(&mut flow);
    flow.checkpoint_context(old_checkpoint.clone()).unwrap();
    flow.edit_goal(
        goal_request(
            &flow,
            GoalEdit::Create {
                text: "retained objective".into(),
            },
        ),
        |_| Ok(()),
    )
    .unwrap();
    operation.goal = flow.goal().map(GoalVersion::from);
    let worker = FlowWorker::start(flow).await.unwrap();
    let context = worker.context();
    let old_window = context.messages();
    let old_full = context.messages_full();
    let original_epoch = context.context_epoch();
    let diagnostic = crate::event::Event::MermaidDiagram {
        source: "retained diagnostic".into(),
    };
    context.sink().emit(diagnostic);
    let before_events = context.sink().events_handle().lock().unwrap().len();
    let diagnostics =
        serde_json::to_value(&*context.sink().events_handle().lock().unwrap()).unwrap();
    let applied = select(&worker, &operation).await;
    let HistoryOperationStatus::Applied { head, .. } = &applied.status else {
        unreachable!()
    };
    assert_eq!(context.messages().history_head(), Some(head));
    assert_eq!(context.messages().to_vec(), old_full[..1]);
    assert_eq!(&**context.messages_full(), &old_full[..1]);
    assert_eq!(*context.messages_handle().lock().unwrap(), old_full[..1]);
    assert_eq!(
        serde_json::to_value(&context.sink().events_handle().lock().unwrap()[..before_events])
            .unwrap(),
        diagnostics
    );
    assert_eq!(old_window.to_vec(), old_checkpoint.messages);
    assert!(old_full.len() > context.messages_full().len());
    assert_eq!(
        worker.goal().await.unwrap().goal.unwrap().status,
        GoalStatus::Paused
    );
    let selected_epoch = context.context_epoch();
    assert_ne!(selected_epoch, original_epoch);
    assert!(selected_epoch.is_some());
    let stale = old_checkpoint.clone();
    assert!(
        worker
            .request(move |store| {
                let mut late = stale;
                late.id = Uuid::new_v4();
                store.checkpoint_context(late)
            })
            .await
            .is_err()
    );
    let projected = context.clone();
    worker
        .request(move |store| {
            let checkpoint = checkpoint(store);
            let saved = store.checkpoint_context(checkpoint)?;
            projected.project_checkpoint(&saved.checkpoint);
            Ok(())
        })
        .await
        .unwrap();
    let with_checkpoint = context.messages();
    let checkpoint_epoch = context.context_epoch();
    assert_eq!(
        worker
            .apply_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
            .await
            .unwrap(),
        applied
    );
    assert_eq!(context.messages().to_vec(), with_checkpoint.to_vec());
    assert_eq!(context.context_epoch(), checkpoint_epoch);
    assert_eq!(&**context.messages_full(), &old_full[..1]);

    let HistoryOperationTarget::Revert { cutoff, .. } = operation.target.clone() else {
        unreachable!()
    };
    let original_branch = operation.expected_head.branch;
    let restore = worker
        .request(move |store| {
            let mut operation = request(store, *cutoff);
            operation.target = HistoryOperationTarget::Restore {
                branch: original_branch,
            };
            Ok(operation)
        })
        .await
        .unwrap();
    select(&worker, &restore).await;
    assert_eq!(context.messages().to_vec(), old_checkpoint.messages);
    assert_eq!(*context.messages_full(), *old_full);
    assert!(context.sink().next_seq_peek() > old_full.len() as u64);
    let restored_epoch = context.context_epoch();
    assert_ne!(restored_epoch, original_epoch);
    assert_ne!(restored_epoch, checkpoint_epoch);
    let final_head = context.messages().history_head().cloned();
    let flow = worker.shutdown().await.unwrap();
    assert!(flow.pending_inputs_at_head().next().is_none());
    drop(flow);
    let runtime = RuntimeStore::open(directory.path()).unwrap();
    let session = runtime.open_session(operation.flow.session_id.0).unwrap();
    let worker = FlowWorker::start(session.open_flow(&operation.flow.instance_id).unwrap())
        .await
        .unwrap();
    assert_eq!(
        worker.context().messages().history_head(),
        final_head.as_ref()
    );
    assert_eq!(worker.context().context_epoch(), restored_epoch);
    assert_eq!(*worker.context().messages_full(), *old_full);
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn history_install_prepares_before_commit_and_survives_disconnected_observer() {
    let (directory, flow, operation) = reviewed();
    let path = flow.dir().unwrap().join("events.jsonl");
    let worker = FlowWorker::start(flow).await.unwrap();
    let context = worker.context();
    let original = context.messages();
    worker
        .begin_history_operation(operation.clone(), |_| Ok(()))
        .await
        .unwrap();
    let ready = worker
        .stop_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
        .await
        .unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let mut corrupt = bytes.clone();
    let offset = corrupt
        .windows(b"retained".len())
        .position(|bytes| bytes == b"retained")
        .unwrap();
    corrupt[offset] = b'X';
    std::fs::write(&path, &corrupt).unwrap();
    assert!(
        worker
            .apply_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), corrupt);
    assert_eq!(context.messages().to_vec(), original.to_vec());
    assert_eq!(
        worker
            .history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
            .await
            .unwrap(),
        Some(ready)
    );
    std::fs::write(&path, &bytes).unwrap();

    // A journal read failure retains the original poisoned-writer contract until reopening.
    drop(worker.shutdown().await.unwrap());
    let runtime = RuntimeStore::open(directory.path()).unwrap();
    let session = runtime.open_session(operation.flow.session_id.0).unwrap();
    let worker = FlowWorker::start(session.open_flow(&operation.flow.instance_id).unwrap())
        .await
        .unwrap();
    let context = worker.context();

    let (entered, admitted) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let caller_worker = worker.clone();
    let sender = operation.sender.clone();
    let id = operation.id;
    let caller = tokio::spawn(async move {
        caller_worker
            .apply_history_operation(sender, id, move |_| {
                entered.send(()).unwrap();
                released
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .map_err(io::Error::other)
            })
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), admitted)
        .await
        .unwrap()
        .unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    release.send(()).unwrap();
    let applied = worker
        .history_operation(operation.sender, id, |_| Ok(()))
        .await
        .unwrap()
        .unwrap();
    let HistoryOperationStatus::Applied { head, .. } = applied.status else {
        unreachable!()
    };
    assert_eq!(context.messages().history_head(), Some(&head));
    assert_eq!(context.messages().len(), 1);
    worker.shutdown().await.unwrap();
}
