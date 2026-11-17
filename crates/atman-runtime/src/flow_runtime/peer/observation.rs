use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use super::*;
use crate::peer_authority::{AuthorityKey, AuthorityMode, AuthorityPlan};

impl FlowClient {
    pub(in crate::flow_runtime) async fn peer_watch_events(
        &self,
        subscription: FlowSubscription,
    ) -> io::Result<Option<SubscriptionRecord>> {
        if self.host.peer_port().is_none() {
            return Ok(None);
        }
        self.authenticate()?;
        let client = self.clone();
        self.host
            .watch_events(self.source().clone(), subscription, move |_| {
                client.authenticate()
            })
            .await
            .map(Some)
    }

    pub(in crate::flow_runtime) async fn peer_prepare_reply(
        &self,
        source: FlowRef,
        correlation: Uuid,
    ) -> io::Result<Option<super::super::reply::ReplyWait>> {
        if self.host.peer_port().is_none() {
            return Ok(None);
        }
        self.authenticate()?;
        let worker = self
            .source_session()?
            .loaded_flow(&self.source().instance_id)
            .ok_or_else(|| denied("original continuation owner is unavailable"))?;
        let guard = owner_guard(&self.host, self.source()).await?;
        authorize(&self.host, self.source(), &source, guard.clone()).await?;
        let waiter = crate::flow_store::waiter::ReplyWaiter {
            correlation,
            lease: self.lease.clone(),
            sender: crate::flow_store::inbox::SenderIdentity::Flow(source),
        };
        let waiting_worker = worker.clone();
        let client = self.clone();
        worker
            .request_with_authority(guard, move |store| {
                client.authenticate()?;
                let record = store.register_reply_waiter(waiter)?;
                if matches!(
                    record.status,
                    crate::flow_store::waiter::ReplyStatus::Cancelled { .. }
                ) {
                    return Err(denied("reply wait was already cancelled"));
                }
                Ok(Some(super::super::reply::ReplyWait::new(
                    waiting_worker,
                    record.waiter,
                )))
            })
            .await
    }
}

type OwnerAuthorization = Arc<dyn Fn(&FlowRef) -> io::Result<()> + Send + Sync>;

pub(super) struct ObservationIdentity {
    principal: ObservationPrincipal,
    binding: EffectBinding,
    publisher: FlowRef,
    access: Weak<std::sync::RwLock<crate::flow_store::access::SessionAccess>>,
    state: tokio::sync::watch::Receiver<atman_rt::FlowInstance<AtmanUuid>>,
    authorize: OwnerAuthorization,
}

pub(super) async fn validate(
    host: &RuntimeHost,
    principal: ObservationPrincipal,
    binding: EffectBinding,
    guard: Arc<crate::peer_authority::AuthorityGuard>,
) -> io::Result<InvocationWitness> {
    if principal.owner != host.generation() {
        return Err(denied("native observation owner generation changed"));
    }
    let identity = host
        .invocation_registry()
        .observations
        .lock()
        .unwrap()
        .get(&principal.nonce)
        .and_then(Weak::upgrade)
        .ok_or_else(|| denied("original native observation admission is unavailable"))?;
    let session = host.session(principal.recipient.session_id.0)?;
    let access = identity
        .access
        .upgrade()
        .ok_or_else(|| denied("original observation access owner is unavailable"))?;
    if identity.binding != binding
        || identity.principal.owner != principal.owner
        || identity.principal.recipient != principal.recipient
        || !Arc::ptr_eq(&access, &session.access)
        || identity.state.has_changed().is_err()
        || binding.target != AccessTarget::Flow(identity.publisher.clone())
        || binding.actions != [FlowAction::Watch]
    {
        return Err(denied(
            "native observation differs from its original admission",
        ));
    }
    (identity.authorize)(&principal.recipient)?;
    if host.is_transient(&principal.recipient) && principal.recipient != identity.publisher {
        return Err(denied("temporary observer cannot use another source"));
    }
    let snapshot = session.snapshot_with_authority(guard.clone()).await?;
    let allowed = access.read().unwrap().allows_scope(
        &principal.recipient,
        &binding.target,
        FlowAction::Watch,
    );
    Ok(InvocationWitness {
        source: principal.recipient,
        lease: None,
        binding,
        access_cursor: snapshot.cursor,
        source_grants: if allowed {
            vec![FlowAction::Watch]
        } else {
            Vec::new()
        },
        registered_authority: None,
    })
}

async fn native_anchor(
    host: &RuntimeHost,
    recipient: &super::super::FlowWorker,
    publisher: &FlowRef,
    authorize: OwnerAuthorization,
) -> io::Result<crate::flow_store::subscription::ObservationAnchor> {
    authorize(recipient.address())?;
    let session = host.session(recipient.address().session_id.0)?;
    let port = host
        .peer_port()
        .ok_or_else(|| protocol("native observation transport is unavailable"))?;
    let owner = if host.session(publisher.session_id.0).is_ok() {
        host.peer_identity()?
    } else {
        port.locate(PeerTarget::Session(publisher.session_id.0))
            .await?
    };
    let operation = PeerOperation::Anchor {
        recipient: recipient.address().clone(),
        source: publisher.clone(),
    };
    let binding = operation.binding(owner.runtime_generation)?;
    let principal = ObservationPrincipal {
        owner: host.generation(),
        nonce: Uuid::new_v4(),
        recipient: recipient.address().clone(),
    };
    let identity = Arc::new(ObservationIdentity {
        principal: principal.clone(),
        binding: binding.clone(),
        publisher: publisher.clone(),
        access: Arc::downgrade(&session.access),
        state: recipient.subscribe_state(),
        authorize,
    });
    {
        let mut entries = host.invocation_registry().observations.lock().unwrap();
        entries.retain(|_, entry| entry.strong_count() != 0);
        entries.insert(principal.nonce, Arc::downgrade(&identity));
    }
    let caller = host.peer_identity()?;
    let host = host.clone();
    let reply = tokio::spawn(async move {
        let _admission = identity;
        let call = PeerCall::Execute {
            principal: PeerPrincipal::Observation(principal),
            binding,
            operation: Box::new(operation),
        };
        if owner.runtime_generation == host.generation() {
            Box::pin(handle_peer_call(host, caller, call)).await
        } else {
            port.call(owner, call).await
        }
    })
    .await
    .map_err(io::Error::other)??;
    match reply {
        PeerReply::Anchor(anchor) => {
            let anchor = anchor.into_anchor();
            if anchor.source() == publisher {
                Ok(anchor)
            } else {
                Err(denied("native watch anchor belongs to another publisher"))
            }
        }
        _ => Err(protocol("native observation returned another reply kind")),
    }
}

pub(in crate::flow_runtime) async fn watch_native(
    host: &RuntimeHost,
    owner: FlowRef,
    subscription: FlowSubscription,
    authorize: OwnerAuthorization,
) -> io::Result<SubscriptionRecord> {
    authorize(&owner)?;
    let recipient = host.flow(&owner).await?;
    let requested = subscription.clone();
    let check = authorize.clone();
    if let Some(record) = recipient
        .request(move |store| {
            check(store.state().address())?;
            super::super::observation::existing_subscription(store, &requested)
        })
        .await?
    {
        return Ok(record);
    }
    let anchor = native_anchor(host, &recipient, &subscription.source, authorize.clone()).await?;
    let guard = owner_guard(host, &owner).await?;
    let requested = subscription.clone();
    let check = authorize.clone();
    if let Some(record) = recipient
        .request_with_authority(guard.clone(), move |store| {
            check(store.state().address())?;
            super::super::observation::existing_subscription(store, &requested)
        })
        .await?
    {
        return Ok(record);
    }
    self::authorize(host, &owner, &subscription.source, guard.clone()).await?;
    recipient
        .request_with_authority(guard, move |store| {
            authorize(store.state().address())?;
            store.subscribe_events(subscription.clone(), &anchor)?;
            super::super::observation::existing_subscription(store, &subscription)?
                .ok_or_else(|| protocol("registered native watcher disappeared"))
        })
        .await
}

pub(in crate::flow_runtime) async fn replace_native(
    host: &RuntimeHost,
    owner: FlowRef,
    expected: crate::flow_store::watch_edit::WatchDefinition,
    subscription: FlowSubscription,
    authorize: OwnerAuthorization,
) -> io::Result<FlowPosition> {
    use crate::flow_store::watch_edit::WatchDefinition;
    authorize(&owner)?;
    let recipient = host.flow(&owner).await?;
    let old = expected.clone();
    let new = WatchDefinition::Flow(subscription.clone());
    let check = authorize.clone();
    if let Some(receipt) = recipient
        .request(move |store| {
            check(store.state().address())?;
            store.watcher_replacement(&old, &new)
        })
        .await?
    {
        return Ok(receipt);
    }
    let anchor = native_anchor(host, &recipient, &subscription.source, authorize.clone()).await?;
    let guard = owner_guard(host, &owner).await?;
    let old = expected.clone();
    let new = WatchDefinition::Flow(subscription.clone());
    let check = authorize.clone();
    if let Some(receipt) = recipient
        .request_with_authority(guard.clone(), move |store| {
            check(store.state().address())?;
            store.watcher_replacement(&old, &new)
        })
        .await?
    {
        return Ok(receipt);
    }
    self::authorize(host, &owner, &subscription.source, guard.clone()).await?;
    let resources = recipient.resources.clone();
    recipient
        .request_with_authority(guard, move |store| {
            authorize(store.state().address())?;
            let receipt = store.replace_flow_watch(expected, subscription, &anchor)?;
            resources.observers.refresh();
            Ok(receipt)
        })
        .await
}

pub(crate) async fn scan_subscription(
    host: &RuntimeHost,
    recipient: &super::super::FlowWorker,
    source: &FlowRef,
    after: u64,
) -> io::Result<Option<()>> {
    if host.peer_port().is_none() {
        return Ok(None);
    }
    let page = super::durable::subscription_events(host, recipient, source, after)
        .await?
        .into_page();
    if page.through() == after {
        return Ok(Some(()));
    }
    let guard = owner_guard(host, recipient.address()).await?;
    authorize(host, recipient.address(), source, guard.clone()).await?;
    recipient
        .request_with_authority(guard, move |store| store.observe_events(&page))
        .await?;
    Ok(Some(()))
}

async fn owner_guard(
    host: &RuntimeHost,
    owner: &FlowRef,
) -> io::Result<Arc<crate::peer_authority::AuthorityGuard>> {
    Ok(Arc::new(
        host.peer_authority(owner.session_id.0)?
            .acquire(
                AuthorityPlan::new([
                    (AuthorityKey::Profile, AuthorityMode::Shared),
                    (
                        AuthorityKey::Session(owner.session_id.0),
                        AuthorityMode::Exclusive,
                    ),
                ])?,
                tokio_util::sync::CancellationToken::new(),
                Instant::now() + Duration::from_secs(15),
            )
            .await?,
    ))
}

async fn authorize(
    host: &RuntimeHost,
    owner: &FlowRef,
    source: &FlowRef,
    guard: Arc<crate::peer_authority::AuthorityGuard>,
) -> io::Result<()> {
    let session = host.session(owner.session_id.0)?;
    if host.is_transient(owner) && owner != source {
        return Err(denied("temporary owner cannot observe another Flow"));
    }
    let _ = session.snapshot_with_authority(guard.clone()).await?;
    let target = AccessTarget::Flow(source.clone());
    let local = session
        .access
        .read()
        .unwrap()
        .allows_scope(owner, &target, FlowAction::Watch);
    if local || super::dispatch::profile_allows(host, owner, &target, FlowAction::Watch).await? {
        Ok(())
    } else {
        Err(denied("observation source is outside current Watch grants"))
    }
}
