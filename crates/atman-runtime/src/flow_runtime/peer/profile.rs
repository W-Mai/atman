use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use super::*;
use crate::{
    flow_store::{inbox::SenderIdentity, profile::ControlBatchRecord},
    peer_authority::{AuthorityKey, AuthorityMode, AuthorityPlan},
};

/// Flow-owned Profile effects retain the original invocation instead of entering the User facade.
pub(super) async fn execute(
    host: RuntimeHost,
    principal: PeerPrincipal,
    binding: EffectBinding,
    operation: PeerOperation,
) -> io::Result<PeerReply> {
    let PeerPrincipal::Invocation(invocation) = &principal else {
        return Err(denied("Profile admission requires its original invocation"));
    };
    let source = invocation.source.clone();
    let sender = SenderIdentity::Flow(source.clone());
    let profile = host
        .profile_handle()
        .ok_or_else(|| denied("Profile operation reached another owner"))?;
    let mode = match &operation {
        PeerOperation::ControlBatch { request }
            if request.sender == sender
                && request
                    .commands
                    .iter()
                    .all(|command| command.sender == sender) =>
        {
            AuthorityMode::Exclusive
        }
        PeerOperation::ControlStatus {
            source: requested,
            id,
        } if requested == &source && !id.is_nil() => AuthorityMode::Shared,
        PeerOperation::Sessions { .. } => AuthorityMode::Shared,
        _ => {
            return Err(denied(
                "Profile operation actor differs from its original invocation",
            ));
        }
    };
    let guard = source_guard(&host, &source, mode).await?;
    let witness =
        super::dispatch::validate_source(&host, &principal, &binding, guard.clone()).await?;
    if witness.source != source || witness.binding != binding {
        return Err(denied("Profile operation has another invocation witness"));
    }
    match operation {
        PeerOperation::Sessions { before, limit } => {
            drop(guard);
            sessions(&host, &source, before, limit, &principal, &binding)
                .await
                .map(PeerReply::Sessions)
        }
        PeerOperation::ControlBatch { request } => {
            for command in &request.commands {
                authorize_target(&host, &source, &command.target, guard.clone()).await?;
            }
            profile
                .submit_controls_with_authority(guard, request, move |_| {
                    if witness.source == source {
                        Ok(())
                    } else {
                        Err(denied("Profile actor changed"))
                    }
                })
                .await
                .map(PeerReply::ControlBatch)
        }
        PeerOperation::ControlStatus { id, .. } => {
            let record = match profile.published_control(&sender, id) {
                Ok(record) => Some(record),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            };
            if let Some(ControlBatchRecord { items, .. }) = &record {
                for item in items {
                    authorize_target(&host, &source, &item.request.target, guard.clone()).await?;
                }
            }
            Ok(PeerReply::ControlStatus(record))
        }
        _ => Err(protocol("unexpected Profile operation family")),
    }
}

async fn authorize_target(
    host: &RuntimeHost,
    source: &FlowRef,
    target: &FlowRef,
    guard: Arc<crate::peer_authority::AuthorityGuard>,
) -> io::Result<()> {
    let target_scope = AccessTarget::Flow(target.clone());
    let allowed = if let Ok(session) = host.session(source.session_id.0) {
        if host.is_transient(source) {
            return Err(denied("temporary source cannot submit Profile controls"));
        }
        let _ = session.snapshot_with_authority(guard.clone()).await?;
        session
            .access
            .read()
            .unwrap()
            .allows_scope(source, &target_scope, FlowAction::Control)
    } else {
        let port = host
            .peer_port()
            .ok_or_else(|| protocol("source authority is unavailable"))?;
        let owner = port
            .locate(PeerTarget::Session(source.session_id.0))
            .await?;
        match port
            .call(
                owner,
                PeerCall::Access {
                    source: source.clone(),
                    target: target_scope.clone(),
                    action: FlowAction::Control,
                },
            )
            .await?
        {
            PeerReply::Access { cursor, allowed } if cursor.journal_id == source.session_id.0 => {
                allowed
            }
            _ => {
                return Err(protocol(
                    "source access evidence belongs to another Session",
                ));
            }
        }
    };
    if allowed
        || super::dispatch::profile_allows(host, source, &target_scope, FlowAction::Control).await?
    {
        Ok(())
    } else {
        Err(denied("control target is outside current access grants"))
    }
}

async fn sessions(
    host: &RuntimeHost,
    source: &FlowRef,
    before: Option<Uuid>,
    limit: usize,
    principal: &PeerPrincipal,
    binding: &EffectBinding,
) -> io::Result<super::super::catalog::SessionPage> {
    let catalog = super::super::catalog::SessionCatalog::new(
        None,
        Some(host.peer_identity()?.canonical_data_root),
    );
    let ids = catalog.ids().await?;
    let guard = source_guard(host, source, AuthorityMode::Shared).await?;
    let witness = super::dispatch::validate_source(host, principal, binding, guard.clone()).await?;
    if witness.source != *source || witness.binding != *binding {
        return Err(denied(
            "catalog preparation belongs to another original invocation",
        ));
    }
    let visible = visible_sessions(host, source, &ids, guard.clone()).await?;
    drop(guard);
    let port = host
        .peer_port()
        .ok_or_else(|| protocol("Session catalog transport is unavailable"))?;
    // This immutable selection is read outside authority gates and reauthorized before disclosure.
    let mut page = catalog
        .page(before, limit, |id| Ok(visible.contains(&id)))
        .await?;
    for summary in &mut page.sessions {
        let owner = if host.session(summary.session_id).is_ok() {
            Ok(host.peer_identity()?)
        } else {
            port.locate(PeerTarget::Session(summary.session_id)).await
        };
        let Ok(owner) = owner else {
            continue;
        };
        let call = PeerCall::SessionSummary {
            session: summary.session_id,
        };
        let observed = if owner.runtime_generation == host.generation() {
            Box::pin(handle_peer_call(host.clone(), host.peer_identity()?, call)).await
        } else {
            port.call(owner, call).await
        };
        if let Ok(PeerReply::SessionSummary(current)) = observed
            && current.session_id == summary.session_id
        {
            *summary = current;
        }
    }
    let guard = source_guard(host, source, AuthorityMode::Shared).await?;
    let witness = super::dispatch::validate_source(host, principal, binding, guard.clone()).await?;
    if witness.source != *source || witness.binding != *binding {
        return Err(denied(
            "catalog disclosure belongs to another original invocation",
        ));
    }
    let visible_ids: Vec<_> = visible.iter().copied().collect();
    let current = visible_sessions(host, source, &visible_ids, guard.clone()).await?;
    if current.len() != visible.len() {
        return Err(denied("Session catalog discovery authority changed"));
    }
    Ok(page)
}

async fn visible_sessions(
    host: &RuntimeHost,
    source: &FlowRef,
    ids: &[Uuid],
    guard: Arc<crate::peer_authority::AuthorityGuard>,
) -> io::Result<std::collections::HashSet<Uuid>> {
    let port = host
        .peer_port()
        .ok_or_else(|| protocol("Session catalog transport is unavailable"))?;
    let mut visible = std::collections::HashSet::new();
    for batch in ids.chunks(256) {
        let decisions = if let Ok(session) = host.session(source.session_id.0) {
            let _ = session.snapshot_with_authority(guard.clone()).await?;
            let access = session.access.read().unwrap();
            batch
                .iter()
                .map(|id| {
                    access.allows_scope(source, &AccessTarget::Session(*id), FlowAction::Discover)
                })
                .collect::<Vec<_>>()
        } else {
            let owner = port
                .locate(PeerTarget::Session(source.session_id.0))
                .await?;
            match port
                .call(
                    owner,
                    PeerCall::AccessBatch {
                        source: source.clone(),
                        targets: batch
                            .iter()
                            .map(|id| (AccessTarget::Session(*id), FlowAction::Discover))
                            .collect(),
                    },
                )
                .await?
            {
                PeerReply::AccessBatch { cursor, allowed }
                    if cursor.journal_id == source.session_id.0 && allowed.len() == batch.len() =>
                {
                    allowed
                }
                _ => {
                    return Err(protocol(
                        "Session catalog access evidence belongs to another source",
                    ));
                }
            }
        };
        for (&id, local) in batch.iter().zip(decisions) {
            if local
                || super::dispatch::profile_allows(
                    host,
                    source,
                    &AccessTarget::Session(id),
                    FlowAction::Discover,
                )
                .await?
            {
                visible.insert(id);
            }
        }
    }
    Ok(visible)
}

async fn source_guard(
    host: &RuntimeHost,
    source: &FlowRef,
    mode: AuthorityMode,
) -> io::Result<Arc<crate::peer_authority::AuthorityGuard>> {
    Ok(Arc::new(
        host.authority_domain()
            .ok_or_else(|| protocol("Profile authority is unavailable"))?
            .acquire(
                AuthorityPlan::new([
                    (AuthorityKey::Profile, mode),
                    (
                        AuthorityKey::Session(source.session_id.0),
                        AuthorityMode::Shared,
                    ),
                ])?,
                tokio_util::sync::CancellationToken::new(),
                Instant::now() + Duration::from_secs(15),
            )
            .await?,
    ))
}
