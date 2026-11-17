//! Reversible lending of the original storage owner before Session release.

use std::{io, sync::Arc};

use tokio::sync::oneshot;

use super::{Command, FlowWorker, resources::InstanceResources, trace::TraceBuffer};
use crate::flow_store::{FlowStore, position::FlowPosition};

pub(super) struct FlowHandoffSuspension {
    store: Option<FlowStore>,
    resume: Option<oneshot::Sender<Option<FlowStore>>>,
    resources: Arc<InstanceResources>,
    trace: Arc<TraceBuffer>,
}

pub(super) fn suspend(
    store: FlowStore,
    resources: Arc<InstanceResources>,
    trace: Arc<TraceBuffer>,
) -> (FlowHandoffSuspension, oneshot::Receiver<Option<FlowStore>>) {
    let (resume, resumed) = oneshot::channel();
    (
        FlowHandoffSuspension {
            store: Some(store),
            resume: Some(resume),
            resources,
            trace,
        },
        resumed,
    )
}

impl FlowHandoffSuspension {
    pub fn cursor(&self) -> &FlowPosition {
        self.store
            .as_ref()
            .expect("prepared storage owner")
            .cursor()
    }

    /// Commit only after every Session member prepared successfully. No file operation occurs here.
    pub fn commit(mut self) -> FlowStore {
        self.resources.close();
        self.trace.close();
        let _ = self
            .resume
            .take()
            .expect("prepared owner decision")
            .send(None);
        self.store.take().expect("prepared storage owner")
    }
}

impl Drop for FlowHandoffSuspension {
    fn drop(&mut self) {
        if let Some(store) = self.store.take()
            && let Some(resume) = self.resume.take()
            && resume.send(Some(store)).is_ok()
        {
            self.trace.resume();
        }
    }
}

impl FlowWorker {
    pub(super) async fn suspend_for_handoff(&self) -> io::Result<FlowHandoffSuspension> {
        let (reply, received) = oneshot::channel();
        self.commands
            .send(Command::SuspendHandoff(reply))
            .await
            .map_err(|_| {
                self.storage_error(io::Error::other(
                    "Flow owner stopped before handoff preparation",
                ))
            })?;
        received.await.map_err(|_| {
            self.storage_error(io::Error::other(
                "Flow handoff preparation did not acknowledge",
            ))
        })?
    }
}
