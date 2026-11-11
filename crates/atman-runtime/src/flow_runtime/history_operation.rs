//! History-operation requests retain their owning writer after the observing caller disconnects.

use std::io;

use super::FlowWorker;
use crate::flow_store::{
    history_operation::{HistoryOperationRecord, HistoryOperationRequest},
    inbox::SenderIdentity,
};

impl FlowWorker {
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
