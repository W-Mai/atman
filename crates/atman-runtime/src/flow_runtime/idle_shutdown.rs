//! Atomic idle retirement for a detached native frontend.

use std::{
    collections::HashMap,
    io,
    sync::{Arc, atomic::Ordering},
};
use tokio::sync::oneshot;

use super::*;
use crate::{
    flow_runtime::{Command, FlowWorker},
    peer_authority::{AuthorityKey, AuthorityMode, AuthorityPlan},
};

struct IdleSnapshot {
    pending: bool,
    cursor: FlowPosition,
    can_admit: bool,
    input: bool,
    observable: u64,
    subscriptions: Vec<crate::flow_store::subscription::SubscriptionRecord>,
}

fn snapshot(store: &crate::flow_store::FlowStore) -> IdleSnapshot {
    IdleSnapshot {
        pending: store.pending_sends().next().is_some()
            || store
                .timers()
                .any(|timer| timer.status == crate::flow_store::timer::TimerStatus::Scheduled),
        cursor: store.cursor().clone(),
        can_admit: store.can_admit_turn(),
        input: store.pending_inputs_at_head().next().is_some(),
        observable: store.observable_sequence(),
        subscriptions: store
            .flow_subscriptions()
            .filter(|record| {
                record.status == crate::flow_store::subscription::SubscriptionStatus::Active
            })
            .cloned()
            .collect(),
    }
}

impl FlowWorker {
    async fn frozen_idle_snapshot(&self) -> io::Result<IdleSnapshot> {
        let (reply, received) = oneshot::channel();
        self.commands
            .send(Command::Inspect(Box::new(move |store| {
                let captured = snapshot(store);
                Box::new(move || {
                    let _ = reply.send(captured);
                })
            })))
            .await
            .map_err(|_| invalid("idle observation owner stopped"))?;
        received
            .await
            .map_err(|_| invalid("idle observation did not acknowledge"))
    }
}

impl RuntimeHost {
    pub(super) async fn idle_background_readiness(
        &self,
        frozen: bool,
    ) -> io::Result<Vec<HandoffBlocker>> {
        let workers: Vec<_> = self
            .owned_flows()?
            .into_iter()
            .filter(|worker| !worker.is_stopped())
            .collect();
        let mut observed = HashMap::new();
        for worker in &workers {
            let value = if frozen {
                worker.frozen_idle_snapshot().await?
            } else {
                worker.request(|store| Ok(snapshot(store))).await?
            };
            observed.insert(worker.address().clone(), value);
        }
        let mut blockers = Vec::new();
        for worker in &workers {
            let value = &observed[worker.address()];
            let scheduled = worker.scheduler_owned.load(Ordering::Acquire);
            let behind = match &*worker.scheduling_progress.borrow() {
                crate::flow_runtime::scheduler::SchedulingProgress::Busy => true,
                crate::flow_runtime::scheduler::SchedulingProgress::Idle(cursor, sources) => {
                    cursor != &value.cursor || sources.has_changed()
                }
                _ => false,
            };
            if value.pending || (scheduled && (behind || (value.can_admit && value.input))) {
                blockers.push(HandoffBlocker::Continuation {
                    instance_id: worker.address().instance_id.clone(),
                });
            }
            for subscription in &value.subscriptions {
                let source = observed.get(&subscription.subscription.source);
                let unread = source.is_none_or(|source| source.observable > subscription.cursor);
                let watcher = subscription.subscription.purpose
                    == crate::flow_store::subscription::SubscriptionPurpose::Watcher
                    && scheduled
                    && value.can_admit;
                if unread || watcher {
                    blockers.push(HandoffBlocker::Observation {
                        instance_id: worker.address().instance_id.clone(),
                    });
                }
            }
        }
        Ok(blockers)
    }

    /// Close new admission only after all original accepted effects and continuations are idle.
    /// Contention or a busy recheck leaves every owner and its native routes usable.
    pub async fn try_shutdown_idle(&self) -> io::Result<bool> {
        let host = self.clone();
        tokio::spawn(async move {
            let Ok(_finalizing) = host.directory.finalizing.clone().try_lock_owned() else {
                return Ok(false);
            };
            if let Some(result) = host.directory.idle_shutdown_result.lock().unwrap().clone() {
                return result.map(|()| true).map_err(io::Error::other);
            }
            if host.directory.stopping.load(Ordering::Acquire) {
                return Ok(false);
            }
            let Ok(_opening) = host.directory.opening.clone().try_lock_owned() else {
                return Ok(false);
            };
            let sessions: Vec<_> = host
                .directory
                .sessions
                .lock()
                .unwrap()
                .values()
                .cloned()
                .collect();
            let mut freezes = Vec::new();
            let mut captures = Vec::new();
            for session in &sessions {
                let Some(freeze) = session.admission.try_freeze() else {
                    return Ok(false);
                };
                freezes.push(freeze);
                let Ok(capture) = session.source_reads.clone().try_write_owned() else {
                    return Ok(false);
                };
                captures.push(capture);
            }
            let authority = match host.authority_domain() {
                Some(domain) => {
                    let plan = AuthorityPlan::new(
                        std::iter::once((AuthorityKey::Profile, AuthorityMode::Exclusive)).chain(
                            sessions.iter().map(|session| {
                                (
                                    AuthorityKey::Session(session.id()),
                                    AuthorityMode::Exclusive,
                                )
                            }),
                        ),
                    )?;
                    let Some(guard) = domain.try_acquire(plan).await? else {
                        return Ok(false);
                    };
                    Some(Arc::new(guard))
                }
                None => None,
            };
            if host.peer_transfers_pending()
                || !host.shutdown_readiness_inner(true).await?.is_empty()
            {
                return Ok(false);
            }
            let workers = host.owned_flows()?;
            let mut compact = Vec::new();
            for worker in &workers {
                let Ok(guard) = worker.context().compact_lock_handle().try_lock_owned() else {
                    return Ok(false);
                };
                compact.push(guard);
            }
            #[cfg(test)]
            {
                let observe = host.directory.idle_before_commit.lock().unwrap().take();
                if let Some(observe) = observe {
                    observe(&host);
                }
            }
            {
                let current = host.directory.sessions.lock().unwrap();
                if current.len() != sessions.len()
                    || host.directory.stopping.load(Ordering::Acquire)
                    || host.directory.host_operations.load(Ordering::Acquire) != 0
                {
                    return Ok(false);
                }
                host.directory.stopping.store(true, Ordering::Release);
            }
            for freeze in freezes {
                freeze.release();
            }
            host.changed();
            host.directory.controls.stop().await;
            let mut errors = Vec::new();
            for session in &sessions {
                let (reply, received) = oneshot::channel();
                let command = CatalogCommand::ShutdownIdle {
                    authority: authority.clone(),
                    reply,
                };
                let result = match session
                    .commands
                    .send(CatalogEnvelope {
                        command,
                        admission: None,
                        authority: authority.clone(),
                    })
                    .await
                {
                    Ok(()) => received
                        .await
                        .map_err(io::Error::other)
                        .and_then(|result| result),
                    Err(_) => Err(invalid("idle Session owner stopped before acknowledgement")),
                };
                if let Err(error) = result {
                    errors.push(format!("session {}: {error}", session.id()));
                }
            }
            host.directory.sessions.lock().unwrap().clear();
            let profile = host.directory.profile.lock().unwrap().take();
            if let Some(profile) = profile {
                let result = match authority.clone() {
                    Some(authority) => profile.shutdown_with_authority(authority).await,
                    None => profile.shutdown().await,
                };
                if let Err(error) = result {
                    errors.push(format!("profile: {error}"));
                }
            }
            let result = if errors.is_empty() {
                Ok(())
            } else {
                Err(errors.join("; "))
            };
            *host.directory.idle_shutdown_result.lock().unwrap() = Some(result.clone());
            result.map(|()| true).map_err(io::Error::other)
        })
        .await
        .map_err(io::Error::other)?
    }
}
