use std::sync::Arc;

use super::{dispatch::PreparedPeerEffect, *};
use crate::permission::{
    ApprovalTarget, DecisionAuthority, PermissionRequestState, PreparedPeerDecisionAuthority,
};

pub(super) async fn execute(
    host: &RuntimeHost,
    worker: super::super::FlowWorker,
    prepared: Arc<PreparedPeerEffect>,
    operation: PeerOperation,
) -> io::Result<PeerReply> {
    prepared.check(worker.address())?;
    let broker = worker.context().permission_broker();
    match operation {
        PeerOperation::Permissions { target } => {
            if target == prepared.witness.source {
                return Err(denied("a flow cannot review its own requests"));
            }
            Ok(PeerReply::Permissions(
                broker
                    .list()
                    .into_iter()
                    .filter(|request| {
                        matches!(
                            request.state,
                            PermissionRequestState::Pending {
                                target: ApprovalTarget::User
                            }
                        )
                    })
                    .collect(),
            ))
        }
        PeerOperation::PermissionReview { target, request } => {
            if target == prepared.witness.source {
                return Err(denied("a flow cannot review its own requests"));
            }
            let request = broker
                .get(&request)
                .filter(|request| {
                    matches!(
                        request.state,
                        PermissionRequestState::Pending {
                            target: ApprovalTarget::User
                        }
                    )
                })
                .ok_or_else(|| denied("permission request was not found or is not visible"))?;
            Ok(PeerReply::PermissionReview(
                crate::permission_audit::PermissionRequestAudit::flow_review(
                    &request,
                    worker.context().sink().redactor().as_deref(),
                ),
            ))
        }
        PeerOperation::ResolvePermission {
            target,
            request,
            revision,
            action,
            reason,
        } => {
            let effective = prepared
                .witness
                .registered_authority
                .clone()
                .ok_or_else(|| denied("peer reviewer has no registered running authority"))?;
            let authority = DecisionAuthority::Peer(PreparedPeerDecisionAuthority {
                reviewer: prepared.witness.source.clone(),
                run: crate::event::FlowRunId::new(
                    prepared
                        .witness
                        .lease
                        .as_ref()
                        .ok_or_else(|| {
                            denied("peer review requires its original invocation lease")
                        })?
                        .run
                        .0,
                ),
                requester: target,
                request_id: request.clone(),
                effective,
                guard: prepared.guard.clone(),
            });
            broker
                .resolve_at_revision(&request, revision, &authority, action, None, reason)
                .map(PeerReply::PermissionResolved)
                .map_err(|error| denied(&error.to_string()))
        }
        PeerOperation::OfferPermission {
            reviewer,
            request,
            revision,
        } => {
            reviewer_authorized(host, &prepared.witness.source, &reviewer).await?;
            if broker.has_pending_requests() {
                return Err(denied("reviewer already has a pending tool approval"));
            }
            let scheduled = worker.scheduler_owned.clone();
            let check = prepared.clone();
            let delivery = review_delivery(&prepared.witness, reviewer, request, revision)?;
            worker
                .request_with_authority(prepared.guard.clone(), move |store| {
                    check.check(store.state().address())?;
                    let ready = match store.state().state() {
                        atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Idle) => {
                            store.can_admit_turn()
                                && scheduled.load(std::sync::atomic::Ordering::Acquire)
                        }
                        atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(
                            active,
                        )) => store
                            .state()
                            .can_receive_at_boundary(&active.lease)
                            .unwrap_or(false),
                        _ => false,
                    };
                    if !ready {
                        return Err(denied("reviewer is unavailable"));
                    }
                    store
                        .accept_input(delivery, |_| Ok(()))
                        .map(PeerReply::Accepted)
                })
                .await
        }
        PeerOperation::PermissionReviewProgress {
            reviewer,
            request,
            revision,
            accepted,
        } => {
            reviewer_authorized(host, &prepared.witness.source, &reviewer).await?;
            let delivery = review_delivery(&prepared.witness, reviewer, request, revision)?;
            let progress = worker.scheduling_progress.borrow().clone();
            let scheduler_live = !matches!(
                progress,
                super::super::scheduler::SchedulingProgress::Stopped
                    | super::super::scheduler::SchedulingProgress::Failed(_)
            );
            worker.request_with_authority(prepared.guard.clone(), move |store| {
                prepared.check(store.state().address())?;
                if store.input_acceptance(&delivery)?.as_ref() != Some(&accepted) { return Err(denied("review progress receipt differs from the original offer")); }
                let pending = store.pending_inputs().any(|entry| entry.id == accepted.message_id);
                let available = super::super::access::permission_access::review_available(store.state(), scheduler_live && store.can_admit_turn(), &broker);
                let idle = matches!(store.state().state(), atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Idle));
                let settled = idle && (!pending || matches!(&progress, super::super::scheduler::SchedulingProgress::Idle(cursor, scopes) if cursor.journal_id() == accepted.receipt.journal_id() && cursor.sequence() >= accepted.receipt.sequence() && !scopes.has_changed()));
                Ok(PeerReply::PermissionReviewProgress { waiting: available && !settled })
            }).await
        }
        _ => Err(protocol("peer permission operation kind differs")),
    }
}

pub(super) async fn reviewer_authorized(
    host: &RuntimeHost,
    source: &FlowRef,
    reviewer: &FlowRef,
) -> io::Result<()> {
    if reviewer == source {
        return Err(denied("a flow cannot review its own requests"));
    }
    let session = host.session(reviewer.session_id.0)?;
    let local =
        session
            .access
            .read()
            .unwrap()
            .allows(reviewer, source, FlowAction::ApprovePermission);
    if local
        || super::dispatch::profile_allows(
            host,
            reviewer,
            &AccessTarget::Flow(source.clone()),
            FlowAction::ApprovePermission,
        )
        .await?
    {
        Ok(())
    } else {
        Err(denied("reviewer is outside current approval grants"))
    }
}

fn review_delivery(
    witness: &InvocationWitness,
    reviewer: FlowRef,
    request: PermissionRequestId,
    revision: u64,
) -> io::Result<DeliveryRequest> {
    let source = &witness.source;
    Ok(DeliveryRequest {
        expected_head: None,
        request_id: request.0,
        sender: crate::flow_store::inbox::SenderIdentity::Host(source.session_id.0),
        recipient: reviewer,
        delivery: crate::flow_store::inbox::DeliveryMode::AtBoundary,
        cause: None,
        correlation: None,
        scope: None,
        payload: crate::flow_store::inbox::InboxPayload::Internal {
            kind: "permission_review".into(),
            content: serde_json::json!({ "target_handle":format!("{}/{}", source.session_id, source.instance_id), "request_id":request, "revision":revision, "requesting_run_id":witness.lease.as_ref().ok_or_else(|| denied("permission offer requires its original invocation lease"))?.run }),
        },
    })
}
