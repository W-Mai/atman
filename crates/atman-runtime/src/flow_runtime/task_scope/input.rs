//! Input references share scope admission without making the recipient a group member.

use std::{collections::HashSet, io};

use tokio::sync::watch;

use super::{ScopeAdmission, TaskScopeRef};
use crate::event_log::journal::CommitReceipt;
use crate::flow_runtime::{FlowWorker, observation::ObservationIssue};
use crate::flow_store::{
    inbox::{Acceptance, DeliveryRequest, InputScopeInvalidation},
    invalid,
};

impl FlowWorker {
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
        let admission = self.resolve_input_scope(scope, None).await;
        let dependencies = admission.as_ref().ok().cloned().unwrap_or_default();
        self.request_scoped(dependencies, move |store, snapshot| {
            store.accept_input(request, |request| {
                authorize_new(request)?;
                admission?.check(snapshot)
            })
        })
        .await
    }

    async fn resolve_input_scope(
        &self,
        scope: TaskScopeRef,
        changes: Option<&mut Vec<watch::Receiver<CommitReceipt>>>,
    ) -> io::Result<ScopeAdmission> {
        scope.validate()?;
        self.host
            .as_ref()
            .ok_or_else(|| invalid("scoped input requires its runtime host"))?
            .upgrade()?
            .resolve_task_scope(None, scope, changes)
            .await
    }

    /// Subscribe before sampling, retaining unknown sources and cancelling only confirmed scope facts.
    pub(in crate::flow_runtime) async fn observe_input_scopes(
        &self,
        changes: &mut Vec<watch::Receiver<CommitReceipt>>,
    ) -> io::Result<Vec<ObservationIssue>> {
        let scopes = self
            .request(|store| {
                let mut seen = HashSet::new();
                Ok(store
                    .pending_inputs()
                    .filter_map(|input| input.request.scope.as_ref())
                    .filter(|scope| seen.insert((*scope).clone()))
                    .cloned()
                    .collect::<Vec<_>>())
            })
            .await?;
        let mut issues = Vec::new();
        for scope in scopes {
            let result = match self.resolve_input_scope(scope.clone(), Some(changes)).await {
                Ok(admission) => {
                    let observed = scope.clone();
                    self.request_scoped(admission, move |store, snapshot| {
                        if let Some(cancellation) = &snapshot.status(&observed)?.cancellation {
                            store.invalidate_input_scope(InputScopeInvalidation {
                                scope: observed,
                                cancellation: cancellation.clone(),
                            })?;
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
