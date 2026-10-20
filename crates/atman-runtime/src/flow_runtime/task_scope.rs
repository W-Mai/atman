//! Explicit membership validation and cancellation through the shared close lifecycle.

use std::{collections::BTreeMap, io};

use atman_rt::InstanceState;
use uuid::Uuid;

use super::{FlowWorker, host::RuntimeHost};
use crate::event_log::journal::CommitReceipt;
use crate::flow_store::{
    FlowRef, FlowStore, FlowTransition, invalid,
    task_scope::{TaskScopeDefinition, TaskScopeRecord, TaskScopeRef},
};

/// Updated under the writer's gate before acknowledging any storage command.
pub(super) struct ScopeState {
    open: bool,
    records: BTreeMap<Uuid, bool>,
}

impl ScopeState {
    pub fn from_store(store: &FlowStore) -> Self {
        Self {
            open: matches!(store.state().state(), InstanceState::Open(_)),
            records: store
                .task_scopes()
                .map(|record| (record.created.definition.id, record.cancellation.is_none()))
                .collect(),
        }
    }
}

/// Sources are resolved without holding writer locks. The immutable ancestry is checked again under read guards at the member's commit boundary, from child to ancestor.
#[derive(Default)]
pub(super) struct ScopeAdmission(Vec<(TaskScopeRef, FlowWorker)>);

pub(super) struct ScopeRead<'a> {
    _guards: Vec<std::sync::RwLockReadGuard<'a, ScopeState>>,
    active: bool,
}

impl ScopeAdmission {
    pub fn validate_member(&self, store: &FlowStore) -> io::Result<()> {
        if store.task_scope_binding() != self.0.first().map(|(scope, _)| scope) {
            return Err(invalid("task scope membership changed during admission"));
        }
        Ok(())
    }
    fn read(&self) -> io::Result<ScopeRead<'_>> {
        let guards = self
            .0
            .iter()
            .map(|(_, worker)| {
                worker
                    .scope_state
                    .read()
                    .map_err(|_| invalid("task scope owner failed"))
            })
            .collect::<io::Result<Vec<_>>>()?;
        let mut active = true;
        for ((scope, worker), state) in self.0.iter().zip(&guards) {
            if worker.is_stopping() || worker.is_stopped() {
                return Err(invalid("task scope owner is unavailable"));
            }
            let registered = state
                .records
                .get(&scope.id)
                .ok_or_else(|| invalid("task scope is not registered by its owner"))?;
            active &= state.open && *registered;
        }
        Ok(ScopeRead {
            _guards: guards,
            active,
        })
    }

    pub fn acquire(&self) -> io::Result<ScopeRead<'_>> {
        let read = self.read()?;
        if !read.active {
            return Err(invalid("task scope is cancelled or its owner is closed"));
        }
        Ok(read)
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
        self.request(move |store| {
            let scope = admission.read()?;
            if !scope.active
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
            .request(move |store| {
                authorize(store.state().address(), &scope)?;
                // Retried binding cannot revive a cancelled group or alter membership.
                if store.task_scope_binding() == Some(&scope) {
                    return store.bind_task_scope(scope);
                }
                let _guard = admission.acquire()?;
                store.bind_task_scope(scope)
            })
            .await
    }
}
