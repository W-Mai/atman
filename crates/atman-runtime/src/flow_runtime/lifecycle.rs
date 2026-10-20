//! Accepted close requests retain ownership until execution and resources have stopped.

use std::io;

use atman_rt::{InstanceActivity, InstanceState};

use super::{FlowWorker, resources::InstanceResources};
use crate::event_log::journal::CommitReceipt;
use crate::flow_store::control::{CommittedControl, ControlAction, ControlRequest};
use crate::flow_store::{FlowRef, FlowStore, FlowTransition, invalid};

impl FlowWorker {
    /// Cleanup failures leave the durable lifecycle Closing. A new close request retries cleanup; reopening also retries, without resuming execution.
    pub fn subscribe_close_error(&self) -> tokio::sync::watch::Receiver<Option<String>> {
        self.close_error.subscribe()
    }

    /// Authenticate and commit one control under the writer. Identical retries return the original receipt. Accepted closure owns its cleanup independently of the caller; observing Closing does not imply that cleanup has finished.
    pub async fn control(
        &self,
        request: ControlRequest,
        authorize: impl FnOnce(&FlowRef) -> io::Result<()> + Send + 'static,
    ) -> io::Result<CommittedControl> {
        let worker = self.clone();
        let runtime = tokio::runtime::Handle::current();
        let scope = if matches!(
            request.action,
            ControlAction::Resume | ControlAction::ReleasePause | ControlAction::Recover
        ) {
            Some(self.scope_admission().await)
        } else {
            None
        };
        self.request(move |store| {
            authorize(store.state().address())?;
            if worker.is_stopping() {
                return Err(invalid("instance host is stopping"));
            }
            let scope = match scope {
                Some(scope) if store.recorded_control(&request)?.is_none() => Some(scope?),
                _ => None,
            };
            let _scope = scope
                .as_ref()
                .map(|scope| {
                    scope.validate_member(store)?;
                    scope.acquire()
                })
                .transpose()?;
            let close = request.action == ControlAction::Close;
            let receipt = store.control(request)?;
            if close && matches!(store.state().state(), InstanceState::Closing { .. }) {
                // Establish ownership before acknowledgement, even if the request waiter vanished.
                worker.start_close(&runtime);
            }
            Ok(receipt)
        })
        .await
    }

    pub(super) fn continue_close(&self) {
        self.start_close(&tokio::runtime::Handle::current());
    }

    fn start_close(&self, runtime: &tokio::runtime::Handle) {
        let worker = self.clone();
        runtime.spawn(async move {
            if let Err(error) = worker.finish_close().await {
                crate::notify!(
                    error,
                    "flow {} close failed: {error}",
                    worker.address().instance_id
                );
            }
        });
    }

    pub(super) async fn wait_execution_stopped(&self) -> io::Result<()> {
        let mut state = self.subscribe_state();
        loop {
            if !matches!(
                state.borrow_and_update().state(),
                InstanceState::Open(InstanceActivity::Active(_))
                    | InstanceState::Closing { active: Some(_) }
            ) {
                return Ok(());
            }
            state
                .changed()
                .await
                .map_err(|_| io::Error::other("instance stopped before execution completed"))?;
        }
    }

    pub(super) async fn finish_close(&self) -> io::Result<()> {
        let _closing = self.close_gate.lock().await;
        self.close_error
            .send_if_modified(|error| error.take().is_some());
        let result = self.complete_close().await;
        self.close_error.send_if_modified(|error| {
            let current = result.as_ref().err().map(ToString::to_string);
            if *error == current {
                return false;
            }
            *error = current;
            true
        });
        result
    }

    async fn complete_close(&self) -> io::Result<()> {
        if self.state.borrow().state() == &InstanceState::Closed {
            return Ok(());
        }
        self.wait_execution_stopped().await?;
        self.resources.shutdown().await;
        let resources = self.resources.clone();
        self.request(move |store| {
            transition(
                store,
                &resources,
                store.state().revision(),
                FlowTransition::Closed,
            )
        })
        .await
        .map(drop)
    }
}

pub(super) fn transition(
    store: &mut FlowStore,
    resources: &InstanceResources,
    revision: u64,
    transition: FlowTransition,
) -> io::Result<CommitReceipt> {
    if matches!(transition, FlowTransition::Closed) {
        if !resources.is_stopped() {
            return Err(invalid("flow process resources are still stopping"));
        }
        if store.state().revision() != revision {
            return Err(invalid("stale flow state revision"));
        }
        let mut candidate = store.state().clone();
        candidate.finish_close().map_err(invalid)?;
        if store.state().state() != &InstanceState::Closed
            && let Some(workspace) = store.workspace_request()
        {
            workspace.release(store.state().address())?;
        }
    }
    store.transition(revision, transition)
}
