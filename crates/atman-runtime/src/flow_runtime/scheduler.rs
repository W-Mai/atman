//! Event-driven scheduling of host-selected input on equal persistent instances.

use std::io;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use futures::FutureExt;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::{
    FlowSnapshot, FlowWorker,
    execution::{TurnCompletion, TurnRequest},
};
use crate::flow_store::position::FlowPosition;
use crate::{Executor, flow_store::invalid};

/// Owns automatic admission, not the lifetime of an already admitted turn.
///
/// Dropping this handle stops scheduling. Use execution controls on `FlowWorker` to pause or cancel an active turn. `stop` waits for the scheduler to relinquish its ownership; admitted turns retain their own execution and completion obligations.
pub struct FlowScheduler {
    stop: CancellationToken,
    task: Option<tokio::task::JoinHandle<SchedulerExit>>,
    status: watch::Receiver<SchedulingStatus>,
}

/// Scheduling ownership is separate from the execution state of an admitted turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SchedulingStatus {
    Running,
    Stopped,
    Failed(String),
}

/// An idle checkpoint acknowledges the scheduling policy, including input it cannot consume.
#[derive(Clone, Debug)]
pub(super) enum SchedulingProgress {
    Busy,
    Idle(
        crate::flow_store::position::FlowPosition,
        super::task_scope::ScopeChanges,
    ),
    Stopped,
    Failed(String),
}

pub(crate) type PrepareTurn =
    Box<dyn FnMut(&FlowSnapshot) -> io::Result<Option<TurnRequest>> + Send>;
pub(crate) type CompletedTurn = Box<dyn FnMut(TurnCompletion) + Send>;

struct SchedulingPolicy {
    executor: Executor,
    prepare: PrepareTurn,
    completed: CompletedTurn,
}

/// Original callbacks and execution binding retained while an owner changes storage.
pub(super) struct SchedulerBinding {
    policy: Option<SchedulingPolicy>,
    previous: SchedulingStatus,
}

struct SchedulerExit {
    result: io::Result<()>,
    binding: SchedulerBinding,
}

impl SchedulerBinding {
    pub(super) fn start(&mut self, worker: &FlowWorker) -> io::Result<FlowScheduler> {
        let policy = self
            .policy
            .as_ref()
            .ok_or_else(|| invalid("scheduling policy was already installed"))?;
        let runtime = tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
        worker
            .context()
            .bind_executor(&policy.executor)
            .map_err(invalid)?;
        worker
            .scheduler_owned
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| invalid("instance already has a scheduler"))?;
        let ownership = SchedulerOwnership(worker.scheduler_owned.clone());
        Ok(worker.schedule_owned(
            self.policy.take().expect("checked scheduling policy"),
            runtime,
            ownership,
        ))
    }

    pub(super) fn restore(mut self, worker: &FlowWorker) -> io::Result<FlowScheduler> {
        if self.previous == SchedulingStatus::Running {
            return self.start(worker);
        }
        let result = match &self.previous {
            SchedulingStatus::Failed(reason) => Err(io::Error::other(reason.clone())),
            _ => Ok(()),
        };
        let status = watch::channel(self.previous.clone()).1;
        Ok(FlowScheduler {
            stop: CancellationToken::new(),
            task: Some(tokio::spawn(async move {
                SchedulerExit {
                    result,
                    binding: self,
                }
            })),
            status,
        })
    }
}

impl FlowScheduler {
    pub fn subscribe_status(&self) -> watch::Receiver<SchedulingStatus> {
        self.status.clone()
    }

    pub async fn stop(mut self) -> io::Result<()> {
        self.stop.cancel();
        self.task
            .take()
            .expect("scheduler owns its task")
            .await
            .map_err(io::Error::other)?
            .result
    }

    /// Observe termination, including preparation/storage failures, without cancelling the scheduler.
    pub async fn wait(mut self) -> io::Result<()> {
        self.task
            .take()
            .expect("scheduler owns its task")
            .await
            .map_err(io::Error::other)?
            .result
    }

    pub(super) async fn suspend(mut self) -> io::Result<SchedulerBinding> {
        let previous = self.status.borrow().clone();
        self.stop.cancel();
        let mut exit = self
            .task
            .take()
            .expect("scheduler owns its task")
            .await
            .map_err(io::Error::other)?;
        exit.binding.previous = previous;
        Ok(exit.binding)
    }
}

impl Drop for FlowScheduler {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

struct SchedulerOwnership(Arc<AtomicBool>);

/// Consume the body-bearing snapshot before admission or idle waits retain the scheduler future.
fn prepare_snapshot(
    snapshot: FlowSnapshot,
    prepare: &mut PrepareTurn,
) -> io::Result<(FlowPosition, Option<TurnRequest>)> {
    let request = if snapshot.state.can_start_turn() && !snapshot.pending.is_empty() {
        prepare(&snapshot)?
    } else {
        None
    };
    if let Some(request) = &request {
        let eligible: std::collections::HashSet<_> =
            snapshot.pending.iter().map(|input| input.id).collect();
        if request.messages.iter().any(|id| !eligible.contains(id)) {
            return Err(invalid("prepared input is outside the scheduling snapshot"));
        }
    }
    Ok((snapshot.cursor, request))
}

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
        prepare: impl FnMut(&FlowSnapshot) -> io::Result<Option<TurnRequest>> + Send + 'static,
    ) -> io::Result<FlowScheduler> {
        self.schedule_observed(executor, Box::new(prepare), Box::new(|_| {}))
    }

    pub(crate) fn schedule_observed(
        &self,
        executor: Executor,
        prepare: PrepareTurn,
        completed: CompletedTurn,
    ) -> io::Result<FlowScheduler> {
        SchedulerBinding {
            policy: Some(SchedulingPolicy {
                executor,
                prepare,
                completed,
            }),
            previous: SchedulingStatus::Running,
        }
        .start(self)
    }

    fn schedule_owned(
        &self,
        policy: SchedulingPolicy,
        runtime: tokio::runtime::Handle,
        ownership: SchedulerOwnership,
    ) -> FlowScheduler {
        let progress = self.scheduling_progress.clone();
        progress.send_replace(SchedulingProgress::Busy);
        let scanning = progress.clone();
        let worker = self.clone();
        let stop = CancellationToken::new();
        let stopping = stop.clone();
        let (status_tx, status) = watch::channel(SchedulingStatus::Running);
        let task = runtime.spawn(async move {
            let _ownership = ownership;
            let SchedulingPolicy {
                executor,
                mut prepare,
                mut completed,
            } = policy;
            let execution = async {
                let mut commits = worker.subscribe_commits();
                loop {
                    if stopping.is_cancelled() || worker.is_stopping() {
                        return Ok(());
                    }
                    commits.borrow_and_update();
                    let (snapshot, mut input_scopes) = worker.scheduling_snapshot().await?;
                    let Some(snapshot) = snapshot else {
                        continue;
                    };
                    // The query may have caught up with a notification that was not yet marked seen.
                    // Do not prepare the same snapshot twice, or erase a commit newer than this read.
                    if *commits.borrow_and_update() != snapshot.cursor {
                        continue;
                    }
                    if matches!(snapshot.state.state(), atman_rt::InstanceState::Closed) {
                        return Ok(());
                    }
                    let (cursor, request) = prepare_snapshot(snapshot, &mut prepare)?;
                    if let Some(request) = request {
                        scanning.send_if_modified(|previous| {
                            let changed = !matches!(previous, SchedulingProgress::Busy);
                            *previous = SchedulingProgress::Busy;
                            changed
                        });
                        if stopping.is_cancelled() {
                            return Ok(());
                        }
                        if let Some(ticket) = worker
                            .admit_turn(executor.clone(), request, Some(cursor))
                            .await?
                        {
                            tokio::select! {
                                biased;
                                _ = stopping.cancelled() => return Ok(()),
                                completion = ticket.wait() => { completed(completion?); }
                            }
                        }
                        // Admission and completion both recheck the queue; no notification needs to survive this scan.
                        continue;
                    }
                    scanning.send_if_modified(|previous| {
                        if matches!(previous, SchedulingProgress::Idle(previous_cursor, sources)
                        if previous_cursor == &cursor && !sources.has_changed())
                        {
                            return false;
                        }
                        *previous = SchedulingProgress::Idle(cursor, input_scopes.changes.clone());
                        true
                    });
                    tokio::select! {
                        biased;
                        _ = stopping.cancelled() => return Ok(()),
                        changed = commits.changed() => {
                            changed.map_err(|_| io::Error::other("instance storage stopped"))?;
                        }
                        _ = input_scopes.changed() => {}
                    }
                }
            };
            let result = std::panic::AssertUnwindSafe(execution)
                .catch_unwind()
                .await
                .unwrap_or_else(|_| Err(io::Error::other("instance scheduler panicked")));
            status_tx.send_replace(match &result {
                Ok(()) => SchedulingStatus::Stopped,
                Err(error) => SchedulingStatus::Failed(error.to_string()),
            });
            progress.send_replace(match &result {
                Ok(()) => SchedulingProgress::Stopped,
                Err(error) => SchedulingProgress::Failed(error.to_string()),
            });
            SchedulerExit {
                result,
                binding: SchedulerBinding {
                    policy: Some(SchedulingPolicy {
                        executor,
                        prepare,
                        completed,
                    }),
                    previous: SchedulingStatus::Stopped,
                },
            }
        });
        FlowScheduler {
            stop,
            task: Some(task),
            status,
        }
    }
}
