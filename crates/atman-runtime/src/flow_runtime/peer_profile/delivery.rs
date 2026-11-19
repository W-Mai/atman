//! Delivery of an immutable Profile batch item to its actual Session owner.

use std::{
    io,
    sync::Arc,
    time::{Duration, Instant},
};

use atman_rt::InstanceState;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{ProfileOperation, ProfileReply};
use crate::{
    event_log::journal::CommitReceipt,
    flow_runtime::{
        host::RuntimeHost,
        peer::{PeerCall, PeerIdentity, PeerReply, PeerTarget},
    },
    flow_store::{
        FlowStore,
        access::{AccessTarget, FlowAction},
        control::{CommittedControl, ControlAction, ControlRequest},
        inbox::SenderIdentity,
        position::FlowPosition,
    },
    peer_authority::{AuthorityKey, AuthorityMode, AuthorityPlan},
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ControlDelivery {
    pub batch: Uuid,
    pub submitted: CommitReceipt,
    pub request: ControlRequest,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ControlDeliveryReply {
    pub accepted: CommittedControl,
    pub applied: Option<FlowPosition>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn denied(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

pub(crate) async fn deliver_remote(
    host: &RuntimeHost,
    batch: Uuid,
    request: &ControlRequest,
) -> io::Result<Option<ControlDeliveryReply>> {
    let Some(port) = host.peer_port() else {
        return Ok(None);
    };
    let profile = host
        .profile_handle()
        .ok_or_else(|| denied("only the original Profile owner delivers control items"))?;
    let record = profile.published_control(&request.sender, batch)?;
    if !record.items.iter().any(|item| item.request == *request) {
        return Err(denied(
            "control item differs from its original Profile batch",
        ));
    }
    let owner = if host.session(request.target.session_id.0).is_ok() {
        host.peer_identity()?
    } else {
        port.locate(PeerTarget::Session(request.target.session_id.0))
            .await?
    };
    match port
        .call(
            owner,
            PeerCall::ProfileDelivery(ControlDelivery {
                batch,
                submitted: record.submitted,
                request: request.clone(),
            }),
        )
        .await?
    {
        PeerReply::ProfileDelivery(reply) => Ok(Some(reply)),
        _ => Err(invalid("unexpected Profile delivery reply")),
    }
}

pub async fn deliver(
    host: &RuntimeHost,
    caller: &PeerIdentity,
    delivery: ControlDelivery,
) -> io::Result<ControlDeliveryReply> {
    let port = host
        .peer_port()
        .ok_or_else(|| denied("internal peer transport is unavailable"))?;
    let owner = port.locate(PeerTarget::Profile).await?;
    if owner != *caller {
        return Err(denied(
            "control delivery must come from the actual Profile writer",
        ));
    }
    if delivery.batch.is_nil() || delivery.submitted.journal_id != owner.profile_id {
        return Err(invalid("invalid Profile control origin"));
    }
    let target = delivery.request.target.clone();
    let worker = host.flow(&target).await?;
    if worker.is_transient() {
        return Err(denied(
            "Profile controls cannot target a temporary conversation",
        ));
    }
    let scopes = if matches!(
        delivery.request.action,
        ControlAction::Resume | ControlAction::ReleasePause | ControlAction::Recover
    ) {
        Some(worker.scope_admission().await)
    } else {
        None
    };
    let mut requirements = scopes
        .as_ref()
        .and_then(|scope| scope.as_ref().ok())
        .map_or_else(Vec::new, |scope| scope.authority_requirements());
    requirements.extend([
        (AuthorityKey::Profile, AuthorityMode::Shared),
        (
            AuthorityKey::Session(target.session_id.0),
            AuthorityMode::Exclusive,
        ),
    ]);
    if let SenderIdentity::Flow(source) = &delivery.request.sender {
        requirements.push((
            AuthorityKey::Session(source.session_id.0),
            AuthorityMode::Shared,
        ));
    }
    let guard = Arc::new(
        host.authority_domain()
            .ok_or_else(|| denied("peer authority domain is unavailable"))?
            .acquire(
                AuthorityPlan::new(requirements)?,
                tokio_util::sync::CancellationToken::new(),
                Instant::now() + Duration::from_secs(15),
            )
            .await?,
    );

    let evidence = ProfileOperation::ControlEvidence {
        sender: delivery.request.sender.clone(),
        batch: delivery.batch,
        target: target.clone(),
    };
    let evidence = if owner.runtime_generation == host.generation() {
        super::dispatch(host, evidence).await?
    } else {
        match port
            .call(owner.clone(), PeerCall::Profile(evidence))
            .await?
        {
            PeerReply::Profile(reply) => reply,
            _ => return Err(invalid("unexpected Profile control evidence")),
        }
    };
    let ProfileReply::ControlEvidence {
        submitted,
        request,
        cursor,
    } = evidence
    else {
        return Err(invalid("unexpected Profile control evidence kind"));
    };
    if submitted != delivery.submitted
        || request != delivery.request
        || cursor.journal_id != owner.profile_id
    {
        return Err(denied(
            "control delivery differs from its committed Profile item",
        ));
    }
    let previous = request.clone();
    let recorded = worker
        .query_with_authority(guard.clone(), move |store| {
            store.recorded_control(&previous)
        })
        .await?;
    let accepted = match recorded {
        Some(record) => record,
        None => {
            if let SenderIdentity::Flow(source) = &request.sender {
                let source_grant = if let Ok(session) = host.session(source.session_id.0) {
                    if host.is_transient(source) {
                        return Err(denied("temporary source cannot control a foreign target"));
                    }
                    let _ = session.snapshot_with_authority(guard.clone()).await?;
                    !host.is_transient(source)
                        && session.access.read().unwrap().allows_scope(
                            source,
                            &AccessTarget::Flow(target.clone()),
                            FlowAction::Control,
                        )
                } else {
                    let source_owner = port
                        .locate(PeerTarget::Session(source.session_id.0))
                        .await?;
                    match port
                        .call(
                            source_owner,
                            PeerCall::Access {
                                source: source.clone(),
                                target: AccessTarget::Flow(target.clone()),
                                action: FlowAction::Control,
                            },
                        )
                        .await?
                    {
                        PeerReply::Access { allowed, .. } => allowed,
                        _ => return Err(invalid("unexpected source grant evidence")),
                    }
                };
                let profile_grant = ProfileOperation::Authorize {
                    source: source.clone(),
                    target: AccessTarget::Flow(target.clone()),
                    action: FlowAction::Control,
                };
                let profile_grant = if owner.runtime_generation == host.generation() {
                    super::dispatch(host, profile_grant).await?
                } else {
                    match port
                        .call(owner.clone(), PeerCall::Profile(profile_grant))
                        .await?
                    {
                        PeerReply::Profile(reply) => reply,
                        _ => return Err(invalid("unexpected management grant evidence")),
                    }
                };
                let ProfileReply::Authorization {
                    profile_id,
                    cursor: grant_cursor,
                    allowed,
                } = profile_grant
                else {
                    return Err(invalid("unexpected management grant evidence kind"));
                };
                if profile_id != owner.profile_id
                    || grant_cursor != cursor
                    || (!source_grant && !allowed)
                {
                    return Err(denied("control permission was revoked"));
                }
            }
            let expected_target = target.clone();
            worker
                .control_with_authority(guard.clone(), request, move |current| {
                    if current == &expected_target {
                        Ok(())
                    } else {
                        Err(denied("control target owner changed"))
                    }
                })
                .await?
        }
    };
    let observed = accepted.clone();
    let applied = worker
        .query_with_authority(guard, move |store| {
            Ok(application_receipt(store, &observed))
        })
        .await?;
    Ok(ControlDeliveryReply { accepted, applied })
}

pub(crate) fn application_receipt(
    store: &FlowStore,
    accepted: &CommittedControl,
) -> Option<FlowPosition> {
    match accepted.update.request.action {
        ControlAction::Close => {
            (store.state().state() == &InstanceState::Closed).then(|| store.cursor().clone())
        }
        ControlAction::CancelTurn => store.cancel_application_receipt(accepted),
        _ => Some(accepted.receipt.clone()),
    }
}
