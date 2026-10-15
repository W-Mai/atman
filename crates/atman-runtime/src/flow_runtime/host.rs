//! Host-owned session catalogs and canonical handles for persistent flow instances.

use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex, RwLock, Weak},
};

use atman_rt::FlowInstance;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::task::AbortOnDropHandle;
use uuid::Uuid;

use super::{
    FlowSnapshot, FlowWorker,
    execution::TurnRequest,
    scheduler::{FlowScheduler, PrepareTurn, SchedulingStatus},
};
use crate::event::AtmanUuid;
use crate::event_log::journal::CommitReceipt;
use crate::flow_store::access::{FlowGrant, SessionAccess};
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
    directory: Arc<Directory>,
}

struct Directory {
    sessions: Mutex<HashMap<Uuid, SessionHandle>>,
    changes: watch::Sender<()>,
}

impl Default for Directory {
    fn default() -> Self {
        Self {
            sessions: Default::default(),
            changes: watch::channel(()).0,
        }
    }
}

/// Workers do not keep their owning directory alive through a reference cycle.
#[derive(Clone)]
pub(super) struct WeakRuntimeHost(Weak<Directory>);

impl WeakRuntimeHost {
    pub fn upgrade(&self) -> io::Result<RuntimeHost> {
        self.0
            .upgrade()
            .map(|directory| RuntimeHost { directory })
            .ok_or_else(|| io::Error::other("instance host is no longer available"))
    }
}

impl RuntimeHost {
    pub(super) fn changes(&self) -> watch::Receiver<()> {
        self.directory.changes.subscribe()
    }

    fn changed(&self) {
        self.directory.changes.send_modify(|_| {});
    }

    /// Attach an exclusively owned catalog without opening or starting its recorded flows.
    pub fn attach_session(&self, store: SessionStore) -> io::Result<SessionHandle> {
        let runtime = tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
        let id = store.projection().descriptor.id;
        let mut sessions = self.directory.sessions.lock().unwrap();
        if sessions.contains_key(&id) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "session is already attached",
            ));
        }
        let (commands, receiver) = mpsc::channel(64);
        let flows = FlowDirectory::default();
        let access = Arc::new(RwLock::new(store.projection().access.clone()));
        let handle = SessionHandle {
            id,
            commands,
            flows: flows.clone(),
            access: access.clone(),
        };
        runtime.spawn(serve_catalog(
            store,
            flows,
            access,
            WeakRuntimeHost(Arc::downgrade(&self.directory)),
            receiver,
        ));
        sessions.insert(id, handle.clone());
        self.changed();
        Ok(handle)
    }

    pub fn session(&self, id: Uuid) -> io::Result<SessionHandle> {
        self.directory
            .sessions
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "session is not attached"))
    }

    pub fn sessions(&self) -> Vec<Uuid> {
        let mut sessions: Vec<_> = self
            .directory
            .sessions
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect();
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
    pub(super) access: Arc<RwLock<SessionAccess>>,
}

#[derive(Clone, Debug)]
pub struct SessionSnapshot {
    pub catalog: SessionProjection,
    pub cursor: CommitReceipt,
}

enum CatalogCommand {
    Describe {
        id: FlowInstanceId,
        reply: oneshot::Sender<io::Result<(String, FlowInstance<AtmanUuid>)>>,
    },
    Schedule {
        id: FlowInstanceId,
        executor: Box<crate::Executor>,
        prepare: PrepareTurn,
        reply: oneshot::Sender<io::Result<watch::Receiver<SchedulingStatus>>>,
    },
    StopScheduling {
        id: FlowInstanceId,
        reply: oneshot::Sender<io::Result<()>>,
    },
    SchedulingStatus {
        id: FlowInstanceId,
        reply: oneshot::Sender<io::Result<Option<watch::Receiver<SchedulingStatus>>>>,
    },
    Access {
        change: AccessChange,
        expected: CommitReceipt,
        reply: oneshot::Sender<io::Result<CommitReceipt>>,
    },
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

enum AccessChange {
    Grant(FlowGrant),
    Revoke(Uuid),
}

enum FlowOperation {
    Create(Box<FlowDescriptor>),
    Open(FlowInstanceId),
}

impl SessionHandle {
    pub(super) fn is_stopped(&self) -> bool {
        self.commands.is_closed()
    }

    /// Trusted host operation; this is not an unverified peer delegation endpoint.
    pub async fn grant_access(
        &self,
        expected: CommitReceipt,
        grant: FlowGrant,
    ) -> io::Result<CommitReceipt> {
        self.request(|reply| CatalogCommand::Access {
            change: AccessChange::Grant(grant),
            expected,
            reply,
        })
        .await
    }

    pub async fn revoke_access(
        &self,
        expected: CommitReceipt,
        id: Uuid,
    ) -> io::Result<CommitReceipt> {
        self.request(|reply| CatalogCommand::Access {
            change: AccessChange::Revoke(id),
            expected,
            reply,
        })
        .await
    }
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// Install automatic admission under catalog ownership. Dropping the returned status receiver
    /// or the requesting view does not stop execution. A live controller must be stopped explicitly
    /// before replacement; recovered journals require a fresh trusted execution binding.
    pub async fn schedule_flow(
        &self,
        id: FlowInstanceId,
        executor: crate::Executor,
        prepare: impl FnMut(&FlowSnapshot) -> io::Result<Option<TurnRequest>> + Send + 'static,
    ) -> io::Result<watch::Receiver<SchedulingStatus>> {
        self.open_flow(id.clone()).await?;
        self.request(|reply| CatalogCommand::Schedule {
            id,
            executor: Box::new(executor),
            prepare: Box::new(prepare),
            reply,
        })
        .await
    }

    /// Stop automatic admission, leaving admitted execution and persistent pause keys unchanged.
    pub async fn stop_scheduling(&self, id: FlowInstanceId) -> io::Result<()> {
        self.request(|reply| CatalogCommand::StopScheduling { id, reply })
            .await
    }

    pub async fn scheduling_status(
        &self,
        id: FlowInstanceId,
    ) -> io::Result<Option<watch::Receiver<SchedulingStatus>>> {
        self.request(|reply| CatalogCommand::SchedulingStatus { id, reply })
            .await
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

    /// Read a registered instance without retaining its conversation or installing observers. Live writers provide their current state; inactive journals recover under catalog ownership.
    pub async fn describe_flow(
        &self,
        id: FlowInstanceId,
    ) -> io::Result<(String, FlowInstance<AtmanUuid>)> {
        let existing = self
            .flows
            .lock()
            .unwrap()
            .get(&id)
            .filter(|worker| !worker.is_stopped())
            .cloned();
        if let Some(worker) = existing {
            return worker
                .request(|store| Ok((store.descriptor().entry.name.clone(), store.state().clone())))
                .await;
        }
        self.request(|reply| CatalogCommand::Describe { id, reply })
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
    access: Arc<RwLock<SessionAccess>>,
    host: WeakRuntimeHost,
    mut commands: mpsc::Receiver<CatalogCommand>,
) {
    // Catalog lifetime owns observation, independently of execution and frontend handles.
    let mut observers: HashMap<FlowInstanceId, AbortOnDropHandle<()>> = HashMap::new();
    let mut schedulers: HashMap<FlowInstanceId, FlowScheduler> = HashMap::new();
    while let Some(command) = commands.recv().await {
        match command {
            CatalogCommand::Describe { id, reply } => {
                let existing = flows
                    .lock()
                    .unwrap()
                    .get(&id)
                    .filter(|worker| !worker.is_stopped())
                    .cloned();
                let result = if let Some(worker) = existing {
                    worker
                        .request(|store| {
                            Ok((store.descriptor().entry.name.clone(), store.state().clone()))
                        })
                        .await
                } else {
                    let returned = tokio::task::spawn_blocking(move || {
                        let result = store.open_flow(&id).map(|flow| {
                            (flow.descriptor().entry.name.clone(), flow.state().clone())
                        });
                        (store, result)
                    })
                    .await;
                    let Ok((returned, result)) = returned else {
                        return;
                    };
                    store = returned;
                    result
                };
                let _ = reply.send(result);
            }
            CatalogCommand::Schedule {
                id,
                executor,
                prepare,
                reply,
            } => {
                let worker = flows
                    .lock()
                    .unwrap()
                    .get(&id)
                    .filter(|w| !w.is_stopped())
                    .cloned();
                let result = match worker {
                    None => Err(invalid("instance storage is unavailable")),
                    Some(worker) => {
                        if schedulers.get(&id).is_some_and(|controller| {
                            *controller.subscribe_status().borrow() == SchedulingStatus::Running
                        }) {
                            Err(invalid("instance already has a host scheduler"))
                        } else {
                            // Await terminal ownership release before installing a replacement.
                            if let Some(previous) = schedulers.remove(&id) {
                                let _ = previous.stop().await;
                            }
                            worker.schedule(*executor, prepare).map(|controller| {
                                let status = controller.subscribe_status();
                                schedulers.insert(id, controller);
                                status
                            })
                        }
                    }
                };
                let _ = reply.send(result);
            }
            CatalogCommand::StopScheduling { id, reply } => {
                let result = match schedulers.remove(&id) {
                    Some(controller) => controller.stop().await,
                    None => Ok(()),
                };
                let _ = reply.send(result);
            }
            CatalogCommand::SchedulingStatus { id, reply } => {
                let _ = reply.send(Ok(schedulers.get(&id).map(FlowScheduler::subscribe_status)));
            }
            CatalogCommand::Access {
                change,
                expected,
                reply,
            } => {
                let published = access.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let result = match change {
                        AccessChange::Grant(grant) => store.grant_access(&expected, grant),
                        AccessChange::Revoke(id) => store.revoke_access(&expected, id),
                    };
                    if result.is_ok() {
                        *published.write().unwrap() = store.projection().access.clone();
                    }
                    (store, result)
                })
                .await;
                let Ok((returned, result)) = result else {
                    return;
                };
                store = returned;
                if result.is_ok() {
                    if let Ok(host) = host.upgrade() {
                        host.changed();
                    }
                }
                let _ = reply.send(result);
            }
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
                if let Some(controller) = schedulers.remove(&id) {
                    let _ = controller.stop().await;
                }
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
                    Ok(flow) => FlowWorker::start_owned(flow, Some(host.clone())).await,
                    Err(error) => Err(error),
                };
                if let Ok(worker) = &result {
                    // Install before acknowledgement, including when the request's receiver was dropped.
                    flows.lock().unwrap().insert(id.clone(), worker.clone());
                    observers.insert(id, super::observation::start(worker.clone(), host.clone()));
                    if let Ok(host) = host.upgrade() {
                        host.changed();
                    }
                }
                let _ = reply.send(result);
            }
        }
    }
}
