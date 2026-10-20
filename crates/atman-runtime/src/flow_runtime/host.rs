//! Host-owned session catalogs and canonical handles for persistent flow instances.

use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex, RwLock, Weak},
};

use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::task::AbortOnDropHandle;
use uuid::Uuid;

#[path = "management.rs"]
mod management;
pub use management::{ProfileHandle, ProfileView};
#[path = "control_service.rs"]
mod control_service;
pub use control_service::{ControlServiceIssue, ControlServiceProgress};
#[path = "execution_services.rs"]
mod execution_services;
pub use execution_services::ExecutorResolver;

use super::{
    FlowSnapshot, FlowWorker,
    access::InstanceObservation,
    execution::TurnRequest,
    scheduler::{FlowScheduler, PrepareTurn, SchedulingStatus},
};
use crate::event_log::journal::CommitReceipt;
use crate::flow_store::access::{FlowGrant, SessionAccess};
use crate::flow_store::inbox::{Acceptance, DeliveryRequest};

use crate::flow_store::{
    FlowDescriptor, FlowInstanceId, FlowRef, RuntimeStore, SessionDescriptor, SessionProjection,
    SessionStore, invalid,
};

type FlowDirectory = Arc<Mutex<HashMap<FlowInstanceId, FlowWorker>>>;

/// Resolve diagnostic redaction from immutable Session configuration before attaching its owner.
pub type SessionRedactor = Arc<
    dyn Fn(&SessionDescriptor) -> io::Result<Option<Arc<crate::redact::Redactor>>> + Send + Sync,
>;

/// Services installed before any instance opens. Approval endpoint ownership remains with the frontend or transport, independently of retained execution handles.
#[derive(Clone, Default)]
pub struct HostServices {
    pub redactor: Option<Arc<crate::redact::Redactor>>,
    pub session_redactor: Option<SessionRedactor>,
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
    profile: Mutex<Option<ProfileHandle>>,
    changes: watch::Sender<()>,
    services: HostServices,
    stopping: std::sync::atomic::AtomicBool,
    shutdown: tokio::sync::OnceCell<Result<(), String>>,
    controls: control_service::ControlService,
    opening: Arc<tokio::sync::Mutex<()>>,
}

impl Default for Directory {
    fn default() -> Self {
        Self {
            sessions: Default::default(),
            profile: Default::default(),
            changes: watch::channel(()).0,
            services: HostServices::default(),
            stopping: Default::default(),
            shutdown: Default::default(),
            controls: Default::default(),
            opening: Default::default(),
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
    /// Remove a confirmed Session, releasing its loaded owners only when every instance is closed or archived. Accepted removal survives caller disconnection; other Sessions keep running.
    pub async fn delete_session(
        &self,
        data: &std::path::Path,
        request: crate::flow_store::SessionRemoval,
    ) -> anyhow::Result<()> {
        let host = self.clone();
        let data = data.to_owned();
        tokio::spawn(async move {
            let _opening = host.directory.opening.lock().await;
            match host.session(request.session_id) {
                Ok(session) => {
                    let result = session
                        .request(|reply| CatalogCommand::ReleaseClosed {
                            data: data.clone(),
                            request: request.clone(),
                            reply,
                        })
                        .await;
                    if session.is_stopped() {
                        host.directory
                            .sessions
                            .lock()
                            .unwrap()
                            .remove(&request.session_id);
                        host.changed();
                    }
                    result?;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            tokio::task::spawn_blocking(move || RuntimeStore::open(&data)?.delete_session(&request))
                .await?
        })
        .await?
    }

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

    /// Reuse or open an existing Session owner without installing execution or scheduling.
    pub async fn open_session(
        &self,
        data: &std::path::Path,
        id: Uuid,
    ) -> io::Result<SessionHandle> {
        self.acquire_session(data, id, None).await
    }

    /// Create or reuse an owner with the same immutable descriptor. Conflicting retries fail.
    pub async fn create_session(
        &self,
        data: &std::path::Path,
        descriptor: SessionDescriptor,
    ) -> io::Result<SessionHandle> {
        self.acquire_session(data, descriptor.id, Some(descriptor))
            .await
    }

    async fn acquire_session(
        &self,
        data: &std::path::Path,
        id: Uuid,
        create: Option<SessionDescriptor>,
    ) -> io::Result<SessionHandle> {
        let opening = self.directory.opening.clone().lock_owned().await;
        let host = self.clone();
        let data = data.to_owned();
        let expected = create.clone();
        // The blocking task retains the gate through attachment even if its caller disappears.
        let session = tokio::task::spawn_blocking(move || {
            let _opening = opening;
            match host.session(id) {
                Ok(session) => return Ok(session),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            let root = RuntimeStore::open(&data)?;
            let store = match create.clone() {
                Some(descriptor) => match root.create_session(descriptor) {
                    Ok(store) => store,
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        root.open_session(id)?
                    }
                    Err(error) => return Err(error),
                },
                None => root.open_session(id)?,
            };
            if create
                .as_ref()
                .is_some_and(|expected| &store.projection().descriptor != expected)
            {
                return Err(invalid(
                    "session creation conflicts with its recorded descriptor",
                ));
            }
            host.attach_session(store)
        })
        .await
        .map_err(io::Error::other)??;
        if let Some(expected) = expected {
            if session.snapshot().await?.catalog.descriptor != expected {
                return Err(invalid(
                    "session creation conflicts with its recorded descriptor",
                ));
            }
        }
        Ok(session)
    }

    /// Attach an exclusively owned catalog without opening or starting its recorded flows.
    pub fn attach_session(&self, store: SessionStore) -> io::Result<SessionHandle> {
        let redactor = match &self.directory.services.session_redactor {
            Some(resolve) => resolve(&store.projection().descriptor)?,
            None => self.directory.services.redactor.clone(),
        };
        self.attach_session_with_redactor(store, redactor)
    }

    /// Bind a Session's diagnostic policy independently of other Session owners.
    pub fn attach_session_with_redactor(
        &self,
        store: SessionStore,
        redactor: Option<Arc<crate::redact::Redactor>>,
    ) -> io::Result<SessionHandle> {
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
        let (title, title_rx) = watch::channel(store.projection().title.clone());
        let handle = SessionHandle {
            id,
            commands,
            flows: flows.clone(),
            access: access.clone(),
            title: title_rx,
            executor_resolver: Default::default(),
        };
        runtime.spawn(serve_catalog(
            store,
            flows,
            access,
            title,
            WeakRuntimeHost(Arc::downgrade(&self.directory)),
            HostServices {
                redactor,
                ..self.directory.services.clone()
            },
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
                    {
                        let _sessions = directory.sessions.lock().unwrap();
                        directory
                            .stopping
                            .store(true, std::sync::atomic::Ordering::Release);
                    }
                    directory.changes.send_modify(|_| {});
                    directory.controls.stop().await;
                    // An in-flight open must finish and release its writer before shutdown returns.
                    let _opening = directory.opening.lock().await;
                    let sessions: Vec<_> = directory
                        .sessions
                        .lock()
                        .unwrap()
                        .values()
                        .cloned()
                        .collect();
                    let results = futures::future::join_all(sessions.iter().map(|session| async {
                        session
                            .request(CatalogCommand::Shutdown)
                            .await
                            .map_err(|error| format!("session {}: {error}", session.id))
                    }))
                    .await;
                    directory.sessions.lock().unwrap().clear();
                    let mut errors: Vec<_> = results.into_iter().filter_map(Result::err).collect();
                    let profile = directory.profile.lock().unwrap().take();
                    if let Some(profile) = profile {
                        if let Err(error) = profile.shutdown().await {
                            errors.push(format!("profile: {error}"));
                        }
                    }
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
    title: watch::Receiver<Option<String>>,
    executor_resolver: Arc<std::sync::OnceLock<ExecutorResolver>>,
}

#[derive(Clone, Debug)]
pub struct SessionSnapshot {
    pub catalog: SessionProjection,
    pub cursor: CommitReceipt,
}

/// Catalog and acknowledged states of already loaded instances. Observation never opens an inactive journal, recovers a turn, or installs scheduling. Instance reads are independent, not a cross-flow transaction; conversation and completed output bodies are not copied.
#[derive(Clone, Debug)]
pub struct SessionObservation {
    pub snapshot: SessionSnapshot,
    pub loaded: Vec<super::goal::GoalView>,
}

enum CatalogCommand {
    ReleaseClosed {
        data: std::path::PathBuf,
        request: crate::flow_store::SessionRemoval,
        reply: oneshot::Sender<io::Result<()>>,
    },
    Shutdown(oneshot::Sender<io::Result<()>>),
    Describe {
        id: FlowInstanceId,
        reply: oneshot::Sender<io::Result<InstanceObservation>>,
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
        expected: CommitReceipt,
        reply: oneshot::Sender<io::Result<CommitReceipt>>,
    },
    Archive {
        id: FlowInstanceId,
        archived: bool,
        expected: CommitReceipt,
        reply: oneshot::Sender<io::Result<CommitReceipt>>,
    },
    Title {
        title: Option<String>,
        expected: CommitReceipt,
        reply: oneshot::Sender<io::Result<CommitReceipt>>,
    },
    Project {
        project: Option<crate::storage::ProjectContext>,
        expected: CommitReceipt,
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

    /// Subscribe to committed Session display metadata without opening or controlling a flow.
    pub fn subscribe_title(&self) -> watch::Receiver<Option<String>> {
        self.title.clone()
    }

    /// Trusted host mutation. Accepted catalog work continues if its caller disconnects.
    pub async fn set_title(
        &self,
        expected: CommitReceipt,
        title: Option<String>,
    ) -> io::Result<CommitReceipt> {
        self.request(|reply| CatalogCommand::Title {
            title,
            expected,
            reply,
        })
        .await
    }

    /// Trusted host mutation of container membership. Captured instance bindings and execution remain unchanged.
    pub async fn set_project(
        &self,
        expected: CommitReceipt,
        project: Option<crate::storage::ProjectContext>,
    ) -> io::Result<CommitReceipt> {
        self.request(|reply| CatalogCommand::Project {
            project,
            expected,
            reply,
        })
        .await
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

    pub async fn observe(&self) -> io::Result<SessionObservation> {
        let snapshot = self.snapshot().await?;
        let workers: Vec<_> = {
            let flows = self.flows.lock().unwrap();
            snapshot
                .catalog
                .flows
                .iter()
                .filter_map(|row| flows.get(&row.id))
                .filter(|worker| !worker.is_stopped())
                .cloned()
                .collect()
        };
        let mut loaded = Vec::with_capacity(workers.len());
        for worker in workers {
            loaded.push(worker.goal().await?);
        }
        Ok(SessionObservation { snapshot, loaded })
    }

    /// Read live process sources without opening a flow journal or installing scheduling. An unloaded recorded instance has no live resources in this host; an unknown instance is rejected.
    pub async fn process_watch_sources(
        &self,
        id: FlowInstanceId,
    ) -> io::Result<Vec<super::ProcessWatchSource>> {
        let snapshot = self.snapshot().await?;
        if !snapshot.catalog.flows.iter().any(|flow| flow.id == id) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "flow is not in the session catalog",
            ));
        }
        let worker = self
            .flows
            .lock()
            .unwrap()
            .get(&id)
            .filter(|worker| !worker.is_stopped())
            .cloned();
        Ok(worker.map_or_else(Vec::new, |worker| worker.process_watch_sources()))
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

    /// Read a registered instance without retaining its conversation or installing observers. Live writers provide acknowledged state; inactive journals are inspected without recovery or mutation.
    pub async fn describe_flow(&self, id: FlowInstanceId) -> io::Result<InstanceObservation> {
        if let Some(observation) = self.describe_loaded_flow(&id).await? {
            return Ok(observation);
        }
        self.request(|reply| CatalogCommand::Describe { id, reply })
            .await
    }

    /// Get a live owner without opening a journal or installing execution.
    pub(crate) fn loaded_flow(&self, id: &FlowInstanceId) -> Option<FlowWorker> {
        if self.is_stopped() {
            return None;
        }
        self.flows
            .lock()
            .unwrap()
            .get(id)
            .filter(|worker| !worker.is_stopped())
            .cloned()
    }

    /// Observe only an existing writer; detached records remain the caller's snapshot.
    pub(crate) async fn describe_loaded_flow(
        &self,
        id: &FlowInstanceId,
    ) -> io::Result<Option<InstanceObservation>> {
        let existing = self.loaded_flow(id);
        if let Some(worker) = existing {
            return worker
                .request(|store| {
                    Ok(Some(InstanceObservation {
                        entry: store.descriptor().content.title().to_owned(),
                        state: store.state().clone(),
                        loaded: true,
                        trailing_bytes: 0,
                        activity: store.activity().cloned(),
                    }))
                })
                .await;
        }
        Ok(None)
    }

    /// Change only discovery metadata through the catalog owner. Archiving requires a closed instance; restoring visibility never restarts it.
    pub async fn set_flow_archived(
        &self,
        expected: CommitReceipt,
        id: FlowInstanceId,
        archived: bool,
    ) -> io::Result<CommitReceipt> {
        self.request(|reply| CatalogCommand::Archive {
            id,
            archived,
            expected,
            reply,
        })
        .await
    }

    /// Change only the default view binding; this does not start, pause, or privilege a flow.
    pub async fn select_default(
        &self,
        expected: CommitReceipt,
        id: FlowInstanceId,
    ) -> io::Result<CommitReceipt> {
        self.request(|reply| CatalogCommand::Select {
            id,
            expected,
            reply,
        })
        .await
    }
}

async fn serve_catalog(
    mut store: SessionStore,
    flows: FlowDirectory,
    access: Arc<RwLock<SessionAccess>>,
    title: watch::Sender<Option<String>>,
    host: WeakRuntimeHost,
    services: HostServices,
    mut commands: mpsc::Receiver<CatalogCommand>,
) {
    // Catalog lifetime owns observation, independently of execution and frontend handles.
    let mut observers: HashMap<FlowInstanceId, AbortOnDropHandle<()>> = HashMap::new();
    let mut schedulers: HashMap<FlowInstanceId, FlowScheduler> = HashMap::new();
    while let Some(command) = commands.recv().await {
        let command = match command {
            CatalogCommand::ReleaseClosed {
                data,
                request,
                reply,
            } => {
                // Keep the catalog serialized while full replay runs off the async worker.
                let checked = tokio::task::spawn_blocking(move || {
                    let result = store.validate_removal(&data, &request);
                    (store, result)
                })
                .await;
                let (returned, result) = match checked {
                    Ok(checked) => checked,
                    Err(error) => {
                        let _ = reply.send(Err(io::Error::other(error)));
                        return;
                    }
                };
                store = returned;
                if let Err(error) = result {
                    let _ = reply.send(Err(io::Error::other(format!("{error:#}"))));
                    continue;
                }
                CatalogCommand::Shutdown(reply)
            }
            command => command,
        };
        match command {
            CatalogCommand::ReleaseClosed { .. } => {
                unreachable!("release validated before dispatch")
            }
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
                            Ok(InstanceObservation {
                                entry: store.descriptor().content.title().to_owned(),
                                state: store.state().clone(),
                                loaded: true,
                                trailing_bytes: 0,
                                activity: store.activity().cloned(),
                            })
                        })
                        .await
                } else {
                    let returned = tokio::task::spawn_blocking(move || {
                        let result = store.inspect_flow(&id).map(|flow| InstanceObservation {
                            entry: flow.descriptor.content.title().to_owned(),
                            state: flow.state,
                            loaded: false,
                            trailing_bytes: flow.journal.trailing_bytes,
                            activity: flow.activity,
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
            CatalogCommand::Title {
                title: next,
                expected,
                reply,
            } => {
                let result = tokio::task::spawn_blocking(move || {
                    let result = store.set_title(&expected, next);
                    (store, result)
                })
                .await;
                let Ok((returned, result)) = result else {
                    return;
                };
                store = returned;
                if result.is_ok() {
                    title.send_if_modified(|current| {
                        if current == &store.projection().title {
                            return false;
                        }
                        *current = store.projection().title.clone();
                        true
                    });
                }
                let _ = reply.send(result);
            }
            CatalogCommand::Select {
                id,
                expected,
                reply,
            } => {
                let result = tokio::task::spawn_blocking(move || {
                    let result = store.select_default(&expected, id);
                    (store, result)
                })
                .await;
                let Ok((returned, result)) = result else {
                    return;
                };
                store = returned;
                let _ = reply.send(result);
            }
            CatalogCommand::Project {
                project,
                expected,
                reply,
            } => {
                let result = tokio::task::spawn_blocking(move || {
                    let result = store.set_project(&expected, project);
                    (store, result)
                })
                .await;
                let Ok((returned, result)) = result else {
                    return;
                };
                store = returned;
                let _ = reply.send(result);
            }
            CatalogCommand::Archive {
                id,
                archived,
                expected,
                reply,
            } => {
                let writer = flows
                    .lock()
                    .unwrap()
                    .get(&id)
                    .filter(|worker| !worker.is_stopped())
                    .cloned();
                let observed = if archived {
                    match writer {
                        Some(writer) => match writer.state().await {
                            Ok(state) => Some(state),
                            Err(error) => {
                                let _ = reply.send(Err(error));
                                continue;
                            }
                        },
                        None => None,
                    }
                } else {
                    None
                };
                let result = tokio::task::spawn_blocking(move || {
                    let result =
                        store.set_flow_archived_observed(&expected, id, archived, observed);
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

#[cfg(test)]
mod removal_tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn accepted_removal_survives_disconnection_and_serializes_host_shutdown() {
        let data = tempfile::tempdir().unwrap();
        let host = RuntimeHost::default();
        let id = Uuid::now_v7();
        let session = host
            .create_session(
                data.path(),
                SessionDescriptor {
                    id,
                    title: "Removal".into(),
                    project: None,
                    configuration: Default::default(),
                },
            )
            .await
            .unwrap();
        let request = crate::flow_store::SessionRemoval {
            session_id: id,
            expected: session.snapshot().await.unwrap().cursor,
        };
        let catalog = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(data.path().join("runtime-v1/.catalog.lock"))
            .unwrap();
        fs2::FileExt::lock_exclusive(&catalog).unwrap();
        let mut removal = Box::pin(host.delete_session(data.path(), request.clone()));
        assert!(futures::poll!(removal.as_mut()).is_pending());
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while host.directory.opening.try_lock().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(removal);
        let closing = {
            let host = host.clone();
            tokio::spawn(async move { host.shutdown().await })
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !host
                .directory
                .stopping
                .load(std::sync::atomic::Ordering::Acquire)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!closing.is_finished());
        fs2::FileExt::unlock(&catalog).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), closing)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(session.snapshot().await.is_err());
        assert!(host.sessions().is_empty());
        assert_eq!(
            RuntimeStore::inspect_session_removal(data.path(), id).unwrap(),
            Some(request)
        );
        assert!(
            !data
                .path()
                .join("runtime-v1/sessions")
                .join(id.to_string())
                .exists()
        );
    }
}
