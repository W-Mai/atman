//! History-operation requests retain their owning writer after the observing caller disconnects.

use std::io;

use super::FlowWorker;
use crate::flow_store::{
    history_operation::{HistoryOperationRecord, HistoryOperationRequest},
    inbox::SenderIdentity,
};

impl FlowWorker {
    /// Install the selected conversation before the serialized owner exposes new admission.
    pub async fn apply_history_operation(
        &self,
        sender: SenderIdentity,
        id: uuid::Uuid,
        authorize: impl FnOnce(&HistoryOperationRecord) -> io::Result<()> + Send + 'static,
    ) -> io::Result<HistoryOperationRecord> {
        let worker = self.clone();
        self.request(move |store| {
            if worker.is_stopping() {
                return Err(crate::flow_store::invalid("instance host is stopping"));
            }
            let (record, selected) = store.apply_history_with_context(sender, id, authorize)?;
            if let Some(selected) = selected {
                worker.context.project_selected_history(selected);
                worker.context.project_instance_goal(store.goal().cloned());
            }
            Ok(record)
        })
        .await
    }

    /// Authorize cancellation of the originally reviewed lease; owner reconciliation retains its completion stage.
    pub async fn stop_history_operation(
        &self,
        sender: SenderIdentity,
        id: uuid::Uuid,
        authorize: impl FnOnce(&HistoryOperationRecord) -> io::Result<()> + Send + 'static,
    ) -> io::Result<HistoryOperationRecord> {
        let worker = self.clone();
        self.request(move |store| {
            if worker.is_stopping() {
                return Err(crate::flow_store::invalid("instance host is stopping"));
            }
            store.stop_history_operation(sender, id, authorize)
        })
        .await
    }

    pub async fn begin_history_operation(
        &self,
        request: HistoryOperationRequest,
        authorize: impl FnOnce(&HistoryOperationRequest) -> io::Result<()> + Send + 'static,
    ) -> io::Result<HistoryOperationRecord> {
        self.request(move |store| store.begin_history_operation(request, authorize))
            .await
    }

    pub async fn abort_history_operation(
        &self,
        sender: SenderIdentity,
        id: uuid::Uuid,
        authorize: impl FnOnce(&HistoryOperationRecord) -> io::Result<()> + Send + 'static,
    ) -> io::Result<HistoryOperationRecord> {
        self.request(move |store| store.abort_history_operation(sender, id, authorize))
            .await
    }

    pub async fn history_operation(
        &self,
        sender: SenderIdentity,
        id: uuid::Uuid,
        authorize: impl FnOnce(&SenderIdentity) -> io::Result<()> + Send + 'static,
    ) -> io::Result<Option<HistoryOperationRecord>> {
        self.request(move |store| {
            authorize(&sender)?;
            Ok(store.history_operation(&sender, id).cloned())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow_runtime::scheduler::{SchedulingProgress, SchedulingStatus};
    use crate::flow_store::{
        RuntimeStore,
        completion::InvocationResult,
        history_operation::{HistoryOperationStatus, tests::active_fixture},
    };

    #[tokio::test]
    async fn history_stop_survives_disconnected_caller_and_finishes_on_original_owner_completion() {
        let (_directory, flow, operation) = active_fixture();
        let worker = FlowWorker::start(flow).await.unwrap();
        worker
            .begin_history_operation(operation.clone(), |_| Ok(()))
            .await
            .unwrap();
        let caller_worker = worker.clone();
        let caller_operation = operation.clone();
        let (entered, admitted) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let caller = tokio::spawn(async move {
            caller_worker
                .stop_history_operation(caller_operation.sender, caller_operation.id, move |_| {
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
        let stopping = worker
            .history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stopping.status, HistoryOperationStatus::Stopping);
        let lease = operation.active.clone().unwrap();
        let done = worker
            .request(move |store| {
                store.finish_turn(
                    lease,
                    InvocationResult::Unobserved(atman_rt::TurnOutcome::Completed),
                )
            })
            .await
            .unwrap();
        let ready = worker
            .history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ready.cancellation, stopping.cancellation);
        assert!(
            matches!(&ready.status, HistoryOperationStatus::Ready { completion: Some(receipt), .. } if receipt == &done.receipt)
        );
        let flow = worker.shutdown().await.unwrap();
        assert_eq!(flow.history_head(), operation.expected_head);
        assert!(!flow.can_admit_turn());
        assert_eq!(flow.pending_inputs().count(), 1);
    }

    #[tokio::test]
    async fn history_stop_reconciles_lost_process_completion_without_recancelling_on_open() {
        let (directory, mut flow, operation) = active_fixture();
        flow.begin_history_operation(operation.clone(), |_| Ok(()))
            .unwrap();
        let stopping = flow
            .stop_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
            .unwrap();
        drop(flow);
        let runtime = RuntimeStore::open(directory.path()).unwrap();
        let session = runtime.open_session(operation.flow.session_id.0).unwrap();
        let flow = session.open_flow(&operation.flow.instance_id).unwrap();
        let done = flow
            .completion(Some(&operation.active.as_ref().unwrap().turn))
            .unwrap()
            .clone();
        let worker = FlowWorker::start(flow).await.unwrap();
        let ready = worker
            .history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ready.cancellation, stopping.cancellation);
        assert!(
            matches!(&ready.status, HistoryOperationStatus::Ready { completion: Some(receipt), .. } if receipt == &done.receipt)
        );
        let flow = worker.shutdown().await.unwrap();
        assert!(!flow.can_admit_turn());
        assert_eq!(flow.history_head(), operation.expected_head);
    }

    #[tokio::test]
    async fn prepared_history_operation_defers_body_reads_and_abort_wakes_the_original_scheduler() {
        let (_directory, flow, operation) =
            crate::flow_store::history_operation::tests::reviewed_fixture();
        let path = flow.dir().unwrap().join("events.jsonl");
        let worker = FlowWorker::start(flow).await.unwrap();
        let prepared = worker
            .begin_history_operation(operation.clone(), |_| Ok(()))
            .await
            .unwrap();
        let original = std::fs::read(&path).unwrap();
        let mut unavailable = original.clone();
        let offset = unavailable
            .windows(b"retained input".len())
            .position(|value| value == b"retained input")
            .unwrap();
        unavailable[offset] = b'X';
        std::fs::write(&path, unavailable).unwrap();
        let (send, mut received) = tokio::sync::mpsc::unbounded_channel();
        let scheduler = worker
            .schedule(crate::Executor::new(), move |snapshot| {
                send.send(snapshot.pending.len()).unwrap();
                Ok(None)
            })
            .unwrap();
        let mut progress = worker.scheduling_progress.subscribe();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match progress.borrow_and_update().clone() {
                    SchedulingProgress::Idle(through, _) if through == prepared.accepted => break,
                    SchedulingProgress::Failed(error) => {
                        panic!("held input body was prepared: {error}")
                    }
                    _ => {}
                }
                progress.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert!(received.try_recv().is_err());
        assert_eq!(
            *scheduler.subscribe_status().borrow(),
            SchedulingStatus::Running
        );
        std::fs::write(&path, original).unwrap();
        worker
            .abort_history_operation(operation.sender, operation.id, |_| Ok(()))
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), received.recv())
                .await
                .unwrap(),
            Some(1)
        );
        scheduler.stop().await.unwrap();
        let flow = worker.shutdown().await.unwrap();
        assert!(flow.pending_history_operation().is_none());
        assert_eq!(flow.pending_inputs().count(), 1);
    }
}
