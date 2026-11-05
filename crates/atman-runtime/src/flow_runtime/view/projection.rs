//! Retained CPU work for one captured journal page.

use std::{io, sync::OnceLock};

use futures::{executor::ThreadPool, future::RemoteHandle, task::SpawnExt};

use crate::{
    event_log::journal::CommittedBatch, flow_store::view::FlowUpdate,
    projection::flow::FlowTranscript,
};

pub(super) fn workers() -> ThreadPool {
    static WORKERS: OnceLock<ThreadPool> = OnceLock::new();
    WORKERS
        .get_or_init(|| {
            ThreadPool::builder()
                .pool_size(2)
                .name_prefix("atman-view-")
                .create()
                .expect("create view projection workers")
        })
        .clone()
}

pub(super) struct Projected {
    pub transcript: FlowTranscript,
    pub result: io::Result<()>,
}

pub(super) struct ProjectionTask {
    work: Option<RemoteHandle<Projected>>,
    pool: ThreadPool,
}

impl ProjectionTask {
    pub fn start(
        pool: &ThreadPool,
        mut transcript: FlowTranscript,
        batches: Vec<CommittedBatch<FlowUpdate>>,
    ) -> Self {
        let work = pool
            .spawn_with_handle(async move {
                let result = crate::panic_capture::blocking(|| {
                    for batch in batches {
                        transcript.apply(batch)?;
                    }
                    Ok(())
                })
                .unwrap_or_else(|_| Err(io::Error::other("flow transcript projection panicked")));
                Projected { transcript, result }
            })
            .expect("view projection workers remain open");
        Self {
            work: Some(work),
            pool: pool.clone(),
        }
    }

    pub async fn finish(&mut self) -> Projected {
        let result = self
            .work
            .as_mut()
            .expect("projection has one consumer")
            .await;
        self.work = None;
        result
    }
}

impl Drop for ProjectionTask {
    fn drop(&mut self) {
        if let Some(work) = self.work.take() {
            // Both queued work and an unread completed transcript are dropped off the UI.
            self.pool.spawn_ok(async move {
                drop(work);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        event_log::journal::CommitReceipt,
        flow_store::{FlowInstanceId, FlowRef, view::FlowContentSummary},
    };

    #[tokio::test]
    async fn failed_projection_returns_its_source_without_advancing_the_cursor() {
        let flow = FlowRef {
            session_id: uuid::Uuid::now_v7().into(),
            instance_id: FlowInstanceId::now(),
        };
        let batch = |journal_id| CommittedBatch {
            receipt: CommitReceipt {
                journal_id,
                sequence: 1,
                digest: "captured".into(),
            },
            facts: vec![FlowUpdate::Created {
                flow: flow.clone(),
                created_by: None,
                content: FlowContentSummary::Entry {
                    name: "main".into(),
                    path: "main.at".into(),
                },
            }],
        };
        let mut task = ProjectionTask::start(
            &workers(),
            FlowTranscript::new(flow.clone()),
            vec![batch(uuid::Uuid::now_v7())],
        );
        let failed = task.finish().await;
        assert_eq!(
            failed.result.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(failed.transcript.cursor().is_none());
        let valid = batch(flow.instance_id.0.0);
        let expected = valid.receipt.clone();
        let mut task = ProjectionTask::start(&workers(), failed.transcript, vec![valid]);
        let valid = task.finish().await;
        valid.result.unwrap();
        assert_eq!(valid.transcript.cursor(), Some(&expected));
        assert_eq!(valid.transcript.flow(), &flow);
    }
}
