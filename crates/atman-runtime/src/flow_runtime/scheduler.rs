//! Event-driven scheduling of host-selected input on equal persistent instances.

use std::io;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use tokio_util::sync::CancellationToken;

use super::{FlowSnapshot, FlowWorker, execution::TurnRequest};
use crate::{Executor, flow_store::invalid};

/// Owns automatic admission, not the lifetime of an already admitted turn.
///
/// Dropping this handle stops scheduling. Use execution controls on `FlowWorker` to pause or cancel an active turn. `stop` waits for the scheduler to relinquish its ownership; admitted turns retain their own execution and completion obligations.
pub struct FlowScheduler {
    stop: CancellationToken,
    task: Option<tokio::task::JoinHandle<io::Result<()>>>,
}

impl FlowScheduler {
    pub async fn stop(mut self) -> io::Result<()> {
        self.stop.cancel();
        self.task
            .take()
            .expect("scheduler owns its task")
            .await
            .map_err(io::Error::other)?
    }

    /// Observe termination, including preparation/storage failures, without cancelling the scheduler.
    pub async fn wait(mut self) -> io::Result<()> {
        self.task
            .take()
            .expect("scheduler owns its task")
            .await
            .map_err(io::Error::other)?
    }
}

impl Drop for FlowScheduler {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

struct SchedulerOwnership(Arc<AtomicBool>);

impl Drop for SchedulerOwnership {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl FlowWorker {
    /// Schedule pending input using a synchronous host preparation policy.
    ///
    /// Preparation selects IDs, arguments, context, and explicit authority from a committed snapshot; it must not block on external I/O. Returning `None` waits for another commit. Invalid preparation stops scheduling without dropping pending input. Execution failures remain interrupted and require explicit recovery; the scheduler does not replay them.
    pub fn schedule(
        &self,
        executor: Executor,
        mut prepare: impl FnMut(&FlowSnapshot) -> io::Result<Option<TurnRequest>> + Send + 'static,
    ) -> io::Result<FlowScheduler> {
        let runtime = tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
        self.scheduler_owned
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| invalid("instance already has a scheduler"))?;
        let ownership = SchedulerOwnership(self.scheduler_owned.clone());
        let worker = self.clone();
        let stop = CancellationToken::new();
        let stopping = stop.clone();
        let task = runtime.spawn(async move {
            let _ownership = ownership;
            let mut commits = worker.subscribe_commits();
            loop {
                if stopping.is_cancelled() {
                    return Ok(());
                }
                commits.borrow_and_update();
                let snapshot = worker.snapshot().await?;
                // The query may have caught up with a notification that was not yet marked seen.
                // Do not prepare the same snapshot twice, or erase a commit newer than this read.
                if *commits.borrow_and_update() != snapshot.cursor {
                    continue;
                }
                if matches!(snapshot.state.state(), atman_rt::InstanceState::Closed) {
                    return Ok(());
                }
                if snapshot.state.can_start_turn()
                    && !snapshot.pending.is_empty()
                    && let Some(request) = prepare(&snapshot)?
                {
                    if stopping.is_cancelled() {
                        return Ok(());
                    }
                    if let Some(ticket) = worker
                        .admit_turn(executor.clone(), request, Some(snapshot.cursor))
                        .await?
                    {
                        tokio::select! {
                            biased;
                            _ = stopping.cancelled() => return Ok(()),
                            completion = ticket.wait() => { completion?; }
                        }
                    }
                    // Admission and completion both recheck the queue; no notification needs to survive this scan.
                    continue;
                }
                tokio::select! {
                    biased;
                    _ = stopping.cancelled() => return Ok(()),
                    changed = commits.changed() => {
                        changed.map_err(|_| io::Error::other("instance storage stopped"))?;
                    }
                }
            }
        });
        Ok(FlowScheduler {
            stop,
            task: Some(task),
        })
    }
}
