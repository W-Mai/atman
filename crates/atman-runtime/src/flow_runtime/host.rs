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

/// Services installed before any instance opens. Approval endpoint ownership remains with the frontend or transport, independently of retained execution handles.
#[derive(Clone, Default)]
pub struct HostServices {
    pub redactor: Option<Arc<crate::redact::Redactor>>,
    pub permission_clients: Weak<crate::permission::PermissionClients>,
}

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
    services: HostServices,
    stopping: std::sync::atomic::AtomicBool,
    shutdown: tokio::sync::OnceCell<Result<(), String>>,
}

impl Default for Directory {
    fn default() -> Self {
        Self {
            sessions: Default::default(),
            changes: watch::channel(()).0,
            services: HostServices::default(),
            stopping: Default::default(),
            shutdown: Default::default(),
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
            .filter(|directory| {
                !directory
                    .stopping
                    .load(std::sync::atomic::Ordering::Acquire)
            })
            .map(|directory| RuntimeHost { directory })
            .ok_or_else(|| io::Error::other("instance host is no longer available"))
    }
}

impl RuntimeHost {
    /// Bind service diagnostic redaction before opening instances. Per-invocation sinks can supply their own execution diagnostic policy.
    pub fn with_redactor(redactor: Arc<crate::redact::Redactor>) -> Self {
        Self::with_services(HostServices {
            redactor: Some(redactor),
            ..HostServices::default()
        })
    }

    pub fn with_services(services: HostServices) -> Self {
        Self {
            directory: Arc::new(Directory {
                services,
                ..Directory::default()
            }),
        }
    }

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
        if self
            .directory
            .stopping
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(invalid("instance host is stopping"));
        }
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
            self.directory.services.clone(),
            receiver,
        ));
        sessions.insert(id, handle.clone());
        self.changed();
        Ok(handle)
    }

    pub fn session(&self, id: Uuid) -> io::Result<SessionHandle> {
        if self
            .directory
            .stopping
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(invalid("instance host is stopping"));
        }
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

    pub(super) fn owned_flows(&self) -> io::Result<Vec<FlowWorker>> {
        let sessions = self.directory.sessions.lock().unwrap();
        if self
            .directory
            .stopping
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(invalid("instance host is stopping"));
        }
        Ok(sessions
            .values()
            .flat_map(|session| {
                session
                    .flows
                    .lock()
                    .unwrap()
                    .values()
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .collect())
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

    /// Stop all attached execution owners and release their journals and process resources.
    ///
    /// Accepted shutdown continues if the waiting caller disappears. Repeated calls observe the same result. Unread input, goals, pause keys and subscriptions remain recorded; instances are not closed or automatically resumed. Disconnecting a frontend must not call this operation.
    pub async fn shutdown(&self) -> io::Result<()> {
        let directory = self.directory.clone();
        tokio::spawn(async move {
            directory
                .shutdown
                .get_or_init(|| async {
                    let sessions: Vec<_> = {
                        let sessions = directory.sessions.lock().unwrap();
                        directory
                            .stopping
                            .store(true, std::sync::atomic::Ordering::Release);
                        sessions.values().cloned().collect()
                    };
                    directory.changes.send_modify(|_| {});
                    let results = futures::future::join_all(sessions.iter().map(|session| async {
                        session
                            .request(CatalogCommand::Shutdown)
                            .await
                            .map_err(|error| format!("session {}: {error}", session.id))
                    }))
                    .await;
                    directory.sessions.lock().unwrap().clear();
                    let errors: Vec<_> = results.into_iter().filter_map(Result::err).collect();
                    if errors.is_empty() {
                        Ok(())
                    } else {
                        Err(errors.join("; "))
                    }
                })
                .await
                .clone()
                .map_err(io::Error::other)
        })
        .await
        .map_err(io::Error::other)?
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
    Shutdown(oneshot::Sender<io::Result<()>>),
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
    AdoptScheduling {
        worker: FlowWorker,
        controller: FlowScheduler,
        reply: oneshot::Sender<io::Result<()>>,
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
    Create(Box<FlowDescriptor>, Option<CommitReceipt>),
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
        let worker = self.open_flow(id.clone()).await?;
        let _binding = worker.launch_gate.clone().lock_owned().await;
        self.install_scheduler(id, executor, Box::new(prepare))
            .await
    }

    /// The caller holds this instance's activation lock through preparation and installation.
    pub(super) async fn install_scheduler(
        &self,
        id: FlowInstanceId,
        executor: crate::Executor,
        prepare: PrepareTurn,
    ) -> io::Result<watch::Receiver<SchedulingStatus>> {
        self.request(|reply| CatalogCommand::Schedule {
            id,
            executor: Box::new(executor),
            prepare,
            reply,
        })
        .await
    }

    pub(crate) async fn adopt_scheduler(
        &self,
        worker: FlowWorker,
        controller: FlowScheduler,
    ) -> io::Result<()> {
        self.request(|reply| CatalogCommand::AdoptScheduling {
            worker,
            controller,
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
        self.create_flow_at(descriptor, None).await
    }

    pub(crate) async fn create_flow_at(
        &self,
        descriptor: FlowDescriptor,
        expected: Option<CommitReceipt>,
    ) -> io::Result<FlowWorker> {
        self.request(|reply| CatalogCommand::Flow {
            operation: FlowOperation::Create(Box::new(descriptor), expected),
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
                .request(|store| {
                    Ok((
                        store.descriptor().content.title().to_owned(),
                        store.state().clone(),
                    ))
                })
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
    services: HostServices,
    mut commands: mpsc::Receiver<CatalogCommand>,
) {
    // Catalog lifetime owns observation, independently of execution and frontend handles.
    let mut observers: HashMap<FlowInstanceId, AbortOnDropHandle<()>> = HashMap::new();
    let mut schedulers: HashMap<FlowInstanceId, FlowScheduler> = HashMap::new();
    while let Some(command) = commands.recv().await {
        match command {
            CatalogCommand::Shutdown(reply) => {
                // Reject queued catalog work before waiting for executions which may themselves be awaiting a catalog reply.
                commands.close();
                drop(commands);
                let mut errors = Vec::new();
                for (id, controller) in schedulers.drain() {
                    if let Err(error) = controller.stop().await {
                        errors.push(format!("flow {id} scheduler: {error}"));
                    }
                }
                for (_, observer) in observers.drain() {
                    observer.abort();
                    let _ = observer.await;
                }
                let workers: Vec<_> = flows
                    .lock()
                    .unwrap()
                    .drain()
                    .map(|(_, worker)| worker)
                    .collect();
                let results = futures::future::join_all(
                    workers
                        .into_iter()
                        .filter(|worker| !worker.is_stopped())
                        .map(|worker| async move {
                            let address = worker.address().clone();
                            worker
                                .stop_execution()
                                .await
                                .map_err(|error| format!("flow {}: {error}", address.instance_id))
                        }),
                )
                .await;
                errors.extend(results.into_iter().filter_map(Result::err));
                // Release the catalog owner before acknowledging that another host can open it.
                drop(store);
                let _ = reply.send(if errors.is_empty() {
                    Ok(())
                } else {
                    Err(io::Error::other(errors.join("; ")))
                });
                return;
            }
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
                            Ok((
                                store.descriptor().content.title().to_owned(),
                                store.state().clone(),
                            ))
                        })
                        .await
                } else {
                    let returned = tokio::task::spawn_blocking(move || {
                        let result = store.open_flow(&id).map(|flow| {
                            (
                                flow.descriptor().content.title().to_owned(),
                                flow.state().clone(),
                            )
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
            CatalogCommand::AdoptScheduling {
                worker,
                controller,
                reply,
            } => {
                let id = &worker.address().instance_id;
                let current = flows.lock().unwrap().get(id).is_some_and(|current| {
                    !current.is_stopped() && current.commands.same_channel(&worker.commands)
                });
                let result = if !current {
                    Err(invalid("instance storage changed during launch"))
                } else if schedulers.get(id).is_some_and(|existing| {
                    *existing.subscribe_status().borrow() == SchedulingStatus::Running
                }) {
                    Err(invalid("instance already has a host scheduler"))
                } else {
                    if let Some(previous) = schedulers.remove(id) {
                        let _ = previous.stop().await;
                    }
                    schedulers.insert(id.clone(), controller);
                    let _ = reply.send(Ok(()));
                    continue;
                };
                let _ = controller.stop().await;
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
                    FlowOperation::Create(descriptor, _) => descriptor.address.instance_id.clone(),
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
                        FlowOperation::Create(descriptor, _) => worker
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
                        FlowOperation::Create(descriptor, expected) => {
                            store.create_flow_at(*descriptor, expected.as_ref())
                        }
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
                    Ok(flow) => {
                        FlowWorker::start_owned(flow, Some(host.clone()), services.redactor.clone())
                            .await
                    }
                    Err(error) => Err(error),
                };
                if let Ok(worker) = &result {
                    // Install before acknowledgement, including when the request's receiver was dropped.
                    if let Some(clients) = services.permission_clients.upgrade() {
                        clients.register(
                            worker.address().clone(),
                            &worker.context().permission_broker(),
                        );
                    }
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
