//! Explicit membership validation and cancellation through the shared close lifecycle.

use std::{
    collections::{BTreeMap, HashMap},
    io,
    sync::{RwLock, RwLockReadGuard, RwLockWriteGuard},
};

use atman_rt::InstanceState;
use uuid::Uuid;

use super::{FlowWorker, host::RuntimeHost};
use crate::event_log::journal::CommitReceipt;
use crate::flow_store::{
    FlowRef, FlowStore, FlowTransition, invalid,
    task_scope::{ScopeCancellation, TaskScopeDefinition, TaskScopeRecord, TaskScopeRef},
};

#[cfg(test)]
mod tests;

/// Updated under the writer's gate before acknowledging any storage command.
pub(super) struct ScopeState {
    open: bool,
    records: BTreeMap<Uuid, Option<ScopeCancellation>>,
}

impl ScopeState {
    pub fn from_store(store: &FlowStore) -> Self {
        Self {
            open: matches!(store.state().state(), InstanceState::Open(_)),
            records: store
                .task_scopes()
                .map(|record| (record.created.definition.id, record.cancellation.clone()))
                .collect(),
        }
    }
}

/// Sources are resolved before queueing a transaction; ancestry remains immutable after activation.
#[derive(Clone, Default)]
pub(super) struct ScopeAdmission(Vec<(TaskScopeRef, FlowWorker)>);

pub(super) struct ScopeStatus {
    open: bool,
    cancellation: Option<ScopeCancellation>,
}

#[derive(Default)]
pub(super) struct ScopeSnapshot(HashMap<TaskScopeRef, Result<ScopeStatus, String>>);

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
}

impl ScopeGuard<'_> {
    fn state(&self) -> &ScopeState {
        match self {
            Self::Read(state) => state,
            Self::Write(state) => state,
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
                .0
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
                    ScopeGuard::Read(
                        state
                            .read()
                            .map_err(|_| invalid("task scope owner failed"))?,
                    )
                };
                Ok((address, guard))
            })
            .collect::<io::Result<_>>()?;
        Ok(Self(guards))
    }

    pub fn snapshot(&self, sources: &ScopeAdmission) -> ScopeSnapshot {
        ScopeSnapshot(
            sources
                .0
                .iter()
                .map(|(scope, worker)| {
                    let status = if worker.is_stopping() || worker.is_stopped() {
                        Err("task scope owner is unavailable".into())
                    } else {
                        self.0
                            .iter()
                            .find(|(address, _)| **address == scope.owner)
                            .and_then(|(_, guard)| {
                                let state = guard.state();
                                state
                                    .records
                                    .get(&scope.id)
                                    .map(|cancellation| ScopeStatus {
                                        open: state.open,
                                        cancellation: cancellation.clone(),
                                    })
                            })
                            .ok_or_else(|| "task scope is not registered by its owner".into())
                    };
                    (scope.clone(), status)
                })
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
    pub fn validate_member(&self, store: &FlowStore) -> io::Result<()> {
        if store.task_scope_binding() != self.0.first().map(|(scope, _)| scope) {
            return Err(invalid("task scope membership changed during admission"));
        }
        Ok(())
    }
    fn is_active(&self, snapshot: &ScopeSnapshot) -> io::Result<bool> {
        let mut active = true;
        let mut unavailable = None;
        for (scope, _) in &self.0 {
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
    ) -> io::Result<CommitReceipt> {
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
        Vec<tokio::sync::watch::Receiver<CommitReceipt>>,
    ) {
        let mut changes = Vec::new();
        let result = self.resolve_scope(Some(&mut changes)).await;
        (result, changes)
    }

    async fn resolve_scope(
        &self,
        changes: Option<&mut Vec<tokio::sync::watch::Receiver<CommitReceipt>>>,
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
                    .resolve_task_scope(self.address(), scope, changes)
                    .await
            }
        }
    }

    pub(super) async fn observe_task_scope(&self, admission: ScopeAdmission) -> io::Result<()> {
        if admission.0.is_empty() {
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
    async fn resolve_task_scope(
        &self,
        member: &FlowRef,
        scope: TaskScopeRef,
        mut changes: Option<&mut Vec<tokio::sync::watch::Receiver<CommitReceipt>>>,
    ) -> io::Result<ScopeAdmission> {
        let mut sources = Vec::new();
        let mut next = Some(scope);
        while let Some(scope) = next {
            if &scope.owner == member
                || sources
                    .iter()
                    .any(|(seen, _): &(TaskScopeRef, FlowWorker)| seen.owner == scope.owner)
            {
                return Err(invalid("task scope membership contains a cycle"));
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
            sources.push((scope, worker));
        }
        Ok(ScopeAdmission(sources))
    }

    /// Authenticate both endpoints before binding a Prepared member. Source validity is held through the member commit; identical retries retain their original receipt.
    pub async fn bind_task_scope(
        &self,
        member: FlowRef,
        scope: TaskScopeRef,
        authorize: impl Fn(&FlowRef, &TaskScopeRef) -> io::Result<()> + Send + Sync + 'static,
    ) -> io::Result<CommitReceipt> {
        authorize(&member, &scope)?;
        let worker = self.flow(&member).await?;
        let admission = self
            .resolve_task_scope(&member, scope.clone(), None)
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
