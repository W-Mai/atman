use std::{
    num::NonZeroUsize,
    sync::Arc,
    time::{Duration, Instant},
};

use super::*;
use crate::peer_authority::{AuthorityGuard, AuthorityKey, AuthorityMode, AuthorityPlan};

pub(super) struct PreparedPeerEffect {
    pub guard: Arc<AuthorityGuard>,
    pub witness: InvocationWitness,
}

impl PreparedPeerEffect {
    pub(super) fn check(&self, target: &FlowRef) -> io::Result<()> {
        if !self.guard.covers(
            AuthorityKey::Session(target.session_id.0),
            AuthorityMode::Exclusive,
        ) || !matches!(&self.witness.binding.target, AccessTarget::Flow(expected) if expected == target)
        {
            return Err(denied("prepared peer effect belongs to another target"));
        }
        Ok(())
    }
}

pub async fn handle_peer_call(
    host: RuntimeHost,
    caller: PeerIdentity,
    call: PeerCall,
) -> io::Result<PeerReply> {
    let own = host.peer_identity()?;
    if caller.profile_id != own.profile_id || caller.canonical_data_root != own.canonical_data_root
    {
        return Err(denied("internal peer belongs to another authority domain"));
    }
    match call {
        PeerCall::Validate { principal, binding } => {
            if caller.runtime_generation != binding.target_owner {
                return Err(denied(
                    "peer validation caller is not the bound target owner",
                ));
            }
            match principal {
                PeerPrincipal::Invocation(principal) => {
                    let client = host.invocation_registry().client(&host, &principal)?;
                    let guard =
                        super::client::read_guard(&host, principal.source.session_id.0).await?;
                    client
                        .peer_witness(binding, guard)
                        .await
                        .map(PeerReply::Witness)
                }
                PeerPrincipal::Durable(principal) => {
                    super::durable::validate(&host, principal, binding)
                        .await
                        .map(PeerReply::Witness)
                }
                PeerPrincipal::Observation(principal) => {
                    let guard =
                        super::client::read_guard(&host, principal.recipient.session_id.0).await?;
                    super::observation::validate(&host, principal, binding, guard)
                        .await
                        .map(PeerReply::Witness)
                }
                PeerPrincipal::PendingLaunch(principal) => {
                    let guard =
                        super::client::read_guard(&host, principal.source.session_id.0).await?;
                    super::pending_launch::validate(&host, principal, binding, guard)
                        .await
                        .map(PeerReply::Witness)
                }
            }
        }
        PeerCall::Access {
            source,
            target,
            action,
        } => {
            let guard = super::client::read_guard(&host, source.session_id.0).await?;
            let session = host.session(source.session_id.0)?;
            let snapshot = session.snapshot_with_authority(guard.clone()).await?;
            if host.is_transient(&source) {
                return Err(denied("temporary source cannot delegate authority"));
            }
            let allowed = session
                .access
                .read()
                .unwrap()
                .allows_scope(&source, &target, action);
            Ok(PeerReply::Access {
                cursor: snapshot.cursor,
                allowed,
            })
        }
        PeerCall::AccessBatch { source, targets } => {
            if targets.is_empty() || targets.len() > 256 {
                return Err(protocol("invalid source access evidence batch"));
            }
            let guard = super::client::read_guard(&host, source.session_id.0).await?;
            let session = host.session(source.session_id.0)?;
            let snapshot = session.snapshot_with_authority(guard.clone()).await?;
            if host.is_transient(&source) {
                return Err(denied("temporary source cannot delegate authority"));
            }
            let access = session.access.read().unwrap();
            Ok(PeerReply::AccessBatch {
                cursor: snapshot.cursor,
                allowed: targets
                    .iter()
                    .map(|(target, action)| access.allows_scope(&source, target, *action))
                    .collect(),
            })
        }
        PeerCall::SessionSummary { session: id } => {
            let guard = super::client::read_guard(&host, id).await?;
            let session = host.session(id)?;
            let snapshot = session.snapshot_with_authority(guard.clone()).await?;
            let loaded: Vec<_> = snapshot
                .catalog
                .flows
                .iter()
                .filter_map(|flow| session.loaded_flow(&flow.id))
                .filter(|worker| !worker.is_stopped())
                .map(|worker| worker.subscribe_state().borrow().clone())
                .collect();
            Ok(PeerReply::SessionSummary(
                super::super::catalog::SessionSummary::observed(snapshot, &loaded),
            ))
        }
        PeerCall::PrepareScopes { references } => {
            if references.is_empty() || references.len() > 256 {
                return Err(protocol("invalid task scope preparation batch"));
            }
            host.prepare_scope_evidence(&references)
                .await
                .map(PeerReply::Scopes)
        }
        PeerCall::Scopes { references } => {
            if references.is_empty() || references.len() > 256 {
                return Err(protocol("invalid task scope evidence batch"));
            }
            let plan = AuthorityPlan::new(references.iter().map(|scope| {
                (
                    AuthorityKey::Session(scope.owner.session_id.0),
                    AuthorityMode::Shared,
                )
            }))?;
            let authority = host
                .authority_domain()
                .ok_or_else(|| protocol("peer authority is unavailable"))?;
            let guard = authority
                .acquire(
                    plan,
                    tokio_util::sync::CancellationToken::new(),
                    Instant::now() + Duration::from_secs(15),
                )
                .await?;
            host.scope_evidence_with_authority(&guard, &references)
                .map(PeerReply::Scopes)
        }
        PeerCall::Profile(operation) => {
            let _guard = if caller.runtime_generation != host.generation()
                && matches!(
                    &operation,
                    super::super::peer_profile::ProfileOperation::Authorize { .. }
                        | super::super::peer_profile::ProfileOperation::ControlEvidence { .. }
                ) {
                Some(
                    host.authority_domain()
                        .ok_or_else(|| protocol("Profile validation authority is unavailable"))?
                        .acquire(
                            AuthorityPlan::new([(AuthorityKey::Profile, AuthorityMode::Shared)])?,
                            tokio_util::sync::CancellationToken::new(),
                            Instant::now() + Duration::from_secs(15),
                        )
                        .await?,
                )
            } else {
                None
            };
            super::super::peer_profile::dispatch(&host, operation)
                .await
                .map(PeerReply::Profile)
        }
        PeerCall::ProfileDelivery(request) => {
            super::super::peer_profile::deliver(&host, &caller, request)
                .await
                .map(PeerReply::ProfileDelivery)
        }
        PeerCall::Execute {
            principal,
            binding,
            operation,
        } => {
            let owner = match &principal {
                PeerPrincipal::Invocation(value) => value.owner,
                PeerPrincipal::Durable(value) => value.owner,
                PeerPrincipal::Observation(value) => value.owner,
                PeerPrincipal::PendingLaunch(value) => value.owner,
            };
            if caller.runtime_generation != owner {
                return Err(denied(
                    "peer operation was not sent by its original source owner",
                ));
            }
            execute(host, principal, binding, *operation).await
        }
    }
}

async fn execute(
    host: RuntimeHost,
    principal: PeerPrincipal,
    binding: EffectBinding,
    operation: PeerOperation,
) -> io::Result<PeerReply> {
    let source = match &principal {
        PeerPrincipal::Invocation(value) => value.source.clone(),
        PeerPrincipal::Durable(value) => super::durable::source(value)?,
        PeerPrincipal::Observation(value) => value.recipient.clone(),
        PeerPrincipal::PendingLaunch(value) => value.source.clone(),
    };
    if matches!(&principal, PeerPrincipal::PendingLaunch(_))
        != matches!(&operation, PeerOperation::RegisterLaunchResult { .. })
    {
        return Err(denied(
            "prepared launch admission cannot perform another operation family",
        ));
    }
    if matches!(principal, PeerPrincipal::Observation(_))
        && !matches!(operation, PeerOperation::Anchor { .. })
    {
        return Err(denied(
            "native observation admission cannot perform another operation family",
        ));
    }
    let mut expected = operation.binding(host.generation())?;
    expected.finishing_context = matches!(&principal, PeerPrincipal::Invocation(_))
        && matches!(&expected.target, AccessTarget::Flow(target) if target == &source)
        && operation.supports_finishing_context();
    if binding.target_owner != host.generation()
        || binding.target != expected.target
        || binding.actions != expected.actions
        || binding.operation_digest != expected.operation_digest
        || binding.finishing_context != expected.finishing_context
        || binding.transaction.is_nil()
    {
        return Err(denied(
            "peer operation differs from its original effect binding",
        ));
    }
    if matches!(&operation, PeerOperation::RegisterLaunchResult { .. }) {
        return super::pending_launch::execute(host, principal, binding, operation).await;
    }
    if matches!(
        &operation,
        PeerOperation::Events { .. }
            | PeerOperation::CaptureChunk {
                read: super::capture::ReadKind::Events,
                ..
            }
            | PeerOperation::ReleaseCapture {
                read: super::capture::ReadKind::Events,
                ..
            }
    ) {
        return super::capture::events::execute(host, principal, binding, operation).await;
    }
    if matches!(
        operation,
        PeerOperation::Capture { .. }
            | PeerOperation::CaptureChunk { .. }
            | PeerOperation::ReleaseCapture { .. }
    ) {
        return super::capture::execute(host, principal, binding, operation).await;
    }
    if matches!(
        operation,
        PeerOperation::ControlBatch { .. }
            | PeerOperation::ControlStatus { .. }
            | PeerOperation::Sessions { .. }
    ) {
        return super::profile::execute(host, principal, binding, operation).await;
    }
    let target_session = match &binding.target {
        AccessTarget::Flow(flow) => flow.session_id.0,
        AccessTarget::Session(session) => *session,
        AccessTarget::AllSessions => {
            return Err(protocol("peer effect requires one target Session"));
        }
    };
    let session = host.session(target_session)?;
    if let AccessTarget::Flow(target) = &binding.target
        && !matches!(
            operation,
            PeerOperation::Observe { .. } | PeerOperation::InputStatus { .. }
        )
        && session
            .loaded_flow(&target.instance_id)
            .is_none_or(|worker| worker.is_stopped())
        && let Some(reply) =
            preflight_open(&host, &principal, &binding, &operation, &source).await?
    {
        return Ok(reply);
    }
    // Resolve actual workers before acquiring authority; opening an owner under a gate would self-lock.
    let worker = match &binding.target {
        AccessTarget::Flow(flow)
            if matches!(
                operation,
                PeerOperation::Observe { .. } | PeerOperation::InputStatus { .. }
            ) =>
        {
            session.loaded_flow(&flow.instance_id)
        }
        AccessTarget::Flow(flow) => match host.flow(flow).await {
            Ok(worker) => Some(worker),
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    && matches!(operation, PeerOperation::Deliver { .. }) =>
            {
                None
            }
            Err(error) => return Err(error),
        },
        _ => None,
    };
    if worker.as_ref().is_some_and(|worker| worker.is_transient())
        && binding.target != AccessTarget::Flow(source.clone())
    {
        return Err(denied(
            "temporary target cannot receive cross-flow peer operations",
        ));
    }
    let scope = match &operation {
        PeerOperation::Deliver { request } => match &request.scope {
            Some(scope) => host
                .prepare_scope_admission(None, scope.clone())
                .await
                .unwrap_or_default(),
            None => Default::default(),
        },
        PeerOperation::Control { request }
            if matches!(
                request.action,
                crate::flow_store::control::ControlAction::Resume
                    | crate::flow_store::control::ControlAction::ReleasePause
                    | crate::flow_store::control::ControlAction::Recover
            ) =>
        {
            match &worker {
                Some(worker) => worker.scope_admission().await.unwrap_or_default(),
                None => Default::default(),
            }
        }
        _ => Default::default(),
    };
    let mut requirements = vec![
        (AuthorityKey::Profile, AuthorityMode::Shared),
        (
            AuthorityKey::Session(source.session_id.0),
            AuthorityMode::Shared,
        ),
        (
            AuthorityKey::Session(target_session),
            AuthorityMode::Exclusive,
        ),
    ];
    requirements.extend(scope.authority_requirements());
    let plan = AuthorityPlan::new(requirements)?;
    let guard = Arc::new(
        host.peer_authority(target_session)?
            .acquire(
                plan,
                tokio_util::sync::CancellationToken::new(),
                Instant::now() + Duration::from_secs(15),
            )
            .await?,
    );
    let witness = validate_source(&host, &principal, &binding, guard.clone()).await?;
    if witness.source != source || witness.binding != binding {
        return Err(denied("source witness does not match the admitted effect"));
    }
    // Receipt reconciliation authenticates the durable origin, but never requires a renewed grant.
    if let PeerOperation::Deliver { request } = &operation {
        if !matches!(principal, PeerPrincipal::Durable(_))
            || request.sender != crate::flow_store::inbox::SenderIdentity::Flow(source.clone())
        {
            return Err(denied(
                "peer delivery requires its original committed outbox",
            ));
        }
        if let Some(worker) = &worker {
            let previous = request.clone();
            if let Some(accepted) = worker
                .query_with_authority(guard.clone(), move |store| {
                    store.input_acceptance(&previous)
                })
                .await?
            {
                return Ok(PeerReply::Accepted(accepted));
            }
        } else {
            let data = host.peer_identity()?.canonical_data_root;
            let previous = request.clone();
            let retained = guard.clone();
            if let Some(accepted) = crate::read_work::capture(move || {
                let _authority = retained;
                crate::flow_store::RuntimeStore::inspect_deleted_delivery(&data, &previous)
            })
            .await
            .map_err(io::Error::other)??
            {
                return Ok(PeerReply::Accepted(accepted));
            }
        }
    }
    enforce_grants(
        &host,
        &witness,
        matches!(operation, PeerOperation::Goal { .. }),
    )
    .await?;
    let prepared = Arc::new(PreparedPeerEffect {
        guard: guard.clone(),
        witness,
    });
    if let PeerOperation::InputStatus { target, request_id } = &operation {
        let target = target.clone();
        let request_id = *request_id;
        let sender = crate::flow_store::inbox::SenderIdentity::Flow(source.clone());
        let result = match worker {
            Some(worker) => {
                worker
                    .query_with_authority(guard, move |store| {
                        store.observe_input_request(&sender, request_id)
                    })
                    .await?
            }
            None => {
                let data = host.peer_identity()?.canonical_data_root;
                crate::read_work::capture(move || {
                    let _guard = guard;
                    crate::flow_store::RuntimeStore::inspect_session(&data, target.session_id.0)?
                        .inspect_input_request(&target.instance_id, &sender, request_id)
                })
                .await
                .map_err(io::Error::other)??
            }
        };
        return Ok(PeerReply::InputStatus(result));
    }
    match operation {
        PeerOperation::Deliver { request } => {
            let target = request.recipient.clone();
            return host
                .deliver_with_authority(guard, request, move |_| prepared.check(&target))
                .await
                .map(PeerReply::Accepted);
        }
        PeerOperation::Control { request } => {
            if request.sender != crate::flow_store::inbox::SenderIdentity::Flow(source.clone()) {
                return Err(denied("peer control sender differs from its source"));
            }
            return worker
                .ok_or_else(|| protocol("control target is unavailable"))?
                .control_with_authority(guard, request, move |target| prepared.check(target))
                .await
                .map(PeerReply::Controlled);
        }
        PeerOperation::EditGoal { request, .. } => {
            if request.sender != crate::flow_store::inbox::SenderIdentity::Flow(source.clone())
                || request.start_turn
            {
                return Err(denied("peer goal request changed its actor or turn policy"));
            }
            let worker = worker.ok_or_else(|| protocol("goal target is unavailable"))?;
            let target = worker.address().clone();
            return worker
                .edit_goal_with_authority(guard, request, move |_| prepared.check(&target))
                .await
                .map(PeerReply::GoalEdited);
        }
        _ => {}
    }
    if let PeerOperation::Instances { before, limit, .. } = operation {
        let snapshot = session.snapshot_with_authority(guard.clone()).await?;
        let selected = session.clone();
        let data = host.peer_identity()?.canonical_data_root;
        return super::super::access::instance_page(
            snapshot,
            before,
            limit,
            |_| Ok(()),
            move |id| {
                let worker = selected.loaded_flow(&id);
                let guard = guard.clone();
                let data = data.clone();
                async move {
                    match worker {
                        Some(worker) => {
                            worker
                                .request_with_authority(
                                    guard,
                                    super::super::access::InstanceObservation::live,
                                )
                                .await
                        }
                        None => crate::read_work::capture(move || {
                            let _authority = guard;
                            crate::flow_store::RuntimeStore::inspect_session(&data, target_session)?
                                .inspect_flow(&id)
                                .map(super::super::access::InstanceObservation::recorded)
                        })
                        .await
                        .map_err(io::Error::other)?,
                    }
                }
            },
        )
        .await
        .map(PeerReply::Instances);
    }
    if worker.is_none() && matches!(operation, PeerOperation::Observe { .. }) {
        let AccessTarget::Flow(target) = binding.target else {
            return Err(protocol("observation requires a Flow"));
        };
        let data = host.peer_identity()?.canonical_data_root;
        let observation = crate::read_work::capture(move || {
            crate::flow_store::RuntimeStore::inspect_session(&data, target.session_id.0)?
                .inspect_flow(&target.instance_id)
                .map(super::super::access::InstanceObservation::recorded)
        })
        .await
        .map_err(io::Error::other)??;
        return Ok(PeerReply::Observation(observation));
    }
    let worker = worker.ok_or_else(|| protocol("peer effect has no target Flow"))?;
    match operation {
        PeerOperation::Permissions { .. }
        | PeerOperation::PermissionReview { .. }
        | PeerOperation::ResolvePermission { .. }
        | PeerOperation::OfferPermission { .. }
        | PeerOperation::PermissionReviewProgress { .. } => {
            super::permission::execute(&host, worker, prepared, operation).await
        }
        operation => {
            worker
                .request_scoped_with_authority(guard, scope, move |store, _| {
                    prepared.check(store.state().address())?;
                    let actor = crate::flow_store::inbox::SenderIdentity::Flow(
                        prepared.witness.source.clone(),
                    );
                    match operation {
                        PeerOperation::Name { .. } => Ok(PeerReply::Name {
                            title: store.display_title(),
                            snapshot: store.name().clone(),
                        }),
                        PeerOperation::Rename { request, .. } => {
                            if request.sender != actor {
                                return Err(denied("peer name sender differs from its source"));
                            }
                            store.update_name(request).map(PeerReply::Renamed)
                        }
                        PeerOperation::Inspect { .. } => Ok(PeerReply::State {
                            state: store.state().clone(),
                        }),
                        PeerOperation::Observe { .. } => {
                            super::super::access::InstanceObservation::live(store)
                                .map(PeerReply::Observation)
                        }
                        PeerOperation::Output { .. } => {
                            let ids: Vec<_> = store
                                .published_messages()
                                .iter()
                                .filter(|record| record.is_shared_final())
                                .map(|record| record.id)
                                .collect();
                            ids.into_iter()
                                .map(|id| {
                                    store
                                        .published_message(id)?
                                        .ok_or_else(|| protocol("published output disappeared"))
                                })
                                .collect::<io::Result<_>>()
                                .map(PeerReply::Output)
                        }
                        PeerOperation::Result { turn, .. } => {
                            Ok(PeerReply::Result(store.completion(turn.as_ref()).cloned()))
                        }
                        PeerOperation::Context { .. } => {
                            store.context_messages().map(PeerReply::Context)
                        }
                        PeerOperation::Planning { .. } => {
                            Ok(PeerReply::Planning(store.planning().clone()))
                        }
                        PeerOperation::Goal { .. } => {
                            Ok(PeerReply::Goal(super::super::goal::GoalView {
                                goal: store.goal().cloned(),
                                execution: store.state().clone(),
                                history_head: store.history_head(),
                            }))
                        }
                        PeerOperation::Anchor { .. } => {
                            Ok(PeerReply::Anchor(store.observation_anchor().into()))
                        }
                        PeerOperation::Events { after, limit, .. } => store
                            .events_after(
                                after,
                                NonZeroUsize::new(limit.min(64))
                                    .ok_or_else(|| protocol("event limit is zero"))?,
                            )
                            .map(|page| PeerReply::Events(page.into())),
                        _ => Err(protocol(
                            "peer effect was routed to another operation family",
                        )),
                    }
                })
                .await
        }
    }
}

async fn preflight_open(
    host: &RuntimeHost,
    principal: &PeerPrincipal,
    binding: &EffectBinding,
    operation: &PeerOperation,
    source: &FlowRef,
) -> io::Result<Option<PeerReply>> {
    let AccessTarget::Flow(target) = &binding.target else {
        return Err(protocol("owner preparation requires one target Flow"));
    };
    let acquire = || async {
        Ok::<_, io::Error>(Arc::new(
            host.peer_authority(target.session_id.0)?
                .acquire(
                    AuthorityPlan::new([
                        (AuthorityKey::Profile, AuthorityMode::Shared),
                        (
                            AuthorityKey::Session(source.session_id.0),
                            AuthorityMode::Shared,
                        ),
                        (
                            AuthorityKey::Session(target.session_id.0),
                            AuthorityMode::Shared,
                        ),
                    ])?,
                    tokio_util::sync::CancellationToken::new(),
                    Instant::now() + Duration::from_secs(15),
                )
                .await?,
        ))
    };
    let retention = if matches!(operation, PeerOperation::Deliver { .. }) {
        Some(host.session(target.session_id.0)?.retain_source().await?)
    } else {
        None
    };
    let guard = acquire().await?;
    let mut witness = validate_source(host, principal, binding, guard.clone()).await?;
    if witness.source != *source || witness.binding != *binding {
        return Err(denied("cold owner preparation has another source witness"));
    }
    let mut guard = guard;
    if let PeerOperation::Deliver { request } = operation {
        if !matches!(principal, PeerPrincipal::Durable(_))
            || request.sender != crate::flow_store::inbox::SenderIdentity::Flow(source.clone())
        {
            return Err(denied(
                "cold delivery requires its original committed outbox",
            ));
        }
        let data = host.peer_identity()?.canonical_data_root;
        let request = request.clone();
        drop(guard);
        let accepted = crate::read_work::capture(move || {
            crate::flow_store::RuntimeStore::inspect_session(&data, request.recipient.session_id.0)?
                .inspect_delivery(&request)
        })
        .await
        .map_err(io::Error::other)?;
        guard = acquire().await?;
        witness = validate_source(host, principal, binding, guard.clone()).await?;
        if witness.source != *source || witness.binding != *binding {
            return Err(denied(
                "cold delivery reconciliation has another source witness",
            ));
        }
        drop(retention);
        match accepted {
            Ok(Some(accepted)) => return Ok(Some(PeerReply::Accepted(accepted))),
            Ok(None) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    enforce_grants(
        host,
        &witness,
        matches!(operation, PeerOperation::Goal { .. }),
    )
    .await?;
    if let PeerOperation::OfferPermission { reviewer, .. }
    | PeerOperation::PermissionReviewProgress { reviewer, .. } = operation
    {
        super::permission::reviewer_authorized(host, source, reviewer).await?;
    }
    drop(guard);
    Ok(None)
}

async fn enforce_grants(
    host: &RuntimeHost,
    witness: &InvocationWitness,
    either: bool,
) -> io::Result<()> {
    let mut authorized = !either;
    for action in &witness.binding.actions {
        let allowed = witness.source_grants.contains(action)
            || profile_allows(host, &witness.source, &witness.binding.target, *action).await?;
        if either {
            authorized |= allowed;
        } else {
            authorized &= allowed;
        }
    }
    if authorized {
        Ok(())
    } else {
        Err(denied("flow action is outside the current access grants"))
    }
}

pub(super) async fn validate_source(
    host: &RuntimeHost,
    principal: &PeerPrincipal,
    binding: &EffectBinding,
    guard: Arc<AuthorityGuard>,
) -> io::Result<InvocationWitness> {
    let source_owner = match principal {
        PeerPrincipal::Invocation(value) => value.owner,
        PeerPrincipal::Durable(value) => value.owner,
        PeerPrincipal::Observation(value) => value.owner,
        PeerPrincipal::PendingLaunch(value) => value.owner,
    };
    if source_owner == host.generation() {
        match principal {
            PeerPrincipal::Invocation(value) => {
                host.invocation_registry()
                    .client(host, value)?
                    .peer_witness(binding.clone(), guard)
                    .await
            }
            PeerPrincipal::Durable(value) => {
                super::durable::validate_prepared(host, value.clone(), binding.clone(), guard).await
            }
            PeerPrincipal::Observation(value) => {
                super::observation::validate(host, value.clone(), binding.clone(), guard).await
            }
            PeerPrincipal::PendingLaunch(value) => {
                super::pending_launch::validate(host, value.clone(), binding.clone(), guard).await
            }
        }
    } else {
        let port = host
            .peer_port()
            .ok_or_else(|| protocol("internal peer port is unavailable"))?;
        let owner = port.locate(PeerTarget::Runtime(source_owner)).await?;
        match port
            .call(
                owner,
                PeerCall::Validate {
                    principal: principal.clone(),
                    binding: binding.clone(),
                },
            )
            .await?
        {
            PeerReply::Witness(witness) => Ok(witness),
            _ => Err(protocol("peer validation returned another reply kind")),
        }
    }
}

pub(super) async fn profile_allows(
    host: &RuntimeHost,
    source: &FlowRef,
    target: &AccessTarget,
    action: FlowAction,
) -> io::Result<bool> {
    let operation = super::super::peer_profile::ProfileOperation::Authorize {
        source: source.clone(),
        target: target.clone(),
        action,
    };
    let reply = if host.profile_handle().is_some() {
        super::super::peer_profile::dispatch(host, operation).await?
    } else {
        let port = host
            .peer_port()
            .ok_or_else(|| protocol("Profile authority is unavailable"))?;
        let owner = port.locate(PeerTarget::Profile).await?;
        match port.call(owner, PeerCall::Profile(operation)).await? {
            PeerReply::Profile(reply) => reply,
            _ => {
                return Err(protocol(
                    "Profile authorization returned another reply kind",
                ));
            }
        }
    };
    match reply {
        super::super::peer_profile::ProfileReply::Authorization {
            profile_id,
            allowed,
            ..
        } if Some(profile_id)
            == host
                .authority_domain()
                .map(|authority| authority.profile_id()) =>
        {
            Ok(allowed)
        }
        _ => Err(denied(
            "Profile authorization belongs to another authority owner",
        )),
    }
}
