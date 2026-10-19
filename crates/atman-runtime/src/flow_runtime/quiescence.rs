//! Event-driven batch-host drain across execution, scheduling, and notification delivery.

use std::{io, num::NonZeroUsize};

use atman_rt::{InstanceActivity, InstanceState};
use futures::{FutureExt, future::BoxFuture};
use tokio::sync::watch;

use super::{host::RuntimeHost, scheduler::SchedulingProgress};
use crate::flow_store::{
    process_watch::ProcessWatchStatus,
    subscription::{SubscriptionPurpose, SubscriptionStatus},
};

impl RuntimeHost {
    /// Wait for owned execution and notification handoffs to settle without stopping any instance.
    ///
    /// Active turns, closing instances, scheduler preparation, unread source facts, and explicit watchers on runnable recipients keep a batch host alive. Paused or unbound input remains durable; it does not authorize execution. Spawn result routes alone do not keep an idle host alive. Scheduler, cleanup and observation failures are returned rather than treated as successful completion. A frontend disconnect must not use this operation to decide instance lifetime.
    pub async fn wait_quiescent(&self) -> io::Result<()> {
        loop {
            let mut changes = Changes::default();
            changes.track(self.changes());
            changes.track(self.subscribe_controls());
            let mut busy = self.control_work_pending()?;
            for worker in self.owned_flows()? {
                changes.track(worker.subscribe_commits());
                changes.track(worker.scheduling_progress.subscribe());
                changes.track(worker.subscribe_observation_issues());
                changes.track(worker.subscribe_close_error());
                let (state, cursor, subscriptions, process_watch) = worker
                    .request(|store| {
                        Ok((
                            store.state().clone(),
                            store.cursor().clone(),
                            store
                                .flow_subscriptions()
                                .filter(|record| record.status == SubscriptionStatus::Active)
                                .cloned()
                                .collect::<Vec<_>>(),
                            store
                                .process_watches()
                                .any(|record| record.status == ProcessWatchStatus::Active),
                        ))
                    })
                    .await?;
                busy |= matches!(
                    state.state(),
                    InstanceState::Open(InstanceActivity::Active(_))
                        | InstanceState::Closing { .. }
                );
                if let Some(error) = worker.close_error.borrow().as_ref() {
                    return Err(io::Error::other(format!(
                        "flow {} cleanup: {error}",
                        worker.address().instance_id
                    )));
                }
                let progress = worker.scheduling_progress.borrow().clone();
                match &progress {
                    SchedulingProgress::Busy => busy = true,
                    SchedulingProgress::Idle(through) => busy |= through != &cursor,
                    SchedulingProgress::Failed(error) => {
                        return Err(io::Error::other(format!(
                            "flow {} scheduler: {error}",
                            worker.address().instance_id
                        )));
                    }
                    SchedulingProgress::Stopped => {}
                }
                if state.can_start_turn()
                    && matches!(
                        progress,
                        SchedulingProgress::Busy | SchedulingProgress::Idle(_)
                    )
                {
                    busy |= process_watch
                        || subscriptions.iter().any(|record| {
                            record.subscription.purpose == SubscriptionPurpose::Watcher
                        });
                }
                for record in subscriptions {
                    let source = &record.subscription.source;
                    self.authorize_observation(worker.address(), source)?;
                    let publisher = self.flow(source).await?;
                    changes.track(publisher.subscribe_commits());
                    // The observer commits its durable cursor with any resulting inbox delivery.
                    // It must catch up even when the publisher has already become idle.
                    let after = record.cursor;
                    busy |= publisher
                        .request(move |store| {
                            Ok(store
                                .events_after(after, NonZeroUsize::new(1).unwrap())?
                                .through()
                                > after)
                        })
                        .await?;
                }
                if let Some(issue) = worker.observation_issues.borrow().first() {
                    return Err(io::Error::other(format!(
                        "flow {} observation: {}",
                        worker.address().instance_id,
                        issue.reason
                    )));
                }
            }
            let changed = futures::future::select_all(changes.0);
            if busy {
                changed.await;
            } else if changed.now_or_never().is_none() {
                return Ok(());
            }
            // Continuous commits must leave time for the owners that make them.
            tokio::task::yield_now().await;
        }
    }
}

#[derive(Default)]
struct Changes(Vec<BoxFuture<'static, ()>>);

impl Changes {
    fn track<T: Clone + Send + Sync + 'static>(&mut self, mut changes: watch::Receiver<T>) {
        changes.borrow_and_update();
        self.0.push(
            async move {
                let _ = changes.changed().await;
            }
            .boxed(),
        );
    }
}
