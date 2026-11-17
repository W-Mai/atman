//! Finite original-owner admission for a Prepared entry's initial result route.

use std::time::{Duration, Instant};

use super::*;
use crate::flow_runtime::{FlowWorker, task_scope::ScopeAdmission};
use crate::flow_store::{
    FlowDescriptor,
    launch::{LaunchInput, ResultRoute},
    subscription::{ObservationAnchor, ResultSubscriptionProof},
};
use crate::peer_authority::{AuthorityGuard, AuthorityKey, AuthorityMode, AuthorityPlan};

pub(in crate::flow_runtime) type LaunchAuthorization =
    Arc<dyn Fn(&FlowDescriptor, &LaunchInput) -> io::Result<()> + Send + Sync>;

/// Exact route data excludes the launch's private inherited context and executable source.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PendingLaunchRequest {
    pub source: FlowRef,
    pub creation_request: Uuid,
    pub creator: Option<FlowRef>,
    pub route: ResultRoute,
    pub anchor: PeerObservationAnchor,
    pub launch_digest: String,
}

pub(super) struct PendingLaunchIdentity {
    principal: PendingLaunchPrincipal,
    binding: EffectBinding,
    descriptor: FlowDescriptor,
    input: LaunchInput,
    authorize: LaunchAuthorization,
    access: Weak<std::sync::RwLock<crate::flow_store::access::SessionAccess>>,
    state: tokio::sync::watch::Receiver<atman_rt::FlowInstance<AtmanUuid>>,
}

pub(in crate::flow_runtime) async fn register(
    host: &RuntimeHost,
    worker: &FlowWorker,
    descriptor: &FlowDescriptor,
    input: &LaunchInput,
    anchor: ObservationAnchor,
    authorize: LaunchAuthorization,
) -> io::Result<ResultSubscriptionProof> {
    let route = input
        .result_route
        .as_ref()
        .ok_or_else(|| protocol("pending launch has no result route"))?;
    let port = host
        .peer_port()
        .ok_or_else(|| protocol("pending launch peer transport is unavailable"))?;
    let owner = if host.session(route.recipient.session_id.0).is_ok() {
        host.peer_identity()?
    } else {
        port.locate(PeerTarget::Session(route.recipient.session_id.0))
            .await?
    };
    if worker.address() != &descriptor.address || anchor.source() != worker.address() {
        return Err(denied("pending launch anchor belongs to another owner"));
    }
    let operation = PeerOperation::RegisterLaunchResult {
        request: PendingLaunchRequest {
            source: descriptor.address.clone(),
            creation_request: descriptor.creation_request,
            creator: descriptor.created_by.clone(),
            route: route.clone(),
            anchor: anchor.into(),
            launch_digest: blake3::hash(
                &serde_json::to_vec(&(descriptor, input)).map_err(io::Error::other)?,
            )
            .to_hex()
            .to_string(),
        },
    };
    let binding = operation.binding(owner.runtime_generation)?;
    let principal = PendingLaunchPrincipal {
        owner: host.generation(),
        nonce: Uuid::new_v4(),
        source: descriptor.address.clone(),
    };
    let session = host.session(descriptor.address.session_id.0)?;
    let identity = Arc::new(PendingLaunchIdentity {
        principal: principal.clone(),
        binding: binding.clone(),
        descriptor: descriptor.clone(),
        input: input.clone(),
        authorize,
        access: Arc::downgrade(&session.access),
        state: worker.subscribe_state(),
    });
    {
        let mut entries = host.invocation_registry().pending_launches.lock().unwrap();
        entries.retain(|_, entry| entry.strong_count() != 0);
        entries.insert(principal.nonce, Arc::downgrade(&identity));
    }
    let caller = host.peer_identity()?;
    let host = host.clone();
    let reply = tokio::spawn(async move {
        let _admission = identity;
        let call = PeerCall::Execute {
            principal: PeerPrincipal::PendingLaunch(principal),
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
        PeerReply::LaunchResult(proof) => Ok(proof.into_proof()),
        _ => Err(protocol("pending launch returned another reply kind")),
    }
}

pub(super) async fn validate(
    host: &RuntimeHost,
    principal: PendingLaunchPrincipal,
    binding: EffectBinding,
    guard: Arc<AuthorityGuard>,
) -> io::Result<InvocationWitness> {
    if principal.owner != host.generation() {
        return Err(denied("pending launch source owner generation changed"));
    }
    let identity = host
        .invocation_registry()
        .pending_launches
        .lock()
        .unwrap()
        .get(&principal.nonce)
        .and_then(Weak::upgrade)
        .ok_or_else(|| denied("original pending launch admission is unavailable"))?;
    let session = host.session(principal.source.session_id.0)?;
    let access = identity
        .access
        .upgrade()
        .ok_or_else(|| denied("original pending launch access owner is unavailable"))?;
    if identity.binding != binding
        || identity.principal.owner != principal.owner
        || identity.principal.source != principal.source
        || identity.descriptor.address != principal.source
        || identity.input.request.request_id != identity.descriptor.creation_request
        || identity.input.request.recipient != principal.source
        || !Arc::ptr_eq(&access, &session.access)
        || identity.state.has_changed().is_err()
        || host.is_transient(&principal.source)
        || binding.finishing_context
    {
        return Err(denied("pending launch differs from its original admission"));
    }
    (identity.authorize)(&identity.descriptor, &identity.input)?;
    let worker = session
        .loaded_flow(&principal.source.instance_id)
        .ok_or_else(|| denied("original Prepared launch owner is unavailable"))?;
    if !worker.subscribe_state().same_channel(&identity.state) {
        return Err(denied(
            "pending launch cannot bind a replacement Prepared owner",
        ));
    }
    let expected = identity.descriptor.clone();
    worker
        .query_with_authority(guard.clone(), move |store| {
            if store.descriptor() != &expected
                || store.state().state() != &atman_rt::InstanceState::Prepared
                || store.launch_record().is_some()
            {
                return Err(denied("original pending entry is no longer Prepared"));
            }
            Ok(())
        })
        .await?;
    let snapshot = session.snapshot_with_authority(guard).await?;
    Ok(InvocationWitness {
        source: principal.source,
        lease: None,
        binding,
        access_cursor: snapshot.cursor,
        source_grants: Vec::new(),
        registered_authority: None,
    })
}

pub(super) async fn execute(
    host: RuntimeHost,
    principal: PeerPrincipal,
    binding: EffectBinding,
    operation: PeerOperation,
) -> io::Result<PeerReply> {
    let PeerPrincipal::PendingLaunch(principal) = principal else {
        return Err(denied(
            "result registration requires its original pending launch admission",
        ));
    };
    let PeerOperation::RegisterLaunchResult { request } = operation else {
        return Err(denied(
            "pending launch cannot perform another operation family",
        ));
    };
    let route = &request.route;
    let anchor = request.anchor.into_anchor();
    if request.source != principal.source
        || request.creation_request.is_nil()
        || request.launch_digest.len() != 64
        || anchor.source() != &request.source
        || binding.target != AccessTarget::Flow(route.recipient.clone())
    {
        return Err(denied(
            "pending launch operation changed its original identities",
        ));
    }
    let peer = PeerPrincipal::PendingLaunch(principal);
    // Opening a cold recipient may recover its journal. Authorize that effect before installing its owner.
    drop(
        registration_authority(
            &host,
            &peer,
            &binding,
            &request.source,
            request.creator.as_ref(),
            route,
        )
        .await?,
    );
    let worker = host.flow(&route.recipient).await?;
    if worker.is_transient() {
        return Err(denied(
            "temporary recipient cannot retain an entry result route",
        ));
    }
    let guard = registration_authority(
        &host,
        &peer,
        &binding,
        &request.source,
        request.creator.as_ref(),
        route,
    )
    .await?;
    let route = route.clone();
    worker
        .request_with_authority(guard, move |store| {
            super::super::launch::register_result_route(store, &request.source, &route, &anchor)
                .map(|proof| PeerReply::LaunchResult(proof.into()))
        })
        .await
}

async fn registration_authority(
    host: &RuntimeHost,
    principal: &PeerPrincipal,
    binding: &EffectBinding,
    source: &FlowRef,
    creator: Option<&FlowRef>,
    route: &ResultRoute,
) -> io::Result<Arc<AuthorityGuard>> {
    let guard = launch_guard(
        host,
        source,
        creator,
        route,
        AuthorityMode::Shared,
        &Default::default(),
    )
    .await?;
    let witness = super::dispatch::validate_source(host, principal, binding, guard.clone()).await?;
    if witness.source != *source || witness.binding != *binding || witness.lease.is_some() {
        return Err(denied(
            "pending launch witness differs from its original admission",
        ));
    }
    authorize_route(host, source, route, guard.clone()).await?;
    Ok(guard)
}

/// Retain both owners' current authorization until the original activation ACK.
pub(in crate::flow_runtime) async fn activation_authority(
    host: &RuntimeHost,
    descriptor: &FlowDescriptor,
    input: &LaunchInput,
    scope: &ScopeAdmission,
) -> io::Result<Option<Arc<AuthorityGuard>>> {
    if host.peer_port().is_none() {
        return Ok(None);
    }
    let Some(route) = &input.result_route else {
        return Ok(None);
    };
    let guard = launch_guard(
        host,
        &descriptor.address,
        descriptor.created_by.as_ref(),
        route,
        AuthorityMode::Exclusive,
        scope,
    )
    .await?;
    authorize_route(host, &descriptor.address, route, guard.clone()).await?;
    Ok(Some(guard))
}

async fn launch_guard(
    host: &RuntimeHost,
    source: &FlowRef,
    creator: Option<&FlowRef>,
    route: &ResultRoute,
    source_mode: AuthorityMode,
    scope: &ScopeAdmission,
) -> io::Result<Arc<AuthorityGuard>> {
    let recipient_mode = if source_mode == AuthorityMode::Exclusive {
        AuthorityMode::Shared
    } else {
        AuthorityMode::Exclusive
    };
    let mut requirements = vec![
        (AuthorityKey::Profile, AuthorityMode::Shared),
        (AuthorityKey::Session(source.session_id.0), source_mode),
        (
            AuthorityKey::Session(route.recipient.session_id.0),
            recipient_mode,
        ),
    ];
    if let Some(parent) = creator {
        requirements.push((
            AuthorityKey::Session(parent.session_id.0),
            AuthorityMode::Shared,
        ));
    }
    requirements.extend(scope.authority_requirements());
    Ok(Arc::new(
        host.authority_domain()
            .ok_or_else(|| protocol("pending launch authority is unavailable"))?
            .acquire(
                AuthorityPlan::new(requirements)?,
                tokio_util::sync::CancellationToken::new(),
                Instant::now() + Duration::from_secs(15),
            )
            .await?,
    ))
}

async fn authorize_route(
    host: &RuntimeHost,
    publisher: &FlowRef,
    route: &ResultRoute,
    guard: Arc<AuthorityGuard>,
) -> io::Result<()> {
    let target = AccessTarget::Flow(publisher.clone());
    let allowed = if let Ok(session) = host.session(route.recipient.session_id.0) {
        let _ = session.snapshot_with_authority(guard).await?;
        session
            .access
            .read()
            .unwrap()
            .allows_scope(&route.recipient, &target, FlowAction::Watch)
    } else {
        let port = host
            .peer_port()
            .ok_or_else(|| protocol("result recipient transport is unavailable"))?;
        let owner = port
            .locate(PeerTarget::Session(route.recipient.session_id.0))
            .await?;
        match port
            .call(
                owner,
                PeerCall::Access {
                    source: route.recipient.clone(),
                    target: target.clone(),
                    action: FlowAction::Watch,
                },
            )
            .await?
        {
            PeerReply::Access { allowed, cursor }
                if cursor.journal_id == route.recipient.session_id.0 =>
            {
                allowed
            }
            _ => {
                return Err(protocol(
                    "result recipient access returned another reply kind",
                ));
            }
        }
    };
    if allowed
        || super::dispatch::profile_allows(host, &route.recipient, &target, FlowAction::Watch)
            .await?
    {
        Ok(())
    } else {
        Err(denied(
            "entry result route is outside current recipient Watch grants",
        ))
    }
}

#[cfg(test)]
mod tests;
