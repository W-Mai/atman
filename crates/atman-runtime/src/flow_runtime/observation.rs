//! Host-owned flow event observation, independent of recipient execution.

use std::{collections::HashMap, io, num::NonZeroUsize};

use futures::{FutureExt, future::BoxFuture};
use tokio::sync::watch;
use tokio_util::task::AbortOnDropHandle;

use super::{
    FlowWorker,
    host::{RuntimeHost, WeakRuntimeHost},
};
use crate::flow_store::access::FlowAction;
use crate::flow_store::{
    FlowRef,
    subscription::{SubscriptionRecord, SubscriptionStatus},
};

/// A retained subscription that could not advance. Retrying never resets its durable cursor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservationIssue {
    pub source: FlowRef,
    pub reason: String,
}

impl FlowWorker {
    pub async fn flow_subscriptions(&self) -> io::Result<Vec<SubscriptionRecord>> {
        self.request(|store| Ok(store.flow_subscriptions().cloned().collect()))
            .await
    }

    pub fn subscribe_observation_issues(&self) -> watch::Receiver<Vec<ObservationIssue>> {
        self.observation_issues.subscribe()
    }

    /// Request another scan after an external storage failure has been resolved.
    pub fn retry_observations(&self) {
        self.observation_retry.send_modify(|_| {});
    }
}

impl RuntimeHost {
    pub(super) fn authorize_observation(
        &self,
        recipient: &FlowRef,
        source: &FlowRef,
    ) -> io::Result<()> {
        let session = self.session(recipient.session_id.0)?;
        if session.is_stopped()
            || !session
                .access
                .read()
                .unwrap()
                .allows(recipient, source, FlowAction::Watch)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "flow observation is outside the current access grants",
            ));
        }
        Ok(())
    }
}

pub(super) fn start(recipient: FlowWorker, host: WeakRuntimeHost) -> AbortOnDropHandle<()> {
    AbortOnDropHandle::new(tokio::spawn(observe(recipient, host)))
}

async fn observe(recipient: FlowWorker, host: WeakRuntimeHost) {
    let Ok(directory) = host.upgrade() else {
        return;
    };
    let mut directory_changes = directory.changes();
    drop(directory);
    let mut recipient_changes = recipient.subscribe_commits();
    let mut retries = recipient.observation_retry.subscribe();
    loop {
        if recipient.is_stopped() {
            return;
        }
        directory_changes.borrow_and_update();
        recipient_changes.borrow_and_update();
        retries.borrow_and_update();
        let Ok(records) = recipient
            .request(|store| {
                if !store.state().accepts_input() {
                    return Ok(
                        matches!(store.state().state(), atman_rt::InstanceState::Prepared)
                            .then(Vec::new),
                    );
                }
                store.expire_flow_watches(chrono::Utc::now())?;
                Ok(Some(
                    store.flow_subscriptions().cloned().collect::<Vec<_>>(),
                ))
            })
            .await
        else {
            return;
        };
        let Some(records) = records else {
            recipient.observation_issues.send_if_modified(|issues| {
                let changed = !issues.is_empty();
                issues.clear();
                changed
            });
            return;
        };
        let deadline = records
            .iter()
            .filter(|record| record.status == SubscriptionStatus::Active)
            .filter_map(|record| record.subscription.expires_at)
            .min();
        let mut positions: HashMap<FlowRef, u64> = HashMap::new();
        for record in records
            .into_iter()
            .filter(|r| r.status == SubscriptionStatus::Active)
        {
            positions
                .entry(record.subscription.source)
                .and_modify(|p| *p = (*p).min(record.cursor))
                .or_insert(record.cursor);
        }
        let mut positions: Vec<_> = positions.into_iter().collect();
        positions.sort_by_key(|(source, _)| (source.session_id.0, source.instance_id.0.0));
        let mut waits: Vec<BoxFuture<'_, ()>> = Vec::new();
        let mut issues = Vec::new();
        for (source, after) in positions {
            let result = scan(&host, &recipient, source.clone(), after, &mut waits).await;
            if let Err(error) = result {
                issues.push(ObservationIssue {
                    source,
                    reason: error.to_string(),
                });
            }
        }
        recipient.observation_issues.send_if_modified(|previous| {
            if *previous == issues {
                return false;
            }
            *previous = issues;
            true
        });
        // All receivers are established before their corresponding reads. An arriving source fact,
        // registration, or grant change remains pending while the scan is in progress.
        let has_sources = !waits.is_empty();
        tokio::select! {
            result = directory_changes.changed() => { if result.is_err() { return; } }
            result = recipient_changes.changed() => { if result.is_err() { return; } }
            result = retries.changed() => { if result.is_err() { return; } }
            _ = async { match deadline { Some(at) => tokio::time::sleep((at - chrono::Utc::now()).to_std().unwrap_or_default()).await, None => std::future::pending().await } } => {}
            _ = async { futures::future::select_all(waits).await }, if has_sources => {}
        }
        // Continuously ready sources must not monopolize the executor.
        tokio::task::yield_now().await;
    }
}

async fn scan(
    host: &WeakRuntimeHost,
    recipient: &FlowWorker,
    source: FlowRef,
    after: u64,
    waits: &mut Vec<BoxFuture<'static, ()>>,
) -> io::Result<()> {
    let directory = host.upgrade()?;
    directory.authorize_observation(recipient.address(), &source)?;
    let publisher = directory.flow(&source).await?;
    drop(directory);
    let mut changes = publisher.subscribe_commits();
    changes.borrow_and_update();
    // Retain only a notification receiver while idle, never a strong host reference.
    waits.push(
        async move {
            let _ = changes.changed().await;
        }
        .boxed(),
    );
    let owner = host.clone();
    let target = recipient.address().clone();
    let page = publisher
        .request(move |store| {
            owner
                .upgrade()?
                .authorize_observation(&target, store.state().address())?;
            store.events_after(after, NonZeroUsize::new(64).unwrap())
        })
        .await?;
    if page.through() == after {
        return Ok(());
    }
    let owner = host.clone();
    recipient
        .request(move |store| {
            owner
                .upgrade()?
                .authorize_observation(store.state().address(), &source)?;
            store.observe_events(&page)
        })
        .await?;
    Ok(())
}
