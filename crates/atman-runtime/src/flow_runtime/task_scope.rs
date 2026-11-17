//! Explicit membership validation and cancellation through the shared close lifecycle.

use std::{
    collections::{BTreeMap, HashMap},
    io,
    sync::{RwLock, RwLockReadGuard, RwLockWriteGuard},
};

use atman_rt::InstanceState;
use uuid::Uuid;

use super::{FlowWorker, host::RuntimeHost};
use crate::flow_store::position::FlowPosition;
use crate::flow_store::{
    FlowRef, FlowStore, FlowTransition, invalid,
    task_scope::{ScopeCancellation, TaskScopeDefinition, TaskScopeRecord, TaskScopeRef},
};

mod message;
pub(super) use message::{MessageScopes, ScopeChanges};
#[cfg(test)]
mod tests;

/// Updated under the writer's gate before acknowledging any storage command.
pub(super) struct ScopeState {
    open: bool,
    records: BTreeMap<Uuid, Option<ScopeCancellation>>,
    ancestry: Option<TaskScopeRef>,
    cursor: FlowPosition,
}

pub(super) struct ColdScope {
    state: ScopeState,
    epoch: u64,
    creation: Uuid,
    worker: Option<tokio::sync::mpsc::WeakSender<super::Command>>,
}

pub(super) type ColdScopeCache =
    std::sync::Arc<RwLock<HashMap<crate::flow_store::FlowInstanceId, ColdScope>>>;

impl ColdScope {
    fn matches(
        &self,
        session: &super::host::SessionHandle,
        id: &crate::flow_store::FlowInstanceId,
    ) -> bool {
        if self.epoch != session.scope_epoch()
            || session.scope_registration(id) != Some(self.creation)
        {
            return false;
        }
        match (&self.worker, session.scope_worker(id)) {
            (None, None) => true,
            (Some(expected), Some(worker)) => {
                worker.is_stopped()
                    && worker.failure.get().is_none()
                    && expected
                        .upgrade()
                        .is_some_and(|sender| sender.same_channel(&worker.commands))
                    && self.state.cursor == *worker.commits.borrow()
            }
            _ => false,
        }
    }
}

impl ScopeState {
    fn evidence(&self, reference: &TaskScopeRef) -> io::Result<super::peer::ScopeEvidence> {
        let cancellation = self
            .records
            .get(&reference.id)
            .cloned()
            .ok_or_else(|| invalid("scope is not registered by its original owner"))?;
        Ok(super::peer::ScopeEvidence {
            reference: reference.clone(),
            open: self.open,
            cancellation,
            ancestors: self.ancestry.iter().cloned().collect(),
            cursor: self.cursor.clone(),
        })
    }

    pub fn from_store(store: &FlowStore) -> Self {
        Self {
            open: matches!(store.state().state(), InstanceState::Open(_)),
            records: store
                .task_scopes()
                .map(|record| (record.created.definition.id, record.cancellation.clone()))
                .collect(),
            ancestry: store.task_scope_binding().cloned(),
            cursor: store.cursor().clone(),
        }
    }
}

/// Sources are resolved before queueing a transaction; ancestry remains immutable after activation.
#[derive(Clone, Default)]
pub(crate) struct ScopeAdmission {
    local: Vec<(TaskScopeRef, FlowWorker)>,
    remote: Vec<RemoteScope>,
    member: Option<TaskScopeRef>,
}

#[derive(Clone)]
struct RemoteScope {
    host: super::host::WeakRuntimeHost,
    evidence: super::peer::ScopeEvidence,
}

async fn sample_remote_scope(
    host: &RuntimeHost,
    reference: TaskScopeRef,
    prepare: bool,
) -> io::Result<super::peer::ScopeEvidence> {
    reference.validate()?;
    let port = host
        .peer_port()
        .ok_or_else(|| invalid("task scope owner is unavailable"))?;
    let owner = port
        .locate(super::peer::PeerTarget::Session(
            reference.owner.session_id.0,
        ))
        .await?;
    if owner.runtime_generation == host.generation() {
        return Err(invalid("scope owner was not prepared before sampling"));
    }
    let reply = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        port.call(
            owner,
            if prepare {
                super::peer::PeerCall::PrepareScopes {
                    references: vec![reference.clone()],
                }
            } else {
                super::peer::PeerCall::Scopes {
                    references: vec![reference.clone()],
                }
            },
        ),
    )
    .await
    .map_err(io::Error::other)??;
    match reply {
        super::peer::PeerReply::Scopes(mut evidence)
            if evidence.len() == 1 && evidence[0].reference == reference =>
        {
            Ok(evidence.remove(0))
        }
        _ => Err(invalid("task scope owner returned mismatched evidence")),
    }
}

fn watch_remote_scope(
    host: &RuntimeHost,
    initial: super::peer::ScopeEvidence,
) -> tokio::sync::watch::Receiver<FlowPosition> {
    let host = host.downgrade();
    let (updates, observed) = tokio::sync::watch::channel(initial.cursor.clone());
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = updates.closed() => return,
                _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {}
            }
            let Ok(host) = host.upgrade() else {
                return;
            };
            match sample_remote_scope(&host, initial.reference.clone(), false).await {
                Ok(evidence) => {
                    updates.send_if_modified(|cursor| {
                        if *cursor == evidence.cursor {
                            return false;
                        }
                        *cursor = evidence.cursor;
                        true
                    });
                }
                Err(_) => {
                    updates.send_modify(|_| {});
                }
            }
        }
    });
    observed
}

pub(super) struct ScopeStatus {
    open: bool,
    cancellation: Option<ScopeCancellation>,
}

#[derive(Default)]
pub(crate) struct ScopeSnapshot(HashMap<TaskScopeRef, Result<ScopeStatus, String>>);

impl ScopeSnapshot {
    fn status(&self, scope: &TaskScopeRef) -> io::Result<&ScopeStatus> {
        self.0
            .get(scope)
            .ok_or_else(|| invalid("task scope was not included in the transaction"))?
            .as_ref()
            .map_err(|error| invalid(error.clone()))
    }
}

enum ScopeGuard<'a> {
    Read(RwLockReadGuard<'a, ScopeState>),
    Write(RwLockWriteGuard<'a, ScopeState>),
    Unavailable,
}

impl ScopeGuard<'_> {
    fn state(&self) -> Option<&ScopeState> {
        match self {
            Self::Read(state) => Some(state),
            Self::Write(state) => Some(state),
            Self::Unavailable => None,
        }
    }
}

/// One target write gate and deduplicated source read gates, always in full-address order.
pub(super) struct ScopeTransaction<'a>(Vec<(&'a FlowRef, ScopeGuard<'a>)>);

impl<'a> ScopeTransaction<'a> {
    pub fn acquire(
        sources: &'a ScopeAdmission,
        owner: &'a FlowRef,
        writer: &'a RwLock<ScopeState>,
    ) -> io::Result<Self> {
        let mut gates = vec![(owner, writer, true)];
        gates.extend(
            sources
                .local
                .iter()
                .map(|(_, source)| (source.address(), source.scope_state.as_ref(), false)),
        );
        gates.sort_by_key(|(address, _, write)| {
            (address.session_id.0, address.instance_id.0.0, !write)
        });
        for pair in gates.windows(2) {
            if pair[0].0 == pair[1].0 && !std::ptr::eq(pair[0].1, pair[1].1) {
                return Err(invalid(
                    "task scope writer changed during transaction preparation",
                ));
            }
        }
        gates.dedup_by(|right, left| right.0 == left.0);
        let guards = gates
            .into_iter()
            .map(|(address, state, write)| {
                let guard = if write {
                    ScopeGuard::Write(
                        state
                            .write()
                            .map_err(|_| invalid("task scope owner failed"))?,
                    )
                } else {
                    state
                        .read()
                        .map(ScopeGuard::Read)
                        .unwrap_or(ScopeGuard::Unavailable)
                };
                Ok((address, guard))
            })
            .collect::<io::Result<_>>()?;
        Ok(Self(guards))
    }

    pub fn snapshot(&self, sources: &ScopeAdmission) -> ScopeSnapshot {
        ScopeSnapshot(
            sources
                .local
                .iter()
                .map(|(scope, worker)| {
                    let status = if worker.is_stopping() || worker.is_stopped() {
                        Err("task scope owner is unavailable".into())
                    } else {
                        self.0
                            .iter()
                            .find(|(address, _)| **address == scope.owner)
                            .map(|(_, guard)| {
                                let state = guard
                                    .state()
                                    .ok_or_else(|| "task scope owner failed".to_owned())?;
                                state
                                    .records
                                    .get(&scope.id)
                                    .map(|cancellation| ScopeStatus {
                                        open: state.open,
                                        cancellation: cancellation.clone(),
                                    })
                                    .ok_or_else(|| {
                                        "task scope is not registered by its owner".to_owned()
                                    })
                            })
                            .unwrap_or_else(|| {
                                Err("task scope was not included in the transaction".into())
                            })
                    };
                    (scope.clone(), status)
                })
                .chain(sources.remote.iter().map(|source| {
                    (
                        source.evidence.reference.clone(),
                        Ok(ScopeStatus {
                            open: source.evidence.open,
                            cancellation: source.evidence.cancellation.clone(),
                        }),
                    )
                }))
                .collect(),
        )
    }

    pub fn publish(&mut self, store: &FlowStore) {
        for (_, guard) in &mut self.0 {
            if let ScopeGuard::Write(state) = guard {
                **state = ScopeState::from_store(store);
                return;
            }
        }
        unreachable!("a scope transaction always owns one target writer")
    }
}

impl ScopeAdmission {
    #[cfg(test)]
    fn local(local: Vec<(TaskScopeRef, FlowWorker)>) -> Self {
        Self {
            local,
            ..Self::default()
        }
    }

    pub(crate) async fn prepare_with_authority(
        &mut self,
        guard: std::sync::Arc<crate::peer_authority::AuthorityGuard>,
    ) -> io::Result<()> {
        if self.local.iter().any(|(_, worker)| worker.is_stopped()) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "task scope owner changed; prepare the original dependency again",
            ));
        }
        for source in &mut self.remote {
            let host = source.host.upgrade()?;
            let domain = host.peer_authority(source.evidence.reference.owner.session_id.0)?;
            if !guard.belongs_to(&domain)
                || !guard.covers(
                    crate::peer_authority::AuthorityKey::Session(
                        source.evidence.reference.owner.session_id.0,
                    ),
                    crate::peer_authority::AuthorityMode::Shared,
                )
            {
                return Err(invalid(
                    "scope evidence is outside the prepared authority transaction",
                ));
            }
            let current =
                sample_remote_scope(&host, source.evidence.reference.clone(), false).await?;
            if current.ancestors != source.evidence.ancestors {
                return Err(invalid(
                    "task scope ancestry changed during transaction preparation",
                ));
            }
            source.evidence = current;
        }
        Ok(())
    }
    pub(crate) fn authority_requirements(
        &self,
    ) -> Vec<(
        crate::peer_authority::AuthorityKey,
        crate::peer_authority::AuthorityMode,
    )> {
        self.local
            .iter()
            .map(|(scope, _)| {
                (
                    crate::peer_authority::AuthorityKey::Session(scope.owner.session_id.0),
                    crate::peer_authority::AuthorityMode::Shared,
                )
            })
            .chain(self.remote.iter().map(|source| {
                (
                    crate::peer_authority::AuthorityKey::Session(
                        source.evidence.reference.owner.session_id.0,
                    ),
                    crate::peer_authority::AuthorityMode::Shared,
                )
            }))
            .collect()
    }

    pub fn extend(&mut self, other: &Self) {
        self.local.extend(other.local.iter().cloned());
        self.remote.extend(other.remote.iter().cloned());
    }
    pub fn validate_member(&self, store: &FlowStore) -> io::Result<()> {
        if store.task_scope_binding()
            != self
                .member
                .as_ref()
                .or_else(|| self.local.first().map(|(scope, _)| scope))
        {
            return Err(invalid("task scope membership changed during admission"));
        }
        Ok(())
    }
    fn is_active(&self, snapshot: &ScopeSnapshot) -> io::Result<bool> {
        let mut active = true;
        let mut unavailable = None;
        for scope in self
            .local
            .iter()
            .map(|(scope, _)| scope)
            .chain(self.remote.iter().map(|source| &source.evidence.reference))
        {
            match snapshot.status(scope) {
                Ok(status) => active &= status.open && status.cancellation.is_none(),
                Err(error) => unavailable = Some(error),
            }
        }
        if active && let Some(error) = unavailable {
            return Err(error);
        }
        Ok(active)
    }

    pub fn check(&self, snapshot: &ScopeSnapshot) -> io::Result<()> {
        if !self.is_active(snapshot)? {
            return Err(invalid("task scope is cancelled or its owner is closed"));
        }
        Ok(())
    }
}

impl FlowWorker {
    pub(crate) async fn scope_admission_with_authority(
        &self,
        authority: std::sync::Arc<crate::peer_authority::AuthorityGuard>,
    ) -> io::Result<ScopeAdmission> {
        let binding = self
            .query_with_authority(authority.clone(), |store| {
                Ok(store.task_scope_binding().cloned())
            })
            .await?;
        match binding {
            None => Ok(ScopeAdmission::default()),
            Some(scope) => {
                self.host
                    .as_ref()
                    .ok_or_else(|| invalid("task scope requires its original runtime"))?
                    .upgrade()?
                    .prepare_scope_admission_with_authority(Some(self.address()), scope, authority)
                    .await
            }
        }
    }
    pub async fn create_task_scope(
        &self,
        definition: TaskScopeDefinition,
        authorize: impl FnOnce(&FlowRef) -> io::Result<()> + Send + 'static,
    ) -> io::Result<TaskScopeRecord> {
        self.request(move |store| {
            authorize(store.state().address())?;
            store.create_task_scope(definition)
        })
        .await
    }

    pub async fn cancel_task_scope(
        &self,
        id: Uuid,
        authorize: impl FnOnce(&FlowRef) -> io::Result<()> + Send + 'static,
    ) -> io::Result<FlowPosition> {
        self.request(move |store| {
            authorize(store.state().address())?;
            store.cancel_task_scope(id)
        })
        .await
    }

    pub async fn task_scopes(
        &self,
        authorize: impl FnOnce(&FlowRef) -> io::Result<()> + Send + 'static,
    ) -> io::Result<Vec<TaskScopeRecord>> {
        self.request(move |store| {
            authorize(store.state().address())?;
            Ok(store.task_scopes().cloned().collect())
        })
        .await
    }

    pub(super) async fn scope_admission(&self) -> io::Result<ScopeAdmission> {
        self.resolve_scope(None).await
    }

    pub(super) async fn scope_observation(
        &self,
    ) -> (
        io::Result<ScopeAdmission>,
        Vec<tokio::sync::watch::Receiver<FlowPosition>>,
    ) {
        let mut changes = Vec::new();
        let result = self.resolve_scope(Some(&mut changes)).await;
        (result, changes)
    }

    async fn resolve_scope(
        &self,
        changes: Option<&mut Vec<tokio::sync::watch::Receiver<FlowPosition>>>,
    ) -> io::Result<ScopeAdmission> {
        let scope = self
            .request(|store| Ok(store.task_scope_binding().cloned()))
            .await?;
        match scope {
            None => Ok(ScopeAdmission::default()),
            Some(scope) => {
                self.host
                    .as_ref()
                    .ok_or_else(|| invalid("task scope membership requires its runtime host"))?
                    .upgrade()?
                    .resolve_task_scope(Some(self.address()), scope, changes)
                    .await
            }
        }
    }

    pub(super) async fn observe_task_scope(&self, admission: ScopeAdmission) -> io::Result<()> {
        if admission.local.is_empty() && admission.remote.is_empty() {
            return Ok(());
        }
        let worker = self.clone();
        self.request_scoped(admission.clone(), move |store, snapshot| {
            if !admission.is_active(snapshot)?
                && !matches!(
                    store.state().state(),
                    InstanceState::Closing { .. } | InstanceState::Closed
                )
            {
                store.transition(store.state().revision(), FlowTransition::CloseRequested)?;
                worker.continue_close();
            }
            Ok(())
        })
        .await
    }
}

impl RuntimeHost {
    /// Prepare immutable cold scope metadata without opening a Flow writer or running recovery.
    pub(crate) async fn prepare_scope_evidence(
        &self,
        references: &[TaskScopeRef],
    ) -> io::Result<Vec<super::peer::ScopeEvidence>> {
        let mut prepared = Vec::with_capacity(references.len());
        for reference in references {
            reference.validate()?;
            let session = self.session(reference.owner.session_id.0)?;
            let _pin = session.retain_operation().await?;
            let _source = session.retain_source().await?;
            let domain = self.peer_authority(session.id())?;
            let plan = crate::peer_authority::AuthorityPlan::new([(
                crate::peer_authority::AuthorityKey::Session(session.id()),
                crate::peer_authority::AuthorityMode::Shared,
            )])?;
            if session
                .scope_worker(&reference.owner.instance_id)
                .is_some_and(|worker| !worker.is_stopped())
            {
                let guard = domain
                    .acquire(
                        plan,
                        tokio_util::sync::CancellationToken::new(),
                        std::time::Instant::now() + std::time::Duration::from_secs(15),
                    )
                    .await?;
                prepared.extend(
                    self.scope_evidence_with_authority(&guard, std::slice::from_ref(reference))?,
                );
                continue;
            }
            let cached = session.cold_scope_cache();
            let available = cached
                .read()
                .unwrap()
                .get(&reference.owner.instance_id)
                .is_some_and(|cached| cached.matches(&session, &reference.owner.instance_id));
            if !available {
                let epoch = session.scope_epoch();
                let worker = session
                    .scope_worker(&reference.owner.instance_id)
                    .map(|worker| worker.commands.downgrade());
                let data = domain.data_root().to_owned();
                let captured_reference = reference.clone();
                let loaded = crate::read_work::capture(move || {
                    let inspected = crate::flow_store::RuntimeStore::inspect_session(
                        &data,
                        captured_reference.owner.session_id.0,
                    )?
                    .inspect_flow(&captured_reference.owner.instance_id)?;
                    if inspected.journal.trailing_bytes != 0 {
                        return Err(invalid("scope journal has incomplete trailing data"));
                    }
                    Ok::<_, io::Error>(ColdScope {
                        state: ScopeState {
                            open: matches!(inspected.state.state(), InstanceState::Open(_)),
                            records: inspected
                                .task_scopes
                                .iter()
                                .map(|record| {
                                    (record.created.definition.id, record.cancellation.clone())
                                })
                                .collect(),
                            ancestry: inspected.task_scope_binding,
                            cursor: inspected
                                .journal
                                .cursor
                                .ok_or_else(|| invalid("scope journal cursor is unavailable"))?
                                .into(),
                        },
                        epoch,
                        creation: inspected.descriptor.creation_request,
                        worker,
                    })
                })
                .await
                .map_err(io::Error::other)??;
                let guard = domain
                    .acquire(
                        plan,
                        tokio_util::sync::CancellationToken::new(),
                        std::time::Instant::now() + std::time::Duration::from_secs(15),
                    )
                    .await?;
                if session
                    .scope_worker(&reference.owner.instance_id)
                    .is_some_and(|worker| !worker.is_stopped())
                {
                    prepared.extend(
                        self.scope_evidence_with_authority(
                            &guard,
                            std::slice::from_ref(reference),
                        )?,
                    );
                    continue;
                }
                if !loaded.matches(&session, &reference.owner.instance_id) {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "scope owner changed during metadata preparation",
                    ));
                }
                cached
                    .write()
                    .unwrap()
                    .insert(reference.owner.instance_id.clone(), loaded);
                prepared.extend(
                    self.scope_evidence_with_authority(&guard, std::slice::from_ref(reference))?,
                );
            } else {
                let guard = domain
                    .acquire(
                        plan,
                        tokio_util::sync::CancellationToken::new(),
                        std::time::Instant::now() + std::time::Duration::from_secs(15),
                    )
                    .await?;
                prepared.extend(
                    self.scope_evidence_with_authority(&guard, std::slice::from_ref(reference))?,
                );
            }
        }
        Ok(prepared)
    }

    pub(crate) async fn prepare_scope_admission_with_authority(
        &self,
        member: Option<&FlowRef>,
        scope: TaskScopeRef,
        authority: std::sync::Arc<crate::peer_authority::AuthorityGuard>,
    ) -> io::Result<ScopeAdmission> {
        let domain = self.peer_authority(scope.owner.session_id.0)?;
        if !authority.belongs_to(&domain) {
            return Err(invalid("scope authority domain changed"));
        }
        let mut admission = ScopeAdmission {
            member: Some(scope.clone()),
            ..Default::default()
        };
        let mut seen = std::collections::HashSet::new();
        let mut next = Some(scope);
        while let Some(reference) = next {
            if Some(&reference.owner) == member
                || !seen.insert(reference.owner.clone())
                || seen.len() > 256
            {
                return Err(invalid("task scope membership contains a cycle"));
            }
            if !authority.covers(
                crate::peer_authority::AuthorityKey::Session(reference.owner.session_id.0),
                crate::peer_authority::AuthorityMode::Shared,
            ) {
                return Err(invalid(
                    "scope dependency is missing from the prepared authority manifest",
                ));
            }
            match self.session(reference.owner.session_id.0) {
                Ok(session) => {
                    let worker = session
                        .loaded_flow(&reference.owner.instance_id)
                        .ok_or_else(|| {
                            invalid("scope owner was not prepared before authority acquisition")
                        })?;
                    let state = worker
                        .scope_state
                        .read()
                        .map_err(|_| invalid("scope owner failed"))?;
                    let cancellation = state
                        .records
                        .get(&reference.id)
                        .ok_or_else(|| invalid("scope is not registered by its owner"))?;
                    next = if state.open && cancellation.is_none() {
                        state.ancestry.clone()
                    } else {
                        None
                    };
                    drop(state);
                    admission.local.push((reference, worker));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    let evidence = sample_remote_scope(self, reference, false).await?;
                    if evidence.ancestors.len() > 1 {
                        return Err(invalid("scope owner returned ambiguous direct ancestry"));
                    }
                    next = if evidence.open && evidence.cancellation.is_none() {
                        evidence.ancestors.first().cloned()
                    } else {
                        None
                    };
                    admission.remote.push(RemoteScope {
                        host: self.downgrade(),
                        evidence,
                    });
                }
                Err(error) => return Err(error),
            }
        }
        Ok(admission)
    }

    pub(crate) async fn prepare_scope_admission(
        &self,
        member: Option<&FlowRef>,
        scope: TaskScopeRef,
    ) -> io::Result<ScopeAdmission> {
        self.resolve_task_scope(member, scope, None).await
    }
    /// Sample only already prepared original owners while the executor retains their compatible gates.
    pub(crate) fn scope_evidence_with_authority(
        &self,
        authority: &crate::peer_authority::AuthorityGuard,
        references: &[TaskScopeRef],
    ) -> io::Result<Vec<super::peer::ScopeEvidence>> {
        references
            .iter()
            .map(|reference| {
                let session = self.session(reference.owner.session_id.0)?;
                session.admission().check_read_authority(authority)?;
                if let Some(worker) = session.loaded_flow(&reference.owner.instance_id) {
                    if worker.is_stopping() {
                        return Err(invalid("scope owner is unavailable during retirement"));
                    }
                    return worker
                        .scope_state
                        .read()
                        .map_err(|_| invalid("scope owner failed"))?
                        .evidence(reference);
                }
                let cached = session.cold_scope_cache();
                let cached = cached.read().unwrap();
                let cached = cached
                    .get(&reference.owner.instance_id)
                    .filter(|cached| cached.matches(&session, &reference.owner.instance_id))
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "scope metadata must be prepared before authority sampling",
                        )
                    })?;
                cached.state.evidence(reference)
            })
            .collect()
    }

    async fn resolve_task_scope(
        &self,
        member: Option<&FlowRef>,
        scope: TaskScopeRef,
        mut changes: Option<&mut Vec<tokio::sync::watch::Receiver<FlowPosition>>>,
    ) -> io::Result<ScopeAdmission> {
        let mut admission = ScopeAdmission {
            member: Some(scope.clone()),
            ..Default::default()
        };
        let mut seen = std::collections::HashSet::new();
        let mut next = Some(scope);
        while let Some(scope) = next {
            if Some(&scope.owner) == member || !seen.insert(scope.owner.clone()) || seen.len() > 256
            {
                return Err(invalid("task scope membership contains a cycle"));
            }
            match self.session(scope.owner.session_id.0) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    let evidence = sample_remote_scope(self, scope.clone(), true).await?;
                    if evidence.ancestors.len() > 1 {
                        return Err(invalid("scope owner returned ambiguous direct ancestry"));
                    }
                    if let Some(changes) = changes.as_mut() {
                        changes.push(watch_remote_scope(self, evidence.clone()));
                    }
                    next = if evidence.open && evidence.cancellation.is_none() {
                        evidence.ancestors.first().cloned()
                    } else {
                        None
                    };
                    admission.remote.push(RemoteScope {
                        host: self.downgrade(),
                        evidence,
                    });
                    continue;
                }
                Err(error) => return Err(error),
            }
            let worker = self.flow(&scope.owner).await?;
            if let Some(changes) = changes.as_mut() {
                let mut receiver = worker.subscribe_commits();
                receiver.borrow_and_update();
                changes.push(receiver);
            }
            let id = scope.id;
            next = worker
                .request(move |store| {
                    // A registered group proves that its owner has already activated,
                    // so its ancestry cannot change after this read.
                    let record = store
                        .task_scopes()
                        .find(|record| record.created.definition.id == id)
                        .ok_or_else(|| invalid("task scope is not registered by its owner"))?;
                    if record.cancellation.is_some()
                        || !matches!(store.state().state(), InstanceState::Open(_))
                    {
                        return Ok(None);
                    }
                    Ok(store.task_scope_binding().cloned())
                })
                .await?;
            admission.local.push((scope, worker));
        }
        Ok(admission)
    }

    /// Authenticate both endpoints before binding a Prepared member. Source validity is held through the member commit; identical retries retain their original receipt.
    pub async fn bind_task_scope(
        &self,
        member: FlowRef,
        scope: TaskScopeRef,
        authorize: impl Fn(&FlowRef, &TaskScopeRef) -> io::Result<()> + Send + Sync + 'static,
    ) -> io::Result<FlowPosition> {
        authorize(&member, &scope)?;
        let worker = self.flow(&member).await?;
        let admission = self
            .resolve_task_scope(Some(&member), scope.clone(), None)
            .await?;
        worker
            .request_scoped(admission.clone(), move |store, snapshot| {
                authorize(store.state().address(), &scope)?;
                // Retried binding cannot revive a cancelled group or alter membership.
                if store.task_scope_binding() == Some(&scope) {
                    return store.bind_task_scope(scope);
                }
                admission.check(snapshot)?;
                store.bind_task_scope(scope)
            })
            .await
    }
}
