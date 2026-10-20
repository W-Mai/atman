//! Explicit peer approval access without sharing brokers or impersonating a user.

use super::{FlowAction, FlowClient, FlowRef, denied};
use crate::permission::{
    ApprovalTarget, DecisionAuthority, DelegatedDecisionAuthority, PermissionAction,
    PermissionError, PermissionRequest, PermissionRequestId, PermissionRequestState,
    ResolveOutcome,
};
use std::{io, sync::Arc};

impl FlowClient {
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
                    let available = match state.state() {
                        atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Idle) => {
                            state.can_start_turn()
                                && scheduled.load(std::sync::atomic::Ordering::Acquire)
                        }
                        atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(
                            active,
                        )) => state
                            .can_receive_at_boundary(&active.lease)
                            .unwrap_or(false),
                        _ => false,
                    };
                    if !available
                        || reviewer_broker.list().iter().any(|request| {
                            matches!(request.state, PermissionRequestState::Pending { .. })
                        })
                    {
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
        self.authorize_permission_review(&target)?;
        if action == PermissionAction::Defer {
            return Err(denied(
                "peer review leaves undecided requests with the user",
            ));
        }
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
