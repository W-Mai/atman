//! Read-only activity subscription for a conversation surface.

use std::io;

use atman_rt::FlowInstance;
use tokio::sync::{broadcast, watch};
use tokio_util::task::AbortOnDropHandle;

use super::{FlowWorker, host::SessionHandle};
use crate::{TaskSnapshot, event::AtmanUuid, flow_store::FlowRef, task_registry::TaskFilter};

#[derive(Clone, Debug)]
pub struct ActivityFlow {
    pub registration_order: usize,
    pub address: FlowRef,
    pub title: String,
    pub preview: String,
    pub state: Option<FlowInstance<AtmanUuid>>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ActivitySnapshot {
    pub flows: Vec<ActivityFlow>,
    pub tasks: Vec<TaskSnapshot>,
    pub error: Option<String>,
}

/// Dropping a view releases its observer without stopping any execution owner.
pub struct ActivityReader {
    snapshots: watch::Receiver<ActivitySnapshot>,
    _task: AbortOnDropHandle<()>,
}

impl ActivityReader {
    pub fn snapshot(&mut self) -> ActivitySnapshot {
        self.snapshots.borrow_and_update().clone()
    }

    pub async fn changed(&mut self) {
        if self.snapshots.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

struct ObservedFlow {
    index: usize,
    name: watch::Receiver<String>,
    state: watch::Receiver<FlowInstance<AtmanUuid>>,
    live: super::live::LiveOutputReader,
}

type States = Vec<ObservedFlow>;

async fn capture(
    session: &SessionHandle,
    owner: &FlowRef,
) -> io::Result<(Vec<ActivityFlow>, States)> {
    let catalog = session.snapshot().await?.catalog;
    let mut rows = Vec::new();
    let mut states = Vec::new();
    for (registration_order, registration) in catalog.flows.iter().enumerate().rev() {
        if registration.id == owner.instance_id || catalog.archived_flows.contains(&registration.id)
        {
            continue;
        }
        // Subscribe before reading so a transition during capture remains observable.
        let mut preview = String::new();
        if let Some(worker) = session.loaded_flow(&registration.id) {
            let mut live = worker.subscribe_live_output();
            preview = live.preview();
            states.push(ObservedFlow {
                index: rows.len(),
                name: worker.subscribe_name(),
                state: worker.subscribe_state(),
                live,
            });
        }
        let address = FlowRef {
            session_id: owner.session_id,
            instance_id: registration.id.clone(),
        };
        let (title, state, recorded_preview, error) =
            match session.describe_flow(registration.id.clone()).await {
                Ok(observation) => (
                    observation.display_title,
                    Some(observation.state),
                    observation.preview,
                    None,
                ),
                Err(error) => (
                    "Unavailable flow".into(),
                    None,
                    None,
                    Some(error.to_string()),
                ),
            };
        if preview.is_empty() {
            preview = recorded_preview.unwrap_or_default();
        }
        rows.push(ActivityFlow {
            registration_order,
            address,
            title,
            preview,
            state,
            error,
        });
    }
    Ok((rows, states))
}

async fn state_changed(states: &mut States) {
    if states.is_empty() {
        std::future::pending::<()>().await;
    }
    let waits: Vec<_> = states
        .iter_mut()
        .map(|flow| {
            Box::pin(async {
                tokio::select! {
                    result = flow.state.changed() => result.is_ok(),
                    result = flow.name.changed() => result.is_ok(),
                    _ = flow.live.changed() => true,
                }
            })
        })
        .collect();
    let (result, index, remaining) = futures::future::select_all(waits).await;
    drop(remaining);
    if !result {
        states.remove(index);
    }
}

impl FlowWorker {
    /// Observe sibling flow states and this flow's processes without loading inactive execution owners.
    pub async fn subscribe_activity(&self) -> io::Result<ActivityReader> {
        let host = self.host.as_ref().map(|host| host.upgrade()).transpose()?;
        let mut directory = host.as_ref().map(|host| host.changes());
        let session = host
            .as_ref()
            .map(|host| host.session(self.address.session_id.into()))
            .transpose()?;
        let owner = self.address.clone();
        let tasks = self.resources.tasks().clone();
        let mut events = tasks.subscribe();
        let mut snapshot = ActivitySnapshot {
            flows: Vec::new(),
            tasks: tasks.list(&TaskFilter::default()),
            error: None,
        };
        let (sender, snapshots) = watch::channel(snapshot.clone());
        let task = tokio::spawn(async move {
            let mut states = Vec::new();
            // Catalog inspection can read inactive journals; keep it off the view attachment path.
            if let Some(session) = &session {
                match capture(session, &owner).await {
                    Ok((flows, current)) => {
                        snapshot.flows = flows;
                        states = current;
                    }
                    Err(error) => snapshot.error = Some(error.to_string()),
                }
                if sender.send(snapshot.clone()).is_err() {
                    return;
                }
            }
            loop {
                tokio::select! {
                    result = async {
                        match &mut directory {
                            Some(directory) => directory.changed().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        if result.is_err() { break; }
                        match capture(session.as_ref().unwrap(), &owner).await {
                            Ok((flows, current)) => {
                                snapshot.flows = flows;
                                states = current;
                                snapshot.error = None;
                            }
                            Err(error) => snapshot.error = Some(error.to_string()),
                        }
                    }
                    _ = state_changed(&mut states) => {}
                    result = events.recv() => {
                        if matches!(result, Err(broadcast::error::RecvError::Closed)) { break; }
                        // A complete snapshot also repairs a lagged broadcast receiver.
                        snapshot.tasks = tasks.list(&TaskFilter::default());
                    }
                }
                for flow in &mut states {
                    snapshot.flows[flow.index].title = flow.name.borrow_and_update().clone();
                    snapshot.flows[flow.index].state = Some(flow.state.borrow_and_update().clone());
                    snapshot.flows[flow.index].preview = flow.live.preview();
                }
                if sender.send(snapshot.clone()).is_err() {
                    break;
                }
            }
        });
        Ok(ActivityReader {
            snapshots,
            _task: AbortOnDropHandle::new(task),
        })
    }
}
