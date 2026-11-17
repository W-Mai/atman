use std::sync::Arc;

use super::*;
use crate::flow_store::outbox::{SendAuthorization, SendRecord};

pub(super) struct DurableIdentity {
    origin: DurableOrigin,
    binding: EffectBinding,
    record: DurableRecord,
    effects: Mutex<HashMap<Uuid, Weak<RegisteredEffect>>>,
}

enum DurableRecord {
    Outbox(Box<SendRecord>),
    Subscription {
        record: Box<SubscriptionRecord>,
        observed_at: FlowPosition,
    },
}

pub(crate) async fn deliver_outbox(
    host: &RuntimeHost,
    sender: &super::super::FlowWorker,
    record: &SendRecord,
) -> io::Result<Option<Acceptance>> {
    let Some(port) = host.peer_port() else {
        return Ok(None);
    };
    if record.intent.authorization != SendAuthorization::PeerGrants {
        return Ok(None);
    }
    if host.is_transient(sender.address()) && record.intent.request.recipient == *sender.address() {
        return Ok(None);
    }
    if record.intent.request.sender
        != crate::flow_store::inbox::SenderIdentity::Flow(sender.address().clone())
    {
        return Err(denied(
            "durable sender differs from its original outbox owner",
        ));
    }
    let owner = if host
        .session(record.intent.request.recipient.session_id.0)
        .is_ok()
    {
        host.peer_identity()?
    } else {
        match port
            .locate(PeerTarget::Session(
                record.intent.request.recipient.session_id.0,
            ))
            .await
        {
            Ok(owner) => owner,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return match deleted_delivery(host, sender, record).await? {
                    Some(accepted) => Ok(Some(accepted)),
                    None => Err(error),
                };
            }
            Err(error) => return Err(error),
        }
    };
    let operation = PeerOperation::Deliver {
        request: record.intent.request.clone(),
    };
    let binding = operation.binding(owner.runtime_generation)?;
    let origin = DurableOrigin::Outbox {
        source: sender.address().clone(),
        request: record.intent.request.request_id,
        intent: record.queued.clone(),
    };
    let identity = Arc::new(DurableIdentity {
        origin: origin.clone(),
        binding: binding.clone(),
        record: DurableRecord::Outbox(Box::new(record.clone())),
        effects: Default::default(),
    });
    let nonce = Uuid::new_v4();
    {
        let mut registry = host.invocation_registry().durable.lock().unwrap();
        registry.retain(|_, identity| identity.strong_count() != 0);
        registry.insert(nonce, Arc::downgrade(&identity));
    }
    let principal = PeerPrincipal::Durable(DurablePrincipal {
        owner: host.generation(),
        nonce,
        origin,
    });
    let call = PeerCall::Execute {
        principal,
        binding,
        operation: Box::new(operation),
    };
    let caller = host.peer_identity()?;
    let host = host.clone();
    let reply = tokio::spawn(async move {
        let _identity = identity;
        if owner.runtime_generation == host.generation() {
            Box::pin(handle_peer_call(host, caller, call)).await
        } else {
            port.call(owner, call).await
        }
    })
    .await
    .map_err(io::Error::other)??;
    match reply {
        PeerReply::Accepted(accepted) => Ok(Some(accepted)),
        _ => Err(protocol("peer delivery returned another reply kind")),
    }
}

async fn deleted_delivery(
    host: &RuntimeHost,
    sender: &super::super::FlowWorker,
    record: &SendRecord,
) -> io::Result<Option<Acceptance>> {
    use crate::peer_authority::{AuthorityKey, AuthorityMode, AuthorityPlan};
    let guard = Arc::new(
        host.authority_domain()
            .ok_or_else(|| protocol("deleted delivery authority is unavailable"))?
            .acquire(
                AuthorityPlan::new([
                    (
                        AuthorityKey::Session(sender.address().session_id.0),
                        AuthorityMode::Shared,
                    ),
                    (
                        AuthorityKey::Session(record.intent.request.recipient.session_id.0),
                        AuthorityMode::Exclusive,
                    ),
                ])?,
                tokio_util::sync::CancellationToken::new(),
                std::time::Instant::now() + std::time::Duration::from_secs(15),
            )
            .await?,
    );
    let expected = record.clone();
    sender
        .query_with_authority(guard.clone(), move |store| {
            let current = store
                .send_summary(expected.intent.request.request_id)
                .ok_or_else(|| denied("original deleted delivery intent is unavailable"))?;
            if current.queued != expected.queued
                || current.recipient != expected.intent.request.recipient
                || current.authorization != SendAuthorization::PeerGrants
            {
                return Err(denied("deleted delivery differs from its original outbox"));
            }
            Ok(())
        })
        .await?;
    let data = host.peer_identity()?.canonical_data_root;
    let request = record.intent.request.clone();
    crate::read_work::capture(move || {
        let _authority = guard;
        crate::flow_store::RuntimeStore::inspect_deleted_delivery(&data, &request)
    })
    .await
    .map_err(io::Error::other)?
}

pub(super) async fn subscription_events(
    host: &RuntimeHost,
    recipient: &super::super::FlowWorker,
    source: &FlowRef,
    after: u64,
) -> io::Result<PeerFlowEventPage> {
    let publisher = source.clone();
    let (record, observed_at) = recipient
        .request(move |store| {
            let record = store
                .flow_subscriptions()
                .find(|record| {
                    record.subscription.source == publisher
                        && record.status
                            == crate::flow_store::subscription::SubscriptionStatus::Active
                        && record.cursor == after
                })
                .cloned()
                .ok_or_else(|| {
                    denied("observation has no matching active original subscription")
                })?;
            Ok((record, store.cursor().clone()))
        })
        .await?;
    let port = host
        .peer_port()
        .ok_or_else(|| protocol("peer observation transport is unavailable"))?;
    let owner = if host.session(source.session_id.0).is_ok() {
        host.peer_identity()?
    } else {
        port.locate(PeerTarget::Session(source.session_id.0))
            .await?
    };
    let operation = PeerOperation::Events {
        recipient: recipient.address().clone(),
        source: source.clone(),
        after,
        limit: 64,
    };
    let binding = operation.binding(owner.runtime_generation)?;
    let origin = DurableOrigin::Subscription {
        recipient: recipient.address().clone(),
        id: record.subscription.id,
        registration: observed_at.clone(),
    };
    let identity = Arc::new(DurableIdentity {
        origin: origin.clone(),
        binding: binding.clone(),
        record: DurableRecord::Subscription {
            record: Box::new(record),
            observed_at,
        },
        effects: Default::default(),
    });
    let nonce = Uuid::new_v4();
    {
        let mut registry = host.invocation_registry().durable.lock().unwrap();
        registry.retain(|_, identity| identity.strong_count() != 0);
        registry.insert(nonce, Arc::downgrade(&identity));
    }
    let principal = DurablePrincipal {
        owner: host.generation(),
        nonce,
        origin,
    };
    super::capture::events::collect(host, principal, identity, owner, operation).await
}

pub(super) async fn subscription_call(
    host: &RuntimeHost,
    principal: &DurablePrincipal,
    identity: &Arc<DurableIdentity>,
    owner: PeerIdentity,
    operation: PeerOperation,
) -> io::Result<PeerReply> {
    let DurableRecord::Subscription { record, .. } = &identity.record else {
        return Err(denied(
            "event transfer requires its original durable subscription",
        ));
    };
    let target = &record.subscription.source;
    let allowed = match &operation {
        PeerOperation::Events {
            recipient,
            source,
            after,
            ..
        } => {
            recipient == &source_of_record(principal)?
                && source == target
                && *after == record.cursor
        }
        PeerOperation::CaptureChunk {
            target: selected,
            read: super::capture::ReadKind::Events,
            ..
        }
        | PeerOperation::ReleaseCapture {
            target: selected,
            read: super::capture::ReadKind::Events,
            ..
        } => selected == target,
        _ => false,
    };
    if !allowed {
        return Err(denied(
            "event transfer differs from its original subscription operation",
        ));
    }
    let binding = operation.binding(owner.runtime_generation)?;
    let registered = Arc::new(RegisteredEffect {
        binding: binding.clone(),
        operation: operation.clone(),
    });
    {
        let mut effects = identity.effects.lock().unwrap();
        effects.retain(|_, effect| effect.strong_count() != 0);
        effects.insert(binding.transaction, Arc::downgrade(&registered));
    }
    let target = target.clone();
    let principal = principal.clone();
    let identity = identity.clone();
    let host = host.clone();
    let (reply, received) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _effect = registered;
        let _registration = identity;
        let call = PeerCall::Execute {
            principal: PeerPrincipal::Durable(principal.clone()),
            binding,
            operation: Box::new(operation),
        };
        let result = if owner.runtime_generation == host.generation() {
            match host.peer_identity() {
                Ok(caller) => Box::pin(handle_peer_call(host.clone(), caller, call)).await,
                Err(error) => Err(error),
            }
        } else {
            match host.peer_port() {
                Some(port) => port.call(owner, call).await,
                None => Err(protocol("subscription transport is unavailable")),
            }
        };
        if let Err(Ok(PeerReply::Capture(transfer))) = reply.send(result) {
            super::capture::events::release_unread(&host, principal, transfer, target);
        }
    });
    received.await.map_err(io::Error::other)?
}

fn source_of_record(principal: &DurablePrincipal) -> io::Result<FlowRef> {
    source(principal)
}

pub(super) fn source(principal: &DurablePrincipal) -> io::Result<FlowRef> {
    match &principal.origin {
        DurableOrigin::Outbox { source, .. } => Ok(source.clone()),
        DurableOrigin::Subscription { recipient, .. } => Ok(recipient.clone()),
    }
}

pub(super) async fn validate(
    host: &RuntimeHost,
    principal: DurablePrincipal,
    binding: EffectBinding,
) -> io::Result<InvocationWitness> {
    let guard = super::client::read_guard(host, source(&principal)?.session_id.0).await?;
    validate_prepared(host, principal, binding, guard).await
}

pub(super) async fn validate_prepared(
    host: &RuntimeHost,
    principal: DurablePrincipal,
    binding: EffectBinding,
    guard: Arc<crate::peer_authority::AuthorityGuard>,
) -> io::Result<InvocationWitness> {
    if principal.owner != host.generation() {
        return Err(denied("durable source owner generation changed"));
    }
    let identity = host
        .invocation_registry()
        .durable
        .lock()
        .unwrap()
        .get(&principal.nonce)
        .and_then(Weak::upgrade)
        .ok_or_else(|| denied("original durable operation registration is unavailable"))?;
    let registered = identity
        .effects
        .lock()
        .unwrap()
        .get(&binding.transaction)
        .and_then(Weak::upgrade);
    if (identity.binding != binding
        && registered
            .as_ref()
            .is_none_or(|effect| effect.binding != binding))
        || serde_json::to_vec(&identity.origin).map_err(io::Error::other)?
            != serde_json::to_vec(&principal.origin).map_err(io::Error::other)?
    {
        return Err(denied(
            "durable effect differs from its original committed operation",
        ));
    }
    let source = source(&principal)?;
    if host.is_transient(&source) && binding.target != AccessTarget::Flow(source.clone()) {
        return Err(denied(
            "temporary source cannot delegate durable cross-flow authority",
        ));
    }
    let session = host.session(source.session_id.0)?;
    let worker = session
        .loaded_flow(&source.instance_id)
        .ok_or_else(|| denied("durable source owner is not available"))?;
    let lease = match &identity.record {
        DurableRecord::Outbox(expected) => {
            let expected = expected.clone();
            let lease = expected.intent.lease.clone();
            worker
                .query_with_authority(guard.clone(), move |store| {
                    let summary = store
                        .send_summary(expected.intent.request.request_id)
                        .ok_or_else(|| denied("durable send intent is no longer recorded"))?;
                    if summary.queued != expected.queued
                        || summary.recipient != expected.intent.request.recipient
                        || summary.authorization != SendAuthorization::PeerGrants
                    {
                        return Err(denied(
                            "durable outbox reference differs from its recorded intent",
                        ));
                    }
                    Ok(())
                })
                .await?;
            Some(lease)
        }
        DurableRecord::Subscription {
            record,
            observed_at,
        } => {
            let expected = record.clone();
            let observed_at = observed_at.clone();
            worker
                .query_with_authority(guard.clone(), move |store| {
                    let current = store
                        .flow_subscriptions()
                        .find(|current| current.subscription.id == expected.subscription.id)
                        .ok_or_else(|| denied("durable subscription is no longer recorded"))?;
                    if current != expected.as_ref()
                        || current.status
                            != crate::flow_store::subscription::SubscriptionStatus::Active
                        || observed_at.journal_id() != store.cursor().journal_id()
                        || observed_at.sequence() > store.cursor().sequence()
                    {
                        return Err(denied(
                            "durable subscription changed after its original observation",
                        ));
                    }
                    Ok(())
                })
                .await?;
            None
        }
    };
    let snapshot = session.snapshot_with_authority(guard.clone()).await?;
    let source_grants = binding
        .actions
        .iter()
        .copied()
        .filter(|action| {
            session
                .access
                .read()
                .unwrap()
                .allows_scope(&source, &binding.target, *action)
        })
        .collect();
    Ok(InvocationWitness {
        source,
        lease,
        binding,
        access_cursor: snapshot.cursor,
        source_grants,
        registered_authority: None,
    })
}
