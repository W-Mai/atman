//! Message references share scope admission without making peers group members.

use std::{
    collections::{HashMap, HashSet},
    io,
};

use futures::FutureExt;
use tokio::sync::watch;

use super::{ScopeAdmission, ScopeSnapshot, TaskScopeRef};
use crate::flow_runtime::{FlowSnapshot, FlowWorker, observation::ObservationIssue};
use crate::flow_store::position::FlowPosition;
use crate::flow_store::{
    FlowStore,
    inbox::{Acceptance, DeliveryRequest, InputScopeInvalidation},
    invalid,
};

#[derive(Clone, Default)]
pub(in crate::flow_runtime) struct MessageScopes {
    pub dependencies: ScopeAdmission,
    admitted: HashMap<TaskScopeRef, ScopeAdmission>,
    pub changes: ScopeChanges,
}

#[derive(Clone, Debug, Default)]
pub(in crate::flow_runtime) struct ScopeChanges {
    sources: Vec<watch::Receiver<FlowPosition>>,
    directory: Option<watch::Receiver<()>>,
    retries: Option<watch::Receiver<()>>,
}

impl MessageScopes {
    pub fn eligible(&self, snapshot: &ScopeSnapshot, request: &DeliveryRequest) -> bool {
        self.eligible_scope(snapshot, request.scope.as_ref())
    }

    pub fn eligible_scope(&self, snapshot: &ScopeSnapshot, scope: Option<&TaskScopeRef>) -> bool {
        scope.is_none_or(|scope| {
            self.admitted
                .get(scope)
                .is_some_and(|admission| admission.check(snapshot).is_ok())
        })
    }

    pub fn invalidate(&self, store: &mut FlowStore, snapshot: &ScopeSnapshot) -> io::Result<()> {
        for scope in self.admitted.keys() {
            if let Ok(status) = snapshot.status(scope)
                && let Some(cancellation) = &status.cancellation
            {
                invalidate_messages(store, scope, cancellation)?;
            }
        }
        Ok(())
    }

    pub async fn changed(&mut self) {
        self.changes.changed().await;
    }
}

impl ScopeChanges {
    pub fn has_changed(&self) -> bool {
        self.sources
            .iter()
            .any(|source| source.has_changed().unwrap_or(true))
            || self
                .directory
                .iter()
                .chain(self.retries.iter())
                .any(|source| source.has_changed().unwrap_or(true))
    }

    pub async fn changed(&mut self) {
        let mut waits: Vec<futures::future::BoxFuture<'_, ()>> = self
            .sources
            .iter_mut()
            .map(|source| {
                async {
                    let _ = source.changed().await;
                }
                .boxed()
            })
            .collect();
        for source in self.directory.iter_mut().chain(self.retries.iter_mut()) {
            waits.push(
                async {
                    let _ = source.changed().await;
                }
                .boxed(),
            );
        }
        if waits.is_empty() {
            std::future::pending::<()>().await;
        } else {
            futures::future::select_all(waits).await;
        }
    }
}

impl FlowWorker {
    /// Input dependency discovery does not copy queued bodies or invocation configuration.
    pub(in crate::flow_runtime) async fn pending_input_scopes(
        &self,
    ) -> io::Result<Vec<TaskScopeRef>> {
        self.pending_input_scope_snapshot()
            .await
            .map(|(_, scopes)| scopes)
    }

    async fn pending_input_scope_snapshot(&self) -> io::Result<(FlowPosition, Vec<TaskScopeRef>)> {
        self.request(|store| {
            let mut seen = HashSet::new();
            let scopes = store
                .pending_inputs_at_head()
                .filter_map(|input| input.request.scope.as_ref())
                .filter(|scope| seen.insert(*scope))
                .cloned()
                .collect();
            Ok((store.cursor().clone(), scopes))
        })
        .await
    }

    pub(in crate::flow_runtime) async fn resolve_message_scopes(
        &self,
        references: Vec<TaskScopeRef>,
    ) -> MessageScopes {
        let mut scopes = MessageScopes {
            changes: ScopeChanges {
                directory: self
                    .host
                    .as_ref()
                    .and_then(|host| host.upgrade().ok())
                    .map(|host| host.changes()),
                retries: Some(self.observation_retry.subscribe()),
                ..Default::default()
            },
            ..Default::default()
        };
        for source in scopes
            .changes
            .directory
            .iter_mut()
            .chain(scopes.changes.retries.iter_mut())
        {
            source.borrow_and_update();
        }
        let mut seen = HashSet::new();
        for scope in references
            .into_iter()
            .filter(|scope| seen.insert(scope.clone()))
        {
            if let Ok(admission) = self
                .resolve_message_scope(scope.clone(), Some(&mut scopes.changes.sources))
                .await
            {
                scopes.dependencies.0.extend(admission.0.iter().cloned());
                scopes.admitted.insert(scope.clone(), admission);
            }
        }
        scopes
    }

    pub(in crate::flow_runtime) async fn scheduling_snapshot(
        &self,
    ) -> io::Result<(Option<FlowSnapshot>, MessageScopes)> {
        let (cursor, references) = self.pending_input_scope_snapshot().await?;
        let scopes = self.resolve_message_scopes(references).await;
        let checked = scopes.clone();
        let view = self
            .request_scoped(scopes.dependencies.clone(), move |store, snapshot| {
                if store.cursor() != &cursor {
                    return Ok(None);
                }
                checked.invalidate(store, snapshot)?;
                let eligible = |input: &crate::flow_store::inbox::InputEnvelope| {
                    checked.eligible_scope(snapshot, input.request.scope.as_ref())
                };
                let view = if store.can_admit_turn() && store.pending_inputs_at_head().any(eligible)
                {
                    FlowSnapshot::read_pending(store, eligible)?
                } else {
                    FlowSnapshot::state_only(store)
                };
                Ok(Some(view))
            })
            .await?;
        Ok((view, scopes))
    }

    /// Authenticate the sender before admission; the callback authorizes only new delivery.
    ///
    /// New scoped messages require an available active source. Original accepted requests retain their receipts even after source cancellation or removal.
    pub async fn accept_input(
        &self,
        request: DeliveryRequest,
        authorize_new: impl FnOnce(&DeliveryRequest) -> io::Result<()> + Send + 'static,
    ) -> io::Result<Acceptance> {
        let Some(scope) = request.scope.clone() else {
            return self
                .request(move |store| store.accept_input(request, authorize_new))
                .await;
        };
        let lookup = request.clone();
        if let Some(previous) = self
            .request(move |store| store.input_acceptance(&lookup))
            .await?
        {
            return Ok(previous);
        }
        let admission = self.resolve_message_scope(scope, None).await;
        let dependencies = admission.as_ref().ok().cloned().unwrap_or_default();
        self.request_scoped(dependencies, move |store, snapshot| {
            store.accept_input(request, |request| {
                authorize_new(request)?;
                admission?.check(snapshot)
            })
        })
        .await
    }

    pub(in crate::flow_runtime) async fn resolve_message_scope(
        &self,
        scope: TaskScopeRef,
        changes: Option<&mut Vec<watch::Receiver<FlowPosition>>>,
    ) -> io::Result<ScopeAdmission> {
        scope.validate()?;
        self.host
            .as_ref()
            .ok_or_else(|| invalid("scoped messages require their runtime host"))?
            .upgrade()?
            .resolve_task_scope(None, scope, changes)
            .await
    }

    pub(in crate::flow_runtime) async fn pending_message_scopes(
        &self,
    ) -> io::Result<Vec<TaskScopeRef>> {
        self.request(|store| {
            let mut seen = HashSet::new();
            Ok(store
                .pending_inputs()
                .filter_map(|input| input.request.scope.clone())
                .chain(store.pending_send_scopes())
                .filter(|scope| seen.insert(scope.clone()))
                .collect::<Vec<_>>())
        })
        .await
    }

    /// Subscribe before sampling, retaining unknown sources and cancelling only confirmed scope facts.
    pub(in crate::flow_runtime) async fn observe_message_scopes(
        &self,
        changes: &mut Vec<watch::Receiver<FlowPosition>>,
    ) -> io::Result<Vec<ObservationIssue>> {
        let scopes = self.pending_message_scopes().await?;
        let mut issues = Vec::new();
        for scope in scopes {
            let result = match self
                .resolve_message_scope(scope.clone(), Some(changes))
                .await
            {
                Ok(admission) => {
                    let observed = scope.clone();
                    self.request_scoped(admission, move |store, snapshot| {
                        if let Some(cancellation) = &snapshot.status(&observed)?.cancellation {
                            invalidate_messages(store, &observed, cancellation)?;
                        }
                        Ok(())
                    })
                    .await
                }
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                issues.push(ObservationIssue {
                    source: scope.owner,
                    reason: error.to_string(),
                });
            }
        }
        Ok(issues)
    }
}

fn invalidate_messages(
    store: &mut FlowStore,
    scope: &TaskScopeRef,
    cancellation: &crate::flow_store::task_scope::ScopeCancellation,
) -> io::Result<()> {
    if store
        .pending_inputs()
        .any(|input| input.request.scope.as_ref() == Some(scope))
    {
        store.invalidate_input_scope(InputScopeInvalidation {
            scope: scope.clone(),
            cancellation: cancellation.clone(),
        })?;
    }
    store.cancel_scoped_replies(scope)
}
