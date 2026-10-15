//! Host-owned session catalogs and canonical handles for persistent flow instances.

use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex},
};

use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use super::FlowWorker;
use crate::event_log::journal::CommitReceipt;
use crate::flow_store::inbox::{Acceptance, DeliveryRequest};

use crate::flow_store::{
    FlowDescriptor, FlowInstanceId, FlowRef, SessionProjection, SessionStore, invalid,
};

type FlowDirectory = Arc<Mutex<HashMap<FlowInstanceId, FlowWorker>>>;

/// A trusted host directory. Registration does not grant peer control or delivery authority.
///
/// The map lock only protects handles. Catalog I/O is serialized by each session owner, and flow input/execution goes directly to the instance worker without passing through that catalog queue.
#[derive(Clone, Default)]
pub struct RuntimeHost {
    sessions: Arc<Mutex<HashMap<Uuid, SessionHandle>>>,
}

impl RuntimeHost {
    /// Attach an exclusively owned catalog without opening or starting its recorded flows.
    pub fn attach_session(&self, store: SessionStore) -> io::Result<SessionHandle> {
        let runtime = tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
        let id = store.projection().descriptor.id;
        let mut sessions = self.sessions.lock().unwrap();
        if sessions.contains_key(&id) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "session is already attached",
            ));
        }
        let (commands, receiver) = mpsc::channel(64);
        let flows = FlowDirectory::default();
        let handle = SessionHandle {
            id,
            commands,
            flows: flows.clone(),
        };
        runtime.spawn(serve_catalog(store, flows, receiver));
        sessions.insert(id, handle.clone());
        Ok(handle)
    }

    pub fn session(&self, id: Uuid) -> io::Result<SessionHandle> {
        self.sessions
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "session is not attached"))
    }

    pub fn sessions(&self) -> Vec<Uuid> {
        let mut sessions: Vec<_> = self.sessions.lock().unwrap().keys().copied().collect();
        sessions.sort_unstable();
        sessions
    }

    pub async fn flow(&self, address: &FlowRef) -> io::Result<FlowWorker> {
        self.session(address.session_id.0)?
            .open_flow(address.instance_id.clone())
            .await
    }

    /// Authenticate the sender before routing, including retries. Authorize new delivery at its commit boundary.
    pub async fn deliver(
        &self,
        request: DeliveryRequest,
        authorize_new: impl FnOnce(&DeliveryRequest) -> io::Result<()> + Send + 'static,
    ) -> io::Result<Acceptance> {
        self.flow(&request.recipient)
            .await?
            .accept_input(request, authorize_new)
            .await
    }
}

/// A session catalog owner shared by all views and callers.
///
/// Dropping the last handle drains accepted catalog work. Instance handles retain their own storage ownership and are not cancelled by catalog handle lifetime.
#[derive(Clone)]
pub struct SessionHandle {
    id: Uuid,
    commands: mpsc::Sender<CatalogCommand>,
    flows: FlowDirectory,
}

#[derive(Clone, Debug)]
pub struct SessionSnapshot {
    pub catalog: SessionProjection,
    pub cursor: CommitReceipt,
}

enum CatalogCommand {
    Flow {
        operation: FlowOperation,
        reply: oneshot::Sender<io::Result<FlowWorker>>,
    },
    Select {
        id: FlowInstanceId,
        reply: oneshot::Sender<io::Result<CommitReceipt>>,
    },
    Snapshot(oneshot::Sender<io::Result<SessionSnapshot>>),
}

enum FlowOperation {
    Create(Box<FlowDescriptor>),
    Open(FlowInstanceId),
}

impl SessionHandle {
    pub fn id(&self) -> Uuid {
        self.id
    }

    async fn request<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<io::Result<T>>) -> CatalogCommand,
    ) -> io::Result<T> {
        let (reply, received) = oneshot::channel();
        self.commands
            .send(make(reply))
            .await
            .map_err(|_| io::Error::other("session catalog stopped"))?;
        received
            .await
            .map_err(|_| io::Error::other("session catalog stopped before acknowledgement"))?
    }

    pub async fn snapshot(&self) -> io::Result<SessionSnapshot> {
        self.request(CatalogCommand::Snapshot).await
    }

    pub async fn create_flow(&self, descriptor: FlowDescriptor) -> io::Result<FlowWorker> {
        self.request(|reply| CatalogCommand::Flow {
            operation: FlowOperation::Create(Box::new(descriptor)),
            reply,
        })
        .await
    }

    pub async fn open_flow(&self, id: FlowInstanceId) -> io::Result<FlowWorker> {
        if let Some(worker) = self
            .flows
            .lock()
            .unwrap()
            .get(&id)
            .filter(|worker| !worker.is_stopped())
            .cloned()
        {
            return Ok(worker);
        }
        self.request(|reply| CatalogCommand::Flow {
            operation: FlowOperation::Open(id),
            reply,
        })
        .await
    }

    /// Change only the default view binding; this does not start, pause, or privilege a flow.
    pub async fn select_default(&self, id: FlowInstanceId) -> io::Result<CommitReceipt> {
        self.request(|reply| CatalogCommand::Select { id, reply })
            .await
    }
}

async fn serve_catalog(
    mut store: SessionStore,
    flows: FlowDirectory,
    mut commands: mpsc::Receiver<CatalogCommand>,
) {
    while let Some(command) = commands.recv().await {
        match command {
            CatalogCommand::Snapshot(reply) => {
                let _ = reply.send(Ok(SessionSnapshot {
                    catalog: store.projection().clone(),
                    cursor: store.cursor().clone(),
                }));
            }
            CatalogCommand::Select { id, reply } => {
                let result = tokio::task::spawn_blocking(move || {
                    let result = store.select_default(id);
                    (store, result)
                })
                .await;
                let Ok((returned, result)) = result else {
                    return;
                };
                store = returned;
                let _ = reply.send(result);
            }
            CatalogCommand::Flow { operation, reply } => {
                let id = match &operation {
                    FlowOperation::Create(descriptor) => descriptor.address.instance_id.clone(),
                    FlowOperation::Open(id) => id.clone(),
                };
                // Never reopen a live writer: that would either contend on its journal or misclassify execution as host loss.
                let existing = flows
                    .lock()
                    .unwrap()
                    .get(&id)
                    .filter(|worker| !worker.is_stopped())
                    .cloned();
                if let Some(worker) = existing {
                    let result = match operation {
                        FlowOperation::Open(_) => Ok(worker.clone()),
                        FlowOperation::Create(descriptor) => worker
                            .request(move |store| {
                                if store.descriptor() != descriptor.as_ref() {
                                    return Err(invalid("flow creation payload changed"));
                                }
                                Ok(())
                            })
                            .await
                            .map(|()| worker.clone()),
                    };
                    let _ = reply.send(result);
                    continue;
                }
                flows.lock().unwrap().remove(&id);
                let result = tokio::task::spawn_blocking(move || {
                    let result = match operation {
                        FlowOperation::Create(descriptor) => store.create_flow(*descriptor),
                        FlowOperation::Open(id) => store.open_flow(&id),
                    };
                    (store, result)
                })
                .await;
                let Ok((returned, result)) = result else {
                    return;
                };
                store = returned;
                let result = match result {
                    Ok(flow) => FlowWorker::start(flow).await,
                    Err(error) => Err(error),
                };
                if let Ok(worker) = &result {
                    // Install before acknowledgement, including when the request's receiver was dropped.
                    flows.lock().unwrap().insert(id, worker.clone());
                }
                let _ = reply.send(result);
            }
        }
    }
}
