//! Bounded delivery of one original durable subscription page.

use super::*;
#[cfg(test)]
mod tests;

async fn authorized(
    host: &RuntimeHost,
    principal: &PeerPrincipal,
    binding: &EffectBinding,
) -> io::Result<Arc<AuthorityGuard>> {
    let PeerPrincipal::Durable(actor) = principal else {
        return Err(denied(
            "event transfer requires its original durable subscription",
        ));
    };
    let DurableOrigin::Subscription { recipient, .. } = &actor.origin else {
        return Err(denied(
            "another durable operation cannot read subscription events",
        ));
    };
    let AccessTarget::Flow(source) = &binding.target else {
        return Err(protocol("event transfer requires its original publisher"));
    };
    if binding.actions != [FlowAction::Watch] || binding.finishing_context {
        return Err(denied(
            "event transfer changed its original Watch authority",
        ));
    }
    let guard = Arc::new(
        host.peer_authority(source.session_id.0)?
            .acquire(
                AuthorityPlan::new([
                    (AuthorityKey::Profile, AuthorityMode::Shared),
                    (
                        AuthorityKey::Session(recipient.session_id.0),
                        AuthorityMode::Shared,
                    ),
                    (
                        AuthorityKey::Session(source.session_id.0),
                        AuthorityMode::Shared,
                    ),
                ])?,
                tokio_util::sync::CancellationToken::new(),
                Instant::now() + Duration::from_secs(15),
            )
            .await?,
    );
    let witness =
        super::super::dispatch::validate_source(host, principal, binding, guard.clone()).await?;
    if witness.source != *recipient || witness.binding != *binding || witness.lease.is_some() {
        return Err(denied("event page belongs to another durable subscription"));
    }
    if !witness.source_grants.contains(&FlowAction::Watch)
        && !super::super::dispatch::profile_allows(
            host,
            recipient,
            &binding.target,
            FlowAction::Watch,
        )
        .await?
    {
        return Err(denied("event source is outside current Watch grants"));
    }
    Ok(guard)
}

pub(in crate::flow_runtime) async fn execute(
    host: RuntimeHost,
    principal: PeerPrincipal,
    binding: EffectBinding,
    operation: PeerOperation,
) -> io::Result<PeerReply> {
    let PeerPrincipal::Durable(actor) = &principal else {
        return Err(denied("event transfer requires a durable subscription"));
    };
    let DurableOrigin::Subscription {
        recipient: original,
        ..
    } = &actor.origin
    else {
        return Err(denied(
            "another durable operation cannot use event transfer",
        ));
    };
    match operation {
        PeerOperation::ReleaseCapture {
            target,
            token,
            read: ReadKind::Events,
        } => {
            retire_transfer(
                &host,
                actor.owner,
                actor.nonce,
                &target,
                token,
                ReadKind::Events,
            );
            Ok(PeerReply::CaptureReleased)
        }
        PeerOperation::CaptureChunk {
            target,
            token,
            offset,
            read: ReadKind::Events,
        } => {
            let _guard = authorized(&host, &principal, &binding).await?;
            read_chunk(
                &host,
                actor.owner,
                actor.nonce,
                &target,
                token,
                offset,
                ReadKind::Events,
            )
        }
        PeerOperation::Events {
            recipient,
            source,
            after,
            limit,
        } => {
            if &recipient != original || !(1..=64).contains(&limit) {
                return Err(denied(
                    "event page changed its original recipient or budget",
                ));
            }
            // Authenticate before an execution owner can be opened; this is preparation, not commit authority.
            drop(authorized(&host, &principal, &binding).await?);
            let session = host.session(source.session_id.0)?;
            let retention = session.retain_source().await?;
            let worker = host.flow(&source).await?;
            if worker.is_transient() {
                return Err(denied(
                    "a temporary Flow cannot publish durable subscription events",
                ));
            }
            let guard = authorized(&host, &principal, &binding).await?;
            let plan = worker
                .query_with_authority(guard.clone(), move |store| {
                    store.capture_events_plan(
                        after,
                        std::num::NonZeroUsize::new(limit)
                            .ok_or_else(|| protocol("event limit is zero"))?,
                        MAX_BYTES,
                    )
                })
                .await?;
            drop(guard);
            let native = actor.owner == host.generation();
            let prepared = crate::read_work::capture(move || {
                let page: PeerFlowEventPage = plan.prepare()?.into();
                if native {
                    return Ok::<_, io::Error>((Ok(page), retention));
                }
                let mut bytes = Bounded(Vec::new());
                serde_json::to_writer(&mut bytes, &page).map_err(io::Error::other)?;
                Ok((
                    if bytes.0.len() <= CHUNK {
                        Ok(page)
                    } else {
                        Err(bytes.0)
                    },
                    retention,
                ))
            })
            .await
            .map_err(io::Error::other)??;
            let (prepared, retention) = prepared;
            let _guard = authorized(&host, &principal, &binding).await?;
            let reply = match prepared {
                Ok(page) => PeerReply::Events(page),
                Err(bytes) => PeerReply::Capture(register_transfer(
                    &host,
                    actor.owner,
                    actor.nonce,
                    source,
                    ReadKind::Events,
                    bytes,
                )?),
            };
            drop(retention);
            Ok(reply)
        }
        _ => Err(denied(
            "durable subscription transfer cannot perform another operation family",
        )),
    }
}

struct ReleaseEvents {
    runtime: tokio::runtime::Handle,
    host: RuntimeHost,
    principal: DurablePrincipal,
    owner: PeerIdentity,
    target: FlowRef,
    token: Uuid,
}

pub(in crate::flow_runtime) fn release_unread(
    host: &RuntimeHost,
    principal: DurablePrincipal,
    transfer: Transfer,
    target: FlowRef,
) {
    let Ok(domain) = host.peer_identity() else {
        return;
    };
    if transfer.owner.profile_id != domain.profile_id
        || transfer.owner.canonical_data_root != domain.canonical_data_root
    {
        return;
    }
    if !matches!(&principal.origin, DurableOrigin::Subscription { .. }) {
        return;
    }
    drop(ReleaseEvents {
        runtime: tokio::runtime::Handle::current(),
        host: host.clone(),
        principal,
        owner: transfer.owner,
        target,
        token: transfer.token,
    });
}
impl Drop for ReleaseEvents {
    fn drop(&mut self) {
        let (host, principal, owner, target, token) = (
            self.host.clone(),
            self.principal.clone(),
            self.owner.clone(),
            self.target.clone(),
            self.token,
        );
        self.runtime.spawn(async move {
            let operation = PeerOperation::ReleaseCapture {
                target,
                token,
                read: ReadKind::Events,
            };
            let Ok(binding) = operation.binding(owner.runtime_generation) else {
                return;
            };
            let call = PeerCall::Execute {
                principal: PeerPrincipal::Durable(principal),
                binding,
                operation: Box::new(operation),
            };
            if owner.runtime_generation == host.generation() {
                if let Ok(caller) = host.peer_identity() {
                    let _ = handle_peer_call(host, caller, call).await;
                }
            } else if let Some(port) = host.peer_port() {
                let _ = port.call(owner, call).await;
            }
        });
    }
}

pub(in crate::flow_runtime) async fn collect(
    host: &RuntimeHost,
    principal: DurablePrincipal,
    identity: Arc<super::super::durable::DurableIdentity>,
    owner: PeerIdentity,
    operation: PeerOperation,
) -> io::Result<PeerFlowEventPage> {
    let PeerOperation::Events { source, after, .. } = &operation else {
        return Err(protocol(
            "event collector requires its original page request",
        ));
    };
    let source = source.clone();
    let after = *after;
    let reply =
        super::super::durable::subscription_call(host, &principal, &identity, owner, operation)
            .await?;
    let page = match reply {
        PeerReply::Events(page) => page,
        PeerReply::Capture(transfer) => {
            let domain = host.peer_identity()?;
            if transfer.owner.profile_id != domain.profile_id
                || transfer.owner.canonical_data_root != domain.canonical_data_root
                || transfer.length > MAX_BYTES
            {
                return Err(protocol(
                    "event transfer belongs to another domain or exceeds its capacity",
                ));
            }
            let _release = ReleaseEvents {
                runtime: tokio::runtime::Handle::current(),
                host: host.clone(),
                principal: principal.clone(),
                owner: transfer.owner.clone(),
                target: source.clone(),
                token: transfer.token,
            };
            let mut bytes = Vec::with_capacity(transfer.length);
            while bytes.len() < transfer.length {
                let offset = bytes.len();
                let reply = super::super::durable::subscription_call(
                    host,
                    &principal,
                    &identity,
                    transfer.owner.clone(),
                    PeerOperation::CaptureChunk {
                        target: source.clone(),
                        token: transfer.token,
                        offset,
                        read: ReadKind::Events,
                    },
                )
                .await?;
                match reply {
                    PeerReply::CaptureChunk {
                        offset: actual,
                        bytes: chunk,
                    } if actual == offset
                        && !chunk.is_empty()
                        && chunk.len() <= CHUNK
                        && chunk.len() <= transfer.length - offset =>
                    {
                        bytes.extend(chunk)
                    }
                    _ => {
                        return Err(protocol(
                            "event page changed its original transfer position",
                        ));
                    }
                }
            }
            if blake3::hash(&bytes).to_hex().as_str() != transfer.digest {
                return Err(protocol("event page transfer digest changed"));
            }
            crate::read_work::capture(move || {
                serde_json::from_slice::<PeerFlowEventPage>(&bytes).map_err(io::Error::other)
            })
            .await
            .map_err(io::Error::other)??
        }
        _ => return Err(protocol("event publisher returned another reply kind")),
    };
    if page.source() != &source || page.after() != after {
        return Err(protocol(
            "event page belongs to another original source cursor",
        ));
    }
    Ok(page)
}
