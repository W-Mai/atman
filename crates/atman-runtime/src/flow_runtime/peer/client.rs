use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use super::*;
use crate::peer_authority::{AuthorityKey, AuthorityMode, AuthorityPlan};

impl FlowClient {
    pub(in crate::flow_runtime) async fn peer_send(
        &self,
        request: DeliveryRequest,
    ) -> io::Result<Option<super::super::send::SendAttempt>> {
        if self.host.peer_port().is_none() || self.host.is_transient(self.source()) {
            return Ok(None);
        }
        self.authenticate()?;
        let client = self.clone();
        tokio::spawn(async move {
            let session = client.source_session()?;
            let sender = session
                .loaded_flow(&client.source().instance_id)
                .ok_or_else(|| denied("original send owner is unavailable"))?;
            let lookup = request.clone();
            let previous = sender
                .request(move |store| {
                    store.send_retry(
                        &lookup,
                        None,
                        crate::flow_store::outbox::SendAuthorization::PeerGrants,
                    )
                })
                .await?;
            let record = match previous {
                Some(record) => record,
                None => {
                    let scope = match &request.scope {
                        Some(scope) => {
                            client
                                .host
                                .prepare_scope_admission(None, scope.clone())
                                .await
                        }
                        None => Ok(Default::default()),
                    };
                    let mut requirements = scope
                        .as_ref()
                        .ok()
                        .map_or_else(Vec::new, |scope| scope.authority_requirements());
                    requirements.extend([
                        (AuthorityKey::Profile, AuthorityMode::Shared),
                        (
                            AuthorityKey::Session(client.source().session_id.0),
                            AuthorityMode::Exclusive,
                        ),
                    ]);
                    let guard = Arc::new(
                        client
                            .host
                            .authority_domain()
                            .ok_or_else(|| protocol("send authority domain is unavailable"))?
                            .acquire(
                                AuthorityPlan::new(requirements)?,
                                tokio_util::sync::CancellationToken::new(),
                                Instant::now() + Duration::from_secs(15),
                            )
                            .await?,
                    );
                    let repeated = request.clone();
                    let repeated = sender
                        .request_with_authority(guard.clone(), move |store| {
                            store.send_retry(
                                &repeated,
                                None,
                                crate::flow_store::outbox::SendAuthorization::PeerGrants,
                            )
                        })
                        .await?;
                    if let Some(record) = repeated {
                        record
                    } else {
                        client.authenticate()?;
                        let _ = session.snapshot_with_authority(guard.clone()).await?;
                        let target = AccessTarget::Flow(request.recipient.clone());
                        let local = session.access.read().unwrap().allows_scope(
                            client.source(),
                            &target,
                            FlowAction::Send,
                        );
                        if !local
                            && !super::dispatch::profile_allows(
                                &client.host,
                                client.source(),
                                &target,
                                FlowAction::Send,
                            )
                            .await?
                        {
                            return Err(denied("send target is outside current access grants"));
                        }
                        let admitted = client.clone();
                        sender
                            .queue_send_with_authority(
                                guard,
                                scope,
                                client.lease.clone(),
                                request,
                                move |_| admitted.authenticate(),
                            )
                            .await?
                    }
                }
            };
            let receiver = client.clone();
            Ok(Some(
                super::super::send::attempt(&client.host, &sender, record, move |request| {
                    receiver.authorize_delivery(&request.recipient, FlowAction::Send)
                })
                .await,
            ))
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Cross-Session operations use the same prepared executor for native and remote owners.
    pub(in crate::flow_runtime) async fn peer_operation(
        &self,
        operation: PeerOperation,
    ) -> io::Result<Option<PeerReply>> {
        self.peer_operation_at(operation, None).await
    }

    pub(in crate::flow_runtime) async fn peer_operation_at(
        &self,
        operation: PeerOperation,
        owner: Option<PeerIdentity>,
    ) -> io::Result<Option<PeerReply>> {
        let Some(port) = self.host.peer_port() else {
            return Ok(None);
        };
        let target = operation.target();
        let finishing_context = matches!(&target, AccessTarget::Flow(flow) if flow == self.source())
            && operation.supports_finishing_context();
        if finishing_context {
            self.authenticate_context()?;
        } else {
            self.authenticate()?;
        }
        let owner = if let Some(owner) = owner {
            owner
        } else {
            match &target {
                AccessTarget::Flow(flow) => {
                    if self.host.session(flow.session_id.0).is_ok() {
                        self.host.peer_identity()?
                    } else {
                        port.locate(PeerTarget::Session(flow.session_id.0)).await?
                    }
                }
                AccessTarget::Session(session) => {
                    if self.host.session(*session).is_ok() {
                        self.host.peer_identity()?
                    } else {
                        port.locate(PeerTarget::Session(*session)).await?
                    }
                }
                AccessTarget::AllSessions => port.locate(PeerTarget::Profile).await?,
            }
        };
        let mut binding = operation.binding(owner.runtime_generation)?;
        binding.finishing_context = finishing_context;
        let registered = Arc::new(RegisteredEffect {
            binding: binding.clone(),
            operation: operation.clone(),
        });
        {
            let mut effects = self.identity.effects.lock().unwrap();
            effects.retain(|_, effect| effect.strong_count() != 0);
            effects.insert(binding.transaction, Arc::downgrade(&registered));
        }
        let principal = PeerPrincipal::Invocation(self.peer_principal());
        let host = self.host.clone();
        let caller = self.host.peer_identity()?;
        let invocation = self.identity.clone();
        // The original operation owns registration until the target response, even if its waiter drops.
        let result = tokio::spawn(async move {
            let _registered_binding = registered;
            let _invocation = invocation;
            let call = PeerCall::Execute {
                principal,
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
        .map_err(io::Error::other)?;
        Ok(Some(result?))
    }

    pub(super) async fn peer_witness(
        &self,
        binding: EffectBinding,
        guard: Arc<crate::peer_authority::AuthorityGuard>,
    ) -> io::Result<InvocationWitness> {
        let registered = self
            .identity
            .effects
            .lock()
            .unwrap()
            .get(&binding.transaction)
            .and_then(Weak::upgrade)
            .ok_or_else(|| denied("peer effect was not registered by its original invocation"))?;
        if registered.binding != binding {
            return Err(denied(
                "peer effect binding differs from its original operation",
            ));
        }
        if binding.finishing_context {
            self.authenticate_context()?;
        } else {
            self.authenticate()?;
        }
        let session = self.source_session()?;
        if let Some(source) = session.loaded_flow(&self.source().instance_id) {
            if source
                .context()
                .flow_registry
                .lookup_run(&crate::event::FlowRunId::new(self.lease.run.0))
                .is_some_and(|identity| !identity.principal_available())
            {
                return Err(denied("source execution no longer admits peer operations"));
            }
            if let PeerOperation::OfferPermission {
                request, revision, ..
            }
            | PeerOperation::PermissionReviewProgress {
                request, revision, ..
            } = &registered.operation
            {
                let pending = source
                    .context()
                    .permission_broker()
                    .get(request)
                    .filter(|pending| {
                        pending.requesting_run_id.0 == self.lease.run.0
                            && pending.revision == *revision
                            && matches!(
                                pending.state,
                                crate::permission::PermissionRequestState::Pending {
                                    target: crate::permission::ApprovalTarget::User
                                }
                            )
                    })
                    .ok_or_else(|| {
                        denied("permission offer no longer matches the source's pending request")
                    })?;
                let _ = pending;
            }
        }
        if self.host.is_transient(self.source())
            && binding.target != AccessTarget::Flow(self.source().clone())
        {
            return Err(denied(
                "temporary source cannot delegate cross-flow authority",
            ));
        }
        if let PeerOperation::InputStatus { target, request_id } = &registered.operation {
            let target = target.clone();
            let request_id = *request_id;
            session
                .loaded_flow(&self.source().instance_id)
                .ok_or_else(|| denied("request sender is no longer loaded"))?
                .query_with_authority(guard.clone(), move |store| {
                    let sent = store
                        .send_summary(request_id)
                        .ok_or_else(|| denied("request is not an original sender intent"))?;
                    if sent.recipient != target {
                        return Err(denied("request target differs from its original recipient"));
                    }
                    Ok(())
                })
                .await?;
        }
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
                    .allows_scope(self.source(), &binding.target, *action)
            })
            .collect();
        let registered_authority = if binding.actions.contains(&FlowAction::ApprovePermission) {
            let source = session
                .loaded_flow(&self.source().instance_id)
                .ok_or_else(|| denied("source execution owner is unavailable"))?;
            let identity = source
                .context()
                .flow_registry
                .lookup_run(&crate::event::FlowRunId::new(self.lease.run.0))
                .filter(|identity| {
                    identity.instance.as_ref() == Some(self.source())
                        && identity.principal_available()
                        && matches!(
                            identity.execution_state(),
                            crate::flow_authority::FlowExecutionState::Running
                        )
                })
                .ok_or_else(|| {
                    denied("peer approval requires the admitted running execution identity")
                })?;
            Some(identity.effective_authority.clone())
        } else {
            None
        };
        Ok(InvocationWitness {
            source: self.source().clone(),
            lease: Some(self.lease.clone()),
            binding,
            access_cursor: snapshot.cursor,
            source_grants,
            registered_authority,
        })
    }
}

impl PeerOperation {
    pub(super) fn supports_finishing_context(&self) -> bool {
        if let Self::Capture { request } = self {
            return matches!(
                request.kind(),
                super::capture::ReadKind::Context | super::capture::ReadKind::Output
            );
        }
        if let Self::CaptureChunk { read, .. } = self {
            return matches!(
                *read,
                super::capture::ReadKind::Context | super::capture::ReadKind::Output
            );
        }
        matches!(
            self,
            Self::Name { .. }
                | Self::Rename { .. }
                | Self::Output { .. }
                | Self::Result { .. }
                | Self::Context { .. }
                | Self::Planning { .. }
                | Self::Goal { .. }
                | Self::EditGoal { .. }
        )
    }
    pub(super) fn target(&self) -> AccessTarget {
        use PeerOperation::*;
        match self {
            RegisterLaunchResult { request } => AccessTarget::Flow(request.route.recipient.clone()),
            Capture { request } => AccessTarget::Flow(request.flow().clone()),
            CaptureChunk { target, .. }
            | ReleaseCapture { target, .. }
            | InputStatus { target, .. } => AccessTarget::Flow(target.clone()),
            ControlBatch { .. } | ControlStatus { .. } | Sessions { .. } => {
                AccessTarget::AllSessions
            }
            Instances { session, .. } => AccessTarget::Session(*session),
            Name { target }
            | Rename { target, .. }
            | Inspect { target }
            | Observe { target }
            | Output { target }
            | Result { target, .. }
            | Context { target }
            | Planning { target }
            | Goal { target }
            | EditGoal { target, .. }
            | Permissions { target }
            | PermissionReview { target, .. }
            | ResolvePermission { target, .. } => AccessTarget::Flow(target.clone()),
            Deliver { request } => AccessTarget::Flow(request.recipient.clone()),
            Control { request } => AccessTarget::Flow(request.target.clone()),
            Anchor { source, .. } | Events { source, .. } => AccessTarget::Flow(source.clone()),
            OfferPermission { reviewer, .. } | PermissionReviewProgress { reviewer, .. } => {
                AccessTarget::Flow(reviewer.clone())
            }
        }
    }

    pub(super) fn actions(&self) -> Vec<FlowAction> {
        use PeerOperation::*;
        vec![match self {
            RegisterLaunchResult { .. } => return Vec::new(),
            Capture { request } => request.kind().action(),
            CaptureChunk { read, .. } | ReleaseCapture { read, .. } => read.action(),
            InputStatus { .. } => return vec![FlowAction::Send, FlowAction::Discover],
            ControlBatch { .. } | ControlStatus { .. } | Sessions { .. } => return Vec::new(),
            Name { .. } | Inspect { .. } | Observe { .. } | Instances { .. } => {
                FlowAction::Discover
            }
            Rename { .. } => FlowAction::Rename,
            Output { .. } | Result { .. } => FlowAction::ReadOutput,
            Context { .. } | Planning { .. } => FlowAction::ReadContext,
            Goal { .. } => return vec![FlowAction::ReadContext, FlowAction::ManageGoal],
            EditGoal { .. } => FlowAction::ManageGoal,
            Deliver { .. } => FlowAction::Send,
            Control { .. } => FlowAction::Control,
            Anchor { .. } | Events { .. } => FlowAction::Watch,
            Permissions { .. } | PermissionReview { .. } | ResolvePermission { .. } => {
                FlowAction::ApprovePermission
            }
            OfferPermission { .. } | PermissionReviewProgress { .. } => return Vec::new(),
        }]
    }

    pub(super) fn binding(&self, owner: Uuid) -> io::Result<EffectBinding> {
        let encoded = serde_json::to_vec(self).map_err(io::Error::other)?;
        Ok(EffectBinding {
            transaction: Uuid::new_v4(),
            target_owner: owner,
            operation_digest: blake3::hash(&encoded).to_hex().to_string(),
            target: self.target(),
            actions: self.actions(),
            finishing_context: false,
        })
    }
}

pub(super) async fn read_guard(
    host: &RuntimeHost,
    session: Uuid,
) -> io::Result<Arc<crate::peer_authority::AuthorityGuard>> {
    Ok(Arc::new(
        host.peer_authority(session)?
            .acquire(
                AuthorityPlan::new([(AuthorityKey::Session(session), AuthorityMode::Shared)])?,
                tokio_util::sync::CancellationToken::new(),
                Instant::now() + Duration::from_secs(15),
            )
            .await?,
    ))
}
