//! Explicit peer approval access without sharing brokers or impersonating a user.

use super::{FlowAction, FlowClient, FlowRef, denied};
use crate::permission::{
    ApprovalTarget, DecisionAuthority, DelegatedDecisionAuthority, PermissionAction,
    PermissionError, PermissionRequest, PermissionRequestId, PermissionRequestState,
    ResolveOutcome,
};
use std::{io, sync::Arc};

pub(in crate::flow_runtime) fn review_available(
    state: &atman_rt::FlowInstance<crate::event::AtmanUuid>,
    scheduled: bool,
    broker: &crate::permission::PermissionBroker,
) -> bool {
    let ready = match state.state() {
        atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Idle) => {
            state.can_start_turn() && scheduled
        }
        atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(active)) => state
            .can_receive_at_boundary(&active.lease)
            .unwrap_or(false),
        _ => false,
    };
    ready && !broker.has_pending_requests()
}

impl FlowClient {
    /// Offer each eligible live reviewer at most once. The caller races this future against the broker's resolution; exhaustion leaves the request with the user.
    pub(crate) async fn route_permission_review(
        &self,
        request: &PermissionRequest,
    ) -> io::Result<()> {
        if self.host.peer_port().is_some() {
            return self.route_remote_permission_review(request).await;
        }
        let mut candidates = self.host.owned_flows()?;
        candidates.sort_by_key(|worker| {
            (
                worker.address().session_id.to_string(),
                worker.address().instance_id.to_string(),
            )
        });
        for worker in candidates {
            self.authenticate()?;
            if worker.address() == self.source() {
                continue;
            }
            let session = match self.host.session(worker.address().session_id.0) {
                Ok(session) => session,
                Err(_) => continue,
            };
            if !self.host.allows_access(
                &session,
                worker.address(),
                self.source(),
                FlowAction::ApprovePermission,
            ) {
                continue;
            }
            // Register before offering or sampling state, including fast review turns.
            let mut commits = worker.subscribe_commits();
            let mut grants = self.host.changes();
            let mut source = self.state.clone();
            let mut scheduling = worker.scheduling_progress.subscribe();
            let accepted = match self
                .offer_permission_review(
                    worker.address().clone(),
                    request.request_id.clone(),
                    request.revision,
                )
                .await
            {
                Ok(accepted) => accepted,
                Err(_) => continue,
            };
            let broker = worker.context().permission_broker();
            loop {
                commits.borrow_and_update();
                grants.borrow_and_update();
                source.borrow_and_update();
                let progress = scheduling.borrow_and_update().clone();
                let scheduler_live = !matches!(
                    progress,
                    crate::flow_runtime::scheduler::SchedulingProgress::Stopped
                        | crate::flow_runtime::scheduler::SchedulingProgress::Failed(_)
                );
                self.authenticate()?;
                let message = accepted.message_id;
                let observed = worker
                    .request(move |store| {
                        Ok((
                            store.state().clone(),
                            store.pending_inputs().any(|entry| entry.id == message),
                            store.can_admit_turn(),
                        ))
                    })
                    .await;
                let Ok((state, pending, can_admit)) = observed else {
                    break;
                };
                if !self.host.allows_access(
                    &session,
                    worker.address(),
                    self.source(),
                    FlowAction::ApprovePermission,
                ) || !review_available(&state, scheduler_live && can_admit, &broker)
                {
                    break;
                }
                if matches!(
                    state.state(),
                    atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Idle)
                ) && (!pending
                    || matches!(&progress,
                        crate::flow_runtime::scheduler::SchedulingProgress::Idle(cursor, scopes)
                        if cursor.journal_id() == accepted.receipt.journal_id()
                            && cursor.sequence() >= accepted.receipt.sequence()
                            && !scopes.has_changed()
                    ))
                {
                    break;
                }
                tokio::select! {
                    changed = commits.changed() => if changed.is_err() { break; },
                    changed = grants.changed() => if changed.is_err() { break; },
                    changed = source.changed() => if changed.is_err() { break; },
                    changed = scheduling.changed() => if changed.is_err() { break; },
                }
            }
        }
        self.route_remote_permission_review(request).await
    }

    async fn route_remote_permission_review(&self, request: &PermissionRequest) -> io::Result<()> {
        use crate::flow_runtime::{
            peer::{PeerCall, PeerOperation, PeerReply, PeerTarget},
            peer_profile::{ProfileOperation, ProfileReply},
        };
        let Some(port) = self.host.peer_port() else {
            return Ok(());
        };
        self.authenticate()?;
        let view = if let Some(profile) = self.host.profile_control_port() {
            profile.snapshot().await.ok()
        } else {
            match port.locate(PeerTarget::Profile).await {
                Ok(owner) => match port
                    .call(owner, PeerCall::Profile(ProfileOperation::Snapshot))
                    .await
                {
                    Ok(PeerReply::Profile(ProfileReply::View(view))) => Some(view),
                    _ => None,
                },
                Err(_) => None,
            }
        };
        // Profile metadata supplies candidate addresses only; each offer samples actual authority again.
        let mut candidates: Vec<_> = self
            .host
            .owned_flows()?
            .into_iter()
            .map(|worker| worker.address().clone())
            .filter(|reviewer| reviewer != self.source())
            .collect();
        if let Some(binding) = view.and_then(|view| view.profile.management) {
            candidates.extend(
                binding
                    .grants
                    .iter()
                    .filter(|grant| {
                        grant.allows(&grant.grantee, self.source(), FlowAction::ApprovePermission)
                    })
                    .map(|grant| grant.grantee.clone())
                    .filter(|reviewer| reviewer != self.source()),
            );
        }
        candidates.sort_by_key(|reviewer| (reviewer.session_id.0, reviewer.instance_id.0.0));
        candidates.dedup();
        for reviewer in candidates {
            self.authenticate()?;
            let local = self
                .host
                .session(reviewer.session_id.0)
                .ok()
                .and_then(|session| session.loaded_flow(&reviewer.instance_id));
            let mut commits = local.as_ref().map(|worker| worker.subscribe_commits());
            let mut scheduling = local
                .as_ref()
                .map(|worker| worker.scheduling_progress.subscribe());
            let mut grants = self.host.changes();
            let mut source_state = self.state.clone();
            let accepted = match self
                .offer_permission_review(
                    reviewer.clone(),
                    request.request_id.clone(),
                    request.revision,
                )
                .await
            {
                Ok(accepted) => accepted,
                Err(_) => continue,
            };
            loop {
                if let Some(changes) = &mut commits {
                    changes.borrow_and_update();
                }
                if let Some(changes) = &mut scheduling {
                    changes.borrow_and_update();
                }
                grants.borrow_and_update();
                source_state.borrow_and_update();
                self.authenticate()?;
                let source = self
                    .source_session()?
                    .loaded_flow(&self.source().instance_id)
                    .ok_or_else(|| denied("original approval owner is unavailable"))?;
                if source
                    .context()
                    .permission_broker()
                    .get(&request.request_id)
                    .is_none_or(|current| {
                        current.revision != request.revision
                            || !matches!(
                                current.state,
                                PermissionRequestState::Pending {
                                    target: ApprovalTarget::User
                                }
                            )
                    })
                {
                    return Ok(());
                }
                match self
                    .peer_operation(PeerOperation::PermissionReviewProgress {
                        reviewer: reviewer.clone(),
                        request: request.request_id.clone(),
                        revision: request.revision,
                        accepted: accepted.clone(),
                    })
                    .await
                {
                    Ok(Some(PeerReply::PermissionReviewProgress { waiting: true })) => {
                        tokio::select! {
                            _ = async { match &mut commits { Some(changes) => { let _ = changes.changed().await; }, None => std::future::pending::<()>().await } } => {},
                            _ = async { match &mut scheduling { Some(changes) => { let _ = changes.changed().await; }, None => std::future::pending::<()>().await } } => {},
                            _ = grants.changed() => {},
                            _ = source_state.changed() => {},
                            _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {},
                        }
                    }
                    _ => break,
                }
            }
        }
        Ok(())
    }

    /// Offer this invocation's pending request to an authorized, available peer through its durable inbox. Acceptance does not resolve the request or remove user access; identical retries reuse the original receipt.
    pub async fn offer_permission_review(
        &self,
        reviewer: FlowRef,
        request_id: PermissionRequestId,
        expected_revision: u64,
    ) -> io::Result<crate::flow_store::inbox::Acceptance> {
        use crate::flow_store::inbox::{
            DeliveryMode, DeliveryRequest, InboxPayload, SenderIdentity,
        };
        self.authenticate()?;
        if &reviewer == self.source() {
            return Err(denied("a flow cannot review its own requests"));
        }
        peer_reply!(
            self,
            crate::flow_runtime::peer::PeerOperation::OfferPermission {
                reviewer: reviewer.clone(),
                request: request_id.clone(),
                revision: expected_revision,
            },
            Accepted
        );
        let unavailable = || denied("reviewer is unavailable or outside current approval grants");
        let session = self
            .host
            .session(reviewer.session_id.0)
            .map_err(|_| unavailable())?;
        let worker = session
            .loaded_flow(&reviewer.instance_id)
            .ok_or_else(unavailable)?;
        let source = self.host.flow(self.source()).await?;
        let broker = source.context().permission_broker();
        let reviewer_broker = worker.context().permission_broker();
        let scheduled = worker.scheduler_owned.clone();
        let client = self.clone();
        let request = DeliveryRequest {
            expected_head: None,
            request_id: request_id.0,
            sender: SenderIdentity::Host(self.source().session_id.0),
            recipient: reviewer.clone(),
            delivery: DeliveryMode::AtBoundary,
            cause: None,
            correlation: None,
            scope: None,
            payload: InboxPayload::Internal {
                kind: "permission_review".into(),
                content: serde_json::json!({
                    "target_handle": format!("{}/{}", self.source().session_id, self.source().instance_id),
                    "request_id": request_id,
                    "revision": expected_revision,
                    "requesting_run_id": self.lease.run,
                }),
            },
        };
        worker
            .request(move |store| {
                client.authenticate()?;
                let state = store.state().clone();
                let can_admit = store.can_admit_turn();
                store.accept_input(request, |_| {
                    if !client.host.allows_access(
                        &session,
                        &reviewer,
                        client.source(),
                        FlowAction::ApprovePermission,
                    ) {
                        return Err(unavailable());
                    }
                    let pending = broker
                        .get(&request_id)
                        .ok_or_else(|| denied("permission request was not found"))?;
                    if pending.requesting_run_id.0 != client.lease.run.0
                        || pending.revision != expected_revision
                        || !matches!(
                            pending.state,
                            PermissionRequestState::Pending {
                                target: ApprovalTarget::User
                            }
                        )
                    {
                        return Err(denied(
                            "permission request is not pending for this invocation and revision",
                        ));
                    }
                    if !review_available(
                        &state,
                        scheduled.load(std::sync::atomic::Ordering::Acquire) && can_admit,
                        &reviewer_broker,
                    ) {
                        return Err(denied(
                            "reviewer is paused, unavailable, or awaiting approval",
                        ));
                    }
                    Ok(())
                })
            })
            .await
    }

    /// Inspect actionable review detail, redacted by the requesting instance before delivery.
    pub async fn permission_review(
        &self,
        target: FlowRef,
        request_id: PermissionRequestId,
    ) -> io::Result<serde_json::Value> {
        peer_reply!(
            self,
            crate::flow_runtime::peer::PeerOperation::PermissionReview {
                target: target.clone(),
                request: request_id.clone()
            },
            PermissionReview
        );
        self.authorize_permission_review(&target)?;
        let worker = self.host.flow(&target).await?;
        self.authorize_permission_review(&target)?;
        let request = worker
            .context()
            .permission_broker()
            .get(&request_id)
            .filter(|request| {
                matches!(
                    request.state,
                    PermissionRequestState::Pending {
                        target: ApprovalTarget::User
                    }
                )
            })
            .ok_or_else(|| denied("permission request was not found or is not visible"))?;
        Ok(
            crate::permission_audit::PermissionRequestAudit::flow_review(
                &request,
                worker.context().sink().redactor().as_deref(),
            ),
        )
    }

    fn authorize_permission_review(&self, target: &FlowRef) -> io::Result<()> {
        if target == self.source() {
            return Err(denied("a flow cannot approve its own tool requests"));
        }
        self.authorize(target, FlowAction::ApprovePermission)
    }

    /// Read pending user approvals under an explicit peer-review grant. This does not transfer a request or authorize an effect.
    pub async fn permission_requests(&self, target: FlowRef) -> io::Result<Vec<PermissionRequest>> {
        peer_reply!(
            self,
            crate::flow_runtime::peer::PeerOperation::Permissions {
                target: target.clone()
            },
            Permissions
        );
        self.authorize_permission_review(&target)?;
        let worker = self.host.flow(&target).await?;
        self.authorize_permission_review(&target)?;
        Ok(worker
            .context()
            .permission_broker()
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
            .collect())
    }

    /// Approve or deny one observed peer request. The authenticated peer retains its own authority ceiling and cannot grant a reusable exception.
    pub async fn resolve_permission(
        &self,
        target: FlowRef,
        request_id: PermissionRequestId,
        expected_revision: u64,
        action: PermissionAction,
        reason: Option<String>,
    ) -> io::Result<ResolveOutcome> {
        if target == *self.source() {
            return Err(denied("a flow cannot approve its own tool requests"));
        }
        if action == PermissionAction::Defer {
            return Err(denied(
                "peer review leaves undecided requests with the user",
            ));
        }
        peer_reply!(
            self,
            crate::flow_runtime::peer::PeerOperation::ResolvePermission {
                target: target.clone(),
                request: request_id.clone(),
                revision: expected_revision,
                action,
                reason: reason.clone(),
            },
            PermissionResolved
        );
        self.authorize_permission_review(&target)?;
        let source = self.host.flow(self.source()).await?;
        let target_worker = self.host.flow(&target).await?;
        let identity = source
            .context()
            .flow_registry
            .lookup_run(&crate::event::FlowRunId::new(self.lease.run.0))
            .filter(|identity| identity.instance.as_ref() == Some(self.source()))
            .ok_or_else(|| denied("peer approval requires the admitted execution identity"))?;
        let client = self.clone();
        let checked_target = target.clone();
        let authority = DecisionAuthority::Delegated(DelegatedDecisionAuthority {
            identity,
            requester: target,
            request_id: request_id.clone(),
            validate: Arc::new(move || {
                client
                    .authorize_permission_review(&checked_target)
                    .map_err(|_| PermissionError::ActorNotAuthorized)
            }),
        });
        target_worker
            .context()
            .permission_broker()
            .resolve_at_revision(
                &request_id,
                expected_revision,
                &authority,
                action,
                None,
                reason,
            )
            .map_err(|error| denied(&error.to_string()))
    }
}
