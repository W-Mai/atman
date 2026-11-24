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
#[path = "host_retention.rs"]
mod retention;
pub use retention::{HostOperationToken, KeptFlow, RetentionPending};
#[path = "authority.rs"]
mod authority;
#[path = "handoff.rs"]
mod handoff;
#[path = "handoff_recovery.rs"]
mod handoff_recovery;
#[path = "idle_shutdown.rs"]
mod idle_shutdown;
pub(in crate::flow_runtime) use handoff::{AdmissionPermit, SessionAdmission};
pub use handoff::{
    HandoffBlocker, HandoffFlow, HandoffScheduling, SessionHandoffOutcome, SessionHandoffReceipt,
    SessionOperationPin, SessionOwnerReleased,
};
#[cfg(test)]
#[path = "transient_tests.rs"]
mod transient_tests;

use super::{
    FlowSnapshot, FlowWorker,
    access::InstanceObservation,
    execution::TurnRequest,
    input_preferences::InputPreferences,
    scheduler::{FlowScheduler, PrepareTurn, SchedulingStatus},
};
use crate::event_log::journal::CommitReceipt;
use crate::flow_store::access::{FlowGrant, SessionAccess};
use crate::flow_store::inbox::{Acceptance, DeliveryRequest};
use crate::flow_store::position::FlowPosition;
use crate::flow_store::transient::{TransientBudget, TransientLimits, TransientRetention};
use crate::retained_file::StagingCapacity;
use crate::user_input::preference_authority::{
    BoundPreferenceEdit, PreferenceIntent, PreferenceOutcome, PreferenceRejection,
};
use crate::user_input::preferences::{EffectiveInputPreferences, PreferenceEdit};

use crate::flow_store::{
    FlowDescriptor, FlowInstanceId, FlowRef, RuntimeStore, SessionDescriptor, SessionProjection,
    SessionStore, invalid,
};

type FlowDirectory = Arc<Mutex<HashMap<FlowInstanceId, FlowWorker>>>;

fn install_recovered_preference_model(
    flows: &FlowDirectory,
    store: &crate::flow_store::SessionStore,
    effect: crate::flow_store::PendingPreferenceEffect,
) -> bool {
    if store
        .projection()
        .preference_outcome(&effect.flow, effect.request)
        != PreferenceOutcome::Applied
    {
        return false;
    }
    if let Some(model) = effect.model {
        let installed = flows
            .lock()
            .unwrap()
            .get(&effect.flow)
            .filter(|worker| worker.preference_binding == effect.binding && !worker.is_stopping())
            .cloned();
        if let Some(worker) = installed {
            worker.context().set_current_model(model);
        }
    }
    true
}

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

#[cfg(test)]
type IdleCommitObserver = Box<dyn FnOnce(&RuntimeHost) + Send>;

struct Directory {
    generation: Uuid,
    transient_budget: TransientBudget,
    sessions: Mutex<HashMap<Uuid, SessionHandle>>,
    storage_roots: Mutex<HashMap<Uuid, std::path::PathBuf>>,
    profile: Mutex<Option<ProfileHandle>>,
    changes: watch::Sender<()>,
    services: HostServices,
    stopping: std::sync::atomic::AtomicBool,
    shutdown: tokio::sync::OnceCell<Result<(), String>>,
    controls: control_service::ControlService,
    opening: Arc<tokio::sync::Mutex<()>>,
    fork_staging: Mutex<HashMap<std::path::PathBuf, Arc<StagingCapacity>>>,
    handoffs: Mutex<HashMap<Uuid, SessionHandoffReceipt>>,
    handoff_requests: Mutex<HashMap<Uuid, Uuid>>,
    released_sessions: Mutex<HashMap<Uuid, SessionHandoffReceipt>>,
    acquired_sessions: Mutex<HashMap<Uuid, SessionHandoffReceipt>>,
    authority: Arc<std::sync::OnceLock<crate::peer_authority::PeerAuthority>>,
    peers: std::sync::OnceLock<Arc<dyn super::peer::PeerPort>>,
    invocations: super::peer::InvocationRegistry,
    finalizing: Arc<tokio::sync::Mutex<()>>,
    idle_shutdown_result: Mutex<Option<Result<(), String>>>,
    host_operations: Arc<std::sync::atomic::AtomicUsize>,
    host_operation_changes: watch::Sender<()>,
    #[cfg(test)]
    idle_before_commit: Mutex<Option<IdleCommitObserver>>,
}

impl Default for Directory {
    fn default() -> Self {
        Self {
            generation: Uuid::now_v7(),
            transient_budget: TransientBudget::new(128 * 1024 * 1024)
                .expect("nonzero host retention budget"),
            sessions: Default::default(),
            storage_roots: Default::default(),
            profile: Default::default(),
            changes: watch::channel(()).0,
            services: HostServices::default(),
            stopping: Default::default(),
            shutdown: Default::default(),
            controls: Default::default(),
            opening: Default::default(),
            fork_staging: Default::default(),
            handoffs: Default::default(),
            handoff_requests: Default::default(),
            released_sessions: Default::default(),
            acquired_sessions: Default::default(),
            authority: Default::default(),
            peers: Default::default(),
            invocations: Default::default(),
            finalizing: Default::default(),
            idle_shutdown_result: Default::default(),
            host_operations: Default::default(),
            host_operation_changes: watch::channel(()).0,
            #[cfg(test)]
            idle_before_commit: Default::default(),
        }
    }
}

/// Workers do not keep their owning directory alive through a reference cycle.
#[derive(Clone)]
pub(crate) struct WeakRuntimeHost(Weak<Directory>);

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
    pub(super) fn peer_transfer_retention(&self, transfer: Uuid) -> io::Result<TransientRetention> {
        TransientRetention::new(
            self.directory.transient_budget.clone(),
            self.generation(),
            transfer,
            TransientLimits {
                maximum_bytes: 128 * 1024 * 1024,
                terminal_reserve_bytes: 16 * 1024,
            },
        )
    }

    pub(super) fn peer_transfers_pending(&self) -> bool {
        !self
            .directory
            .invocations
            .captures
            .lock()
            .unwrap()
            .is_empty()
    }
    pub(crate) fn downgrade(&self) -> WeakRuntimeHost {
        WeakRuntimeHost(Arc::downgrade(&self.directory))
    }
    /// Identity of this live host generation, independent of durable journal receipts.
    pub fn generation(&self) -> Uuid {
        self.directory.generation
    }

    /// Observe host directory changes without opening any Session or Flow.
    pub fn subscribe_changes(&self) -> watch::Receiver<()> {
        self.directory.changes.subscribe()
    }

    pub(super) fn transient_retention(&self, flow: &FlowRef) -> io::Result<TransientRetention> {
        self.session(flow.session_id.0)?;
        TransientRetention::new(
            self.directory.transient_budget.clone(),
            self.directory.generation,
            flow.instance_id.0.0,
            TransientLimits::default(),
        )
    }

    /// Release one host-local conversation after fencing its scheduling and execution. Accepted disposal survives disconnection; durable flows cannot enter this path.
    pub async fn discard_transient(
        &self,
        target: FlowRef,
        expected: FlowPosition,
        authorize: impl Fn(&FlowRef) -> io::Result<()> + Send + Sync + 'static,
    ) -> io::Result<()> {
        let host = self.clone();
        tokio::spawn(async move {
            authorize(&target)?;
            let session = host.session(target.session_id.0)?;
            let Some(worker) = session.loaded_flow(&target.instance_id) else {
                return session
                    .confirm_discarded(target.instance_id, expected)
                    .await;
            };
            let _launch = worker.launch_gate.clone().lock_owned().await;
            if worker.is_stopped() {
                return session
                    .confirm_discarded(target.instance_id, expected)
                    .await;
            }
            let stopping = worker.stopping.clone();
            worker
                .request(move |store| {
                    authorize(store.state().address())?;
                    if !matches!(store.cursor(), FlowPosition::Transient { .. })
                        || !store.cursor().same_history(&expected)
                    {
                        return Err(invalid("temporary conversation identity changed"));
                    }
                    store.transition(
                        store.state().revision(),
                        crate::flow_store::FlowTransition::CloseRequested,
                    )?;
                    stopping.store(true, std::sync::atomic::Ordering::Release);
                    Ok(())
                })
                .await?;
            let scheduling = session.stop_scheduling(target.instance_id).await;
            let stopped = worker.clone().stop_execution().await;
            let forgotten = session
                .request(|reply| CatalogCommand::ForgetTransient { worker, reply })
                .await;
            scheduling.and(stopped).and(forgotten)
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Close a confirmed Session's inactive instances through their lifecycle coordinator, then perform strict deletion. Active turns and stale reviews are rejected; accepted cleanup survives caller disconnection.
    pub async fn close_and_delete_session(
        &self,
        data: &std::path::Path,
        request: crate::flow_store::SessionRemoval,
    ) -> anyhow::Result<()> {
        let host = self.clone();
        let data = data.to_owned();
        tokio::spawn(async move {
            let source = data.clone();
            let id = request.session_id;
            if tokio::task::spawn_blocking(move || {
                crate::flow_store::RuntimeStore::inspect_session_removal(&source, id)
            })
            .await??
            .is_some()
            {
                return host.delete_session(&data, request).await;
            }
            let session = match host.session(id) {
                Ok(session) => session,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    let source = data.clone();
                    let reviewed = request.clone();
                    tokio::task::spawn_blocking(move || {
                        crate::flow_store::RuntimeStore::open(&source)?
                            .prepare_session_removal(&reviewed)
                    })
                    .await??;
                    host.open_session(&data, id).await?
                }
                Err(error) => return Err(error.into()),
            };
            let targets = session
                .request(|reply| CatalogCommand::PrepareRemoval {
                    data: data.clone(),
                    request: request.clone(),
                    reply,
                })
                .await?;
            let mut inactive = Vec::with_capacity(targets.len());
            for target in targets {
                let worker = session.open_flow(target).await?;
                let state = worker.state().await?;
                anyhow::ensure!(
                    !matches!(
                        state.state(),
                        atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(_))
                            | atman_rt::InstanceState::Closing { active: Some(_) }
                    ),
                    "flow {} is running; stop it before deleting this session",
                    worker.address().instance_id
                );
                inactive.push((worker, state.revision()));
            }
            let sender = crate::flow_store::inbox::SenderIdentity::Host(Uuid::now_v7());
            for (worker, revision) in inactive {
                let owner = worker.address().clone();
                worker
                    .control(
                        crate::flow_store::control::ControlRequest {
                            id: Uuid::now_v7(),
                            sender: sender.clone(),
                            target: owner.clone(),
                            expected_revision: revision,
                            pause_key: Uuid::now_v7().into(),
                            action: crate::flow_store::control::ControlAction::Close,
                        },
                        move |actual| {
                            if actual == &owner {
                                Ok(())
                            } else {
                                Err(invalid("session removal targets another flow"))
                            }
                        },
                    )
                    .await?;
                worker.finish_close().await?;
            }
            host.delete_session(&data, request).await
        })
        .await?
    }

    /// Remove a confirmed Session, releasing its loaded owners only when every instance is closed or archived. Accepted removal survives caller disconnection; other Sessions keep running.
    pub async fn delete_session(
        &self,
        data: &std::path::Path,
        request: crate::flow_store::SessionRemoval,
    ) -> anyhow::Result<()> {
        let host = self.clone();
        let data = data.to_owned();
        tokio::spawn(async move {
            // Retention waits are per Session and never hold the global opening gate.
            let _captures = match host.session(request.session_id) {
                Ok(session) => Some(session.source_reads.clone().write_owned().await),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            let authority = match host.authority_domain() {
                Some(domain) => Some(Arc::new(
                    domain
                        .acquire(
                            crate::peer_authority::AuthorityPlan::new([
                                (
                                    crate::peer_authority::AuthorityKey::Profile,
                                    crate::peer_authority::AuthorityMode::Shared,
                                ),
                                (
                                    crate::peer_authority::AuthorityKey::Session(
                                        request.session_id,
                                    ),
                                    crate::peer_authority::AuthorityMode::Exclusive,
                                ),
                            ])?,
                            tokio_util::sync::CancellationToken::new(),
                            std::time::Instant::now() + std::time::Duration::from_secs(15),
                        )
                        .await?,
                )),
                None => None,
            };
            let _opening = host.directory.opening.lock().await;
            match host.session(request.session_id) {
                Ok(session) => {
                    let result = session
                        .request_prepared(authority.clone(), |reply| {
                            CatalogCommand::ReleaseClosed {
                                data: data.clone(),
                                request: request.clone(),
                                reply,
                            }
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
            tokio::task::spawn_blocking(move || {
                let _authority = authority;
                RuntimeStore::open(&data)?.delete_session(&request)
            })
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

    pub(crate) fn fork_staging_capacity(
        &self,
        root: &std::path::Path,
    ) -> io::Result<Arc<StagingCapacity>> {
        let key = root.to_owned();
        let mut capacities = self.directory.fork_staging.lock().unwrap();
        if let Some(capacity) = capacities.get(&key) {
            return Ok(capacity.clone());
        }
        let capacity = StagingCapacity::new(
            key.clone(),
            super::source_capture::FORK_STAGING_CAPACITY_BYTES,
        )?;
        capacities.insert(key, capacity.clone());
        Ok(capacity)
    }

    /// Reuse or open an existing Session owner without installing execution or scheduling.
    pub async fn open_session(
        &self,
        data: &std::path::Path,
        id: Uuid,
    ) -> io::Result<SessionHandle> {
        self.acquire_session(data, id, None, None).await
    }

    /// Create or reuse an owner with the same immutable descriptor. Conflicting retries fail.
    pub async fn create_session(
        &self,
        data: &std::path::Path,
        descriptor: SessionDescriptor,
    ) -> io::Result<SessionHandle> {
        self.acquire_session(data, descriptor.id, Some(descriptor), None)
            .await
    }

    async fn acquire_session(
        &self,
        data: &std::path::Path,
        id: Uuid,
        create: Option<SessionDescriptor>,
        handoff: Option<SessionHandoffReceipt>,
    ) -> io::Result<SessionHandle> {
        let authority = match self.authority_domain() {
            Some(domain) => Some(Arc::new(
                domain
                    .acquire(
                        crate::peer_authority::AuthorityPlan::new([(
                            crate::peer_authority::AuthorityKey::Session(id),
                            crate::peer_authority::AuthorityMode::Exclusive,
                        )])?,
                        tokio_util::sync::CancellationToken::new(),
                        std::time::Instant::now() + std::time::Duration::from_secs(10),
                    )
                    .await?,
            )),
            None => None,
        };
        let opening = self.directory.opening.clone().lock_owned().await;
        if handoff.is_none()
            && let Some(receipt) = self.directory.released_sessions.lock().unwrap().get(&id)
        {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                SessionOwnerReleased {
                    session_id: id,
                    source_generation: receipt.source_generation,
                },
            ));
        }
        let host = self.clone();
        let data = data.to_owned();
        let expected = create.clone();
        // The blocking task retains the gate through attachment even if its caller disappears.
        let session = tokio::task::spawn_blocking(move || {
            let _authority = authority;
            let _opening = opening;
            match host.session(id) {
                Ok(session) => {
                    if let Some(receipt) = &handoff
                        && host.directory.acquired_sessions.lock().unwrap().get(&id)
                            != Some(receipt)
                    {
                        return Err(invalid(
                            "Session already has an owner without this handoff installation",
                        ));
                    }
                    return Ok(session);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            if let Some(domain) = host.authority_domain()
                && data.canonicalize()? != domain.data_root()
            {
                return Err(invalid(
                    "Session storage is outside the installed authority domain",
                ));
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
            if let Some(receipt) = &handoff
                && (receipt.session_id != id || store.cursor() != &receipt.catalog)
            {
                return Err(invalid("handoff catalog changed before acquisition"));
            }
            if let Some(receipt) = &handoff {
                let roster: std::collections::HashSet<_> = store
                    .projection()
                    .flows
                    .iter()
                    .map(|flow| flow.id.clone())
                    .collect();
                let incoming: std::collections::HashSet<_> = receipt
                    .flows
                    .iter()
                    .map(|flow| flow.instance_id.clone())
                    .collect();
                if roster != incoming || incoming.len() != receipt.flows.len() {
                    return Err(invalid("handoff Flow roster does not match the catalog"));
                }
                for flow in &receipt.flows {
                    let cursor = flow.cursor.require_durable()?;
                    if cursor.journal_id != flow.instance_id.0.0 {
                        return Err(invalid("handoff Flow cursor belongs to another journal"));
                    }
                    let inspected = store.inspect_flow(&flow.instance_id)?;
                    if inspected.journal.trailing_bytes != 0
                        || inspected.journal.cursor.as_ref() != Some(cursor)
                    {
                        return Err(invalid("handoff Flow changed before acquisition"));
                    }
                }
            }
            if create
                .as_ref()
                .is_some_and(|expected| &store.projection().descriptor != expected)
            {
                return Err(invalid(
                    "session creation conflicts with its recorded descriptor",
                ));
            }
            let session = host.attach_session(store)?;
            if let Some(receipt) = handoff {
                host.directory.released_sessions.lock().unwrap().remove(&id);
                host.directory
                    .acquired_sessions
                    .lock()
                    .unwrap()
                    .insert(id, receipt);
            }
            Ok(session)
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
        let data = store
            .dir()
            .parent()
            .and_then(std::path::Path::parent)
            .and_then(std::path::Path::parent)
            .ok_or_else(|| invalid("session storage has no runtime data root"))?
            .canonicalize()?;
        let mut sessions = self.directory.sessions.lock().unwrap();
        if self
            .authority_domain()
            .is_some_and(|authority| authority.data_root() != data)
        {
            return Err(invalid(
                "Session storage is outside the installed authority domain",
            ));
        }
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
        if self
            .directory
            .storage_roots
            .lock()
            .unwrap()
            .get(&id)
            .is_some_and(|known| *known != data)
        {
            return Err(invalid(
                "session identity is already bound to another storage root",
            ));
        }
        let (commands, receiver) = mpsc::channel(64);
        let flows = FlowDirectory::default();
        let access = Arc::new(RwLock::new(store.projection().access.clone()));
        let (title, title_rx) = watch::channel(store.projection().title.clone());
        let (catalog_state, catalog_view) = watch::channel(SessionSnapshot {
            catalog: store.projection().clone(),
            cursor: store.cursor().clone(),
        });
        let (input_preferences, preferences_view) =
            watch::channel(EffectiveInputPreferences::default());
        let scope_epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let handle = SessionHandle {
            id,
            commands,
            flows: flows.clone(),
            access: access.clone(),
            title: title_rx,
            executor_resolver: Default::default(),
            source_reads: Default::default(),
            admission: Arc::new(SessionAdmission::with_authority(
                id,
                self.generation(),
                self.directory.authority.clone(),
            )),
            handoff: Default::default(),
            question_starts: Default::default(),
            catalog: catalog_view,
            input_preferences: preferences_view,
            cold_scopes: Default::default(),
            scope_epoch: scope_epoch.clone(),
        };
        runtime.spawn(serve_catalog(
            store,
            flows,
            access,
            CatalogPublication {
                title,
                catalog: catalog_state,
                scope_epoch,
                input_preferences,
            },
            WeakRuntimeHost(Arc::downgrade(&self.directory)),
            HostServices {
                redactor,
                ..self.directory.services.clone()
            },
            receiver,
        ));
        sessions.insert(id, handle.clone());
        self.directory
            .storage_roots
            .lock()
            .unwrap()
            .insert(id, data);
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

    pub(super) fn is_transient(&self, address: &FlowRef) -> bool {
        self.directory
            .sessions
            .lock()
            .unwrap()
            .get(&address.session_id.0)
            .and_then(|session| session.loaded_flow(&address.instance_id))
            .is_some_and(|worker| {
                matches!(*worker.commits.borrow(), FlowPosition::Transient { .. })
            })
    }

    /// Authenticate the sender before routing, including retries. Authorize new delivery at its commit boundary.
    pub async fn deliver(
        &self,
        request: DeliveryRequest,
        authorize_new: impl FnOnce(&DeliveryRequest) -> io::Result<()> + Send + 'static,
    ) -> io::Result<Acceptance> {
        self.deliver_prepared(None, request, authorize_new).await
    }

    pub(crate) async fn deliver_with_authority(
        &self,
        authority: Arc<crate::peer_authority::AuthorityGuard>,
        request: DeliveryRequest,
        authorize_new: impl FnOnce(&DeliveryRequest) -> io::Result<()> + Send + 'static,
    ) -> io::Result<Acceptance> {
        self.deliver_prepared(Some(authority), request, authorize_new)
            .await
    }

    async fn deliver_prepared(
        &self,
        authority: Option<Arc<crate::peer_authority::AuthorityGuard>>,
        request: DeliveryRequest,
        authorize_new: impl FnOnce(&DeliveryRequest) -> io::Result<()> + Send + 'static,
    ) -> io::Result<Acceptance> {
        if let crate::flow_store::inbox::SenderIdentity::Flow(source) = &request.sender
            && source != &request.recipient
            && (self.is_transient(source) || self.is_transient(&request.recipient))
        {
            return Err(invalid(
                "cross-flow delivery requires retained conversation identities",
            ));
        }
        let recipient = if authority.is_some() {
            self.session(request.recipient.session_id.0)
                .and_then(|session| {
                    session
                        .loaded_flow(&request.recipient.instance_id)
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::NotFound,
                                "delivery target was not prepared before authority acquisition",
                            )
                        })
                })
        } else {
            self.flow(&request.recipient).await
        };
        match recipient {
            Ok(recipient) => match authority {
                Some(authority) => {
                    recipient
                        .accept_input_with_authority(authority, request, authorize_new)
                        .await
                }
                None => recipient.accept_input(request, authorize_new).await,
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let root = {
                    let roots = self.directory.storage_roots.lock().unwrap();
                    roots
                        .get(&request.recipient.session_id.0)
                        .or_else(|| {
                            if let crate::flow_store::inbox::SenderIdentity::Flow(source) =
                                &request.sender
                            {
                                roots.get(&source.session_id.0)
                            } else {
                                None
                            }
                        })
                        .cloned()
                };
                tokio::task::spawn_blocking(move || {
                    let _authority = authority;
                    if let Some(root) = root {
                        if let Some(accepted) =
                            RuntimeStore::inspect_deleted_delivery(&root, &request)?
                        {
                            return Ok(accepted);
                        }
                    }
                    Err(error)
                })
                .await
                .map_err(io::Error::other)?
            }
            Err(error) => Err(error),
        }
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
                    let _finalizing = directory.finalizing.lock().await;
                    if let Some(result) = directory.idle_shutdown_result.lock().unwrap().clone() {
                        return result;
                    }
                    // Drain previously admitted peer effects before making their validators unavailable.
                    let (fence, _opening) = loop {
                        if directory.authority.get().is_none() {
                            {
                                let _sessions = directory.sessions.lock().unwrap();
                                directory
                                    .stopping
                                    .store(true, std::sync::atomic::Ordering::Release);
                            }
                            break (None, directory.opening.clone().lock_owned().await);
                        }
                        let ids: Vec<_> =
                            directory.sessions.lock().unwrap().keys().copied().collect();
                        let fence = if let Some(domain) = directory.authority.get() {
                            let plan = crate::peer_authority::AuthorityPlan::new(
                                std::iter::once((
                                    crate::peer_authority::AuthorityKey::Profile,
                                    crate::peer_authority::AuthorityMode::Exclusive,
                                ))
                                .chain(ids.iter().map(|id| {
                                    (
                                        crate::peer_authority::AuthorityKey::Session(*id),
                                        crate::peer_authority::AuthorityMode::Exclusive,
                                    )
                                })),
                            )
                            .map_err(|error| error.to_string())?;
                            Some(
                                domain
                                    .acquire(
                                        plan,
                                        tokio_util::sync::CancellationToken::new(),
                                        std::time::Instant::now()
                                            + std::time::Duration::from_secs(15),
                                    )
                                    .await
                                    .map_err(|error| error.to_string())?,
                            )
                        } else {
                            None
                        };
                        let opening = directory.opening.clone().lock_owned().await;
                        {
                            let sessions = directory.sessions.lock().unwrap();
                            if sessions.len() == ids.len()
                                && ids.iter().all(|id| sessions.contains_key(id))
                            {
                                directory
                                    .stopping
                                    .store(true, std::sync::atomic::Ordering::Release);
                                break (fence, opening);
                            }
                        }
                        drop(opening);
                        drop(fence);
                    };
                    drop(fence);
                    directory.changes.send_modify(|_| {});
                    directory.controls.stop().await;
                    let sessions: Vec<_> = directory
                        .sessions
                        .lock()
                        .unwrap()
                        .values()
                        .cloned()
                        .collect();
                    let results = futures::future::join_all(sessions.iter().map(|session| async {
                        match session.request(CatalogCommand::Shutdown).await {
                            Ok(()) => Ok(()),
                            Err(error)
                                if error
                                    .get_ref()
                                    .and_then(|cause| cause.downcast_ref::<SessionOwnerReleased>())
                                    .is_some_and(|released| released.session_id == session.id) =>
                            {
                                Ok(())
                            }
                            Err(error) => Err(format!("session {}: {error}", session.id)),
                        }
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
    commands: mpsc::Sender<CatalogEnvelope>,
    flows: FlowDirectory,
    pub(super) access: Arc<RwLock<SessionAccess>>,
    title: watch::Receiver<Option<String>>,
    executor_resolver: Arc<std::sync::OnceLock<ExecutorResolver>>,
    source_reads: Arc<tokio::sync::RwLock<()>>,
    admission: Arc<SessionAdmission>,
    handoff: Arc<tokio::sync::Mutex<()>>,
    pub(super) question_starts: Arc<super::question::start::QuestionStarts>,
    catalog: watch::Receiver<SessionSnapshot>,
    input_preferences: watch::Receiver<EffectiveInputPreferences>,
    cold_scopes: super::task_scope::ColdScopeCache,
    scope_epoch: Arc<std::sync::atomic::AtomicU64>,
}

struct CatalogEnvelope {
    command: CatalogCommand,
    admission: Option<AdmissionPermit>,
    authority: Option<Arc<crate::peer_authority::AuthorityGuard>>,
}

struct CatalogPublication {
    title: watch::Sender<Option<String>>,
    catalog: watch::Sender<SessionSnapshot>,
    scope_epoch: Arc<std::sync::atomic::AtomicU64>,
    input_preferences: watch::Sender<EffectiveInputPreferences>,
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
    InstallRecoveredHandoff {
        request_id: Uuid,
        source_generation: Uuid,
        stores: Vec<crate::flow_store::FlowStore>,
        terminal: HashMap<FlowInstanceId, HandoffScheduling>,
        reply: oneshot::Sender<io::Result<SessionHandoffReceipt>>,
    },
    RetentionPending(oneshot::Sender<io::Result<bool>>),
    ShutdownIdle {
        authority: Option<Arc<crate::peer_authority::AuthorityGuard>>,
        reply: oneshot::Sender<io::Result<()>>,
    },
    InstallHandoffScheduling {
        receipt: SessionHandoffReceipt,
        reply: oneshot::Sender<io::Result<()>>,
    },
    Handoff {
        request_id: Uuid,
        roster: handoff::PreparedRoster,
        reply: oneshot::Sender<io::Result<SessionHandoffOutcome>>,
    },
    Keep {
        request: retention::KeepRequest,
        reply: oneshot::Sender<io::Result<KeptFlow>>,
    },
    AcknowledgeRetention {
        receipt: crate::flow_store::retention::RetentionReceipt,
        authorize: retention::Authorize,
        reply: oneshot::Sender<io::Result<()>>,
    },
    ResumeRetained {
        request: retention::ResumeRequest,
        reply: oneshot::Sender<io::Result<()>>,
    },
    ConfirmDiscarded {
        id: FlowInstanceId,
        expected: FlowPosition,
        reply: oneshot::Sender<io::Result<()>>,
    },
    ForgetTransient {
        worker: FlowWorker,
        reply: oneshot::Sender<io::Result<()>>,
    },
    PrepareRemoval {
        data: std::path::PathBuf,
        request: crate::flow_store::SessionRemoval,
        reply: oneshot::Sender<io::Result<Vec<FlowInstanceId>>>,
    },
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
    ObserveInput {
        id: FlowInstanceId,
        sender: crate::flow_store::inbox::SenderIdentity,
        request_id: Uuid,
        reply: oneshot::Sender<
            io::Result<Option<crate::flow_store::input_observation::InputObservation>>,
        >,
    },
    ObserveOperatorInput {
        id: FlowInstanceId,
        operator: crate::flow_store::local_operator::LocalOperator,
        request_id: Uuid,
        reply: oneshot::Sender<
            io::Result<Option<crate::flow_store::input_observation::InputObservation>>,
        >,
    },
    ObserveHistory {
        id: FlowInstanceId,
        sender: crate::flow_store::inbox::SenderIdentity,
        request_id: Uuid,
        reply: oneshot::Sender<
            io::Result<Option<crate::flow_store::history_operation::HistoryOperationRecord>>,
        >,
    },
    ObserveOperatorHistory {
        id: FlowInstanceId,
        operator: crate::flow_store::local_operator::LocalOperator,
        request_id: Uuid,
        reply: oneshot::Sender<
            io::Result<Option<crate::flow_store::history_operation::HistoryOperationRecord>>,
        >,
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
    SetInputPreference {
        edit: PreferenceEdit,
        bound: Option<(Uuid, Box<BoundPreferenceEdit>)>,
        reply: oneshot::Sender<io::Result<PreferenceOutcome>>,
    },
    RefreshInputPreferences(oneshot::Sender<io::Result<()>>),
    Snapshot(oneshot::Sender<io::Result<SessionSnapshot>>),
}

impl CatalogCommand {
    fn catalog_write(&self) -> bool {
        matches!(
            self,
            Self::Access { .. }
                | Self::Select { .. }
                | Self::Title { .. }
                | Self::Project { .. }
                | Self::SetInputPreference { .. }
                | Self::RefreshInputPreferences(_)
                | Self::ForgetTransient { .. }
                | Self::InstallHandoffScheduling { .. }
        )
    }
}

fn publish_catalog(
    store: &crate::flow_store::SessionStore,
    state: &watch::Sender<SessionSnapshot>,
) {
    state.send_if_modified(|current| {
        if &current.cursor == store.cursor() {
            return false;
        }
        *current = SessionSnapshot {
            catalog: store.projection().clone(),
            cursor: store.cursor().clone(),
        };
        true
    });
}

pub(super) async fn catalog_authority(
    host: &WeakRuntimeHost,
    session: Uuid,
) -> io::Result<Option<Arc<crate::peer_authority::AuthorityGuard>>> {
    host.upgrade()?
        .session(session)?
        .admission
        .prepare_authority(Default::default(), None)
        .await
}

enum AccessChange {
    Grant(FlowGrant),
    Revoke(Uuid),
}

enum FlowOperation {
    Create(
        Box<FlowDescriptor>,
        Option<CommitReceipt>,
        Option<FlowGrant>,
    ),
    CreateTransient(
        Box<FlowDescriptor>,
        Option<CommitReceipt>,
        Option<crate::storage::ProjectContext>,
    ),
    Open(FlowInstanceId),
}

impl SessionHandle {
    /// Compare the original catalog actor, including release and reacquisition in one host.
    pub fn same_owner(&self, other: &Self) -> bool {
        self.commands.same_channel(&other.commands)
    }
    pub(in crate::flow_runtime) fn scope_registration(&self, id: &FlowInstanceId) -> Option<Uuid> {
        self.catalog
            .borrow()
            .catalog
            .flows
            .iter()
            .find(|flow| &flow.id == id)
            .map(|flow| flow.creation_request)
    }
    pub(in crate::flow_runtime) fn cold_scope_cache(&self) -> super::task_scope::ColdScopeCache {
        self.cold_scopes.clone()
    }
    pub(in crate::flow_runtime) fn scope_epoch(&self) -> u64 {
        self.scope_epoch.load(std::sync::atomic::Ordering::Acquire)
    }
    pub(in crate::flow_runtime) fn scope_worker(&self, id: &FlowInstanceId) -> Option<FlowWorker> {
        self.flows.lock().unwrap().get(id).cloned()
    }
    async fn retention_pending(&self) -> io::Result<bool> {
        self.request(CatalogCommand::RetentionPending).await
    }

    async fn frozen_retention_pending(&self) -> io::Result<bool> {
        let (reply, received) = oneshot::channel();
        self.commands
            .send(CatalogEnvelope {
                command: CatalogCommand::RetentionPending(reply),
                admission: None,
                authority: None,
            })
            .await
            .map_err(|_| invalid("Session owner stopped"))?;
        received
            .await
            .map_err(|_| invalid("Session read did not acknowledge"))?
    }
    /// Compatible authoritative catalog sampling; no writer, recovery or file read is queued.
    pub(crate) async fn snapshot_with_authority(
        &self,
        authority: Arc<crate::peer_authority::AuthorityGuard>,
    ) -> io::Result<SessionSnapshot> {
        self.admission.check_read_authority(&authority)?;
        if self.is_stopped() {
            return Err(invalid("Session owner stopped"));
        }
        Ok(self.catalog.borrow().clone())
    }
    async fn confirm_discarded(
        &self,
        id: FlowInstanceId,
        expected: FlowPosition,
    ) -> io::Result<()> {
        self.request(|reply| CatalogCommand::ConfirmDiscarded {
            id,
            expected,
            reply,
        })
        .await
    }
    pub(super) async fn create_transient_flow(
        &self,
        descriptor: FlowDescriptor,
        expected: Option<CommitReceipt>,
        project: Option<crate::storage::ProjectContext>,
    ) -> io::Result<FlowWorker> {
        self.request(|reply| CatalogCommand::Flow {
            operation: FlowOperation::CreateTransient(Box::new(descriptor), expected, project),
            reply,
        })
        .await
    }

    pub(crate) async fn retain_source(&self) -> io::Result<tokio::sync::OwnedRwLockReadGuard<()>> {
        let guard = self.source_reads.clone().read_owned().await;
        if self.is_stopped() {
            return Err(io::Error::other("source Session owner is unavailable"));
        }
        Ok(guard)
    }

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

    pub fn subscribe_input_preferences(&self) -> watch::Receiver<EffectiveInputPreferences> {
        self.input_preferences.clone()
    }

    /// Refresh the original configuration source when another host may have changed its defaults.
    pub async fn refresh_input_preferences(&self) -> io::Result<()> {
        self.request(CatalogCommand::RefreshInputPreferences).await
    }

    /// Save one explicit operator choice on this Session and its original configuration source.
    pub async fn set_input_preference(&self, edit: PreferenceEdit) -> io::Result<()> {
        self.request(|reply| CatalogCommand::SetInputPreference {
            edit,
            bound: None,
            reply,
        })
        .await
        .map(drop)
    }

    /// Persist one choice on behalf of the local operator and the original loaded Flow.
    pub async fn set_bound_input_preference(
        &self,
        id: FlowInstanceId,
        request: Uuid,
        expected: String,
        edit: PreferenceEdit,
    ) -> io::Result<PreferenceOutcome> {
        let intent = PreferenceIntent {
            flow: id.clone(),
            expected,
            edit,
        };
        if let Some(outcome) = self
            .catalog
            .borrow()
            .catalog
            .previous_preference_attempt(request, &intent)
        {
            return Ok(outcome);
        }
        let Some(worker) = self
            .loaded_flow(&id)
            .filter(|worker| !worker.is_transient() && !worker.is_stopping())
        else {
            return Ok(PreferenceRejection::InvalidTarget.into());
        };
        let providers = if matches!(intent.edit, PreferenceEdit::Model(_)) {
            let services = match worker.project().await {
                Ok(project) => self.executor_for(project).await.ok(),
                Err(_) => None,
            };
            services.map(|executor| executor.providers)
        } else {
            None
        };
        let edit = intent.edit.clone();
        let bound = Some((
            request,
            Box::new(BoundPreferenceEdit {
                worker,
                intent,
                providers,
            }),
        ));
        self.request(|reply| CatalogCommand::SetInputPreference { edit, bound, reply })
            .await
    }

    /// Observe published receipts without entering the write queue or loading a Flow.
    pub fn input_preference_outcome(
        &self,
        flow: &FlowInstanceId,
        request: Uuid,
    ) -> PreferenceOutcome {
        self.catalog
            .borrow()
            .catalog
            .preference_outcome(flow, request)
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
        self.request_prepared(None, make).await
    }

    async fn request_prepared<T>(
        &self,
        authority: Option<Arc<crate::peer_authority::AuthorityGuard>>,
        make: impl FnOnce(oneshot::Sender<io::Result<T>>) -> CatalogCommand,
    ) -> io::Result<T> {
        let (reply, received) = oneshot::channel();
        let admission = self.admission.enter().await?;
        let command = make(reply);
        let authority = if authority.is_some() || command.catalog_write() {
            self.admission
                .prepare_authority(Default::default(), authority)
                .await?
        } else {
            None
        };
        self.commands
            .send(CatalogEnvelope {
                command,
                admission: Some(admission),
                authority,
            })
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
        self.create_flow_with_access(descriptor, expected, None)
            .await
    }

    pub(crate) async fn create_flow_with_access(
        &self,
        descriptor: FlowDescriptor,
        expected: Option<CommitReceipt>,
        grant: Option<FlowGrant>,
    ) -> io::Result<FlowWorker> {
        self.request(|reply| CatalogCommand::Flow {
            operation: FlowOperation::Create(Box::new(descriptor), expected, grant),
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

    pub(crate) async fn observe_operator_input(
        &self,
        id: FlowInstanceId,
        operator: crate::flow_store::local_operator::LocalOperator,
        request_id: Uuid,
    ) -> io::Result<Option<crate::flow_store::input_observation::InputObservation>> {
        if let Some(worker) = self.loaded_flow(&id) {
            return worker.observe_operator_input(operator, request_id).await;
        }
        self.request(|reply| CatalogCommand::ObserveOperatorInput {
            id,
            operator,
            request_id,
            reply,
        })
        .await
    }

    pub(crate) async fn observe_input_request(
        &self,
        id: FlowInstanceId,
        sender: crate::flow_store::inbox::SenderIdentity,
        request_id: Uuid,
    ) -> io::Result<Option<crate::flow_store::input_observation::InputObservation>> {
        if let Some(worker) = self.loaded_flow(&id) {
            return worker
                .request(move |store| store.observe_input_request(&sender, request_id))
                .await;
        }
        self.request(|reply| CatalogCommand::ObserveInput {
            id,
            sender,
            request_id,
            reply,
        })
        .await
    }

    pub(crate) async fn observe_operator_history_operation(
        &self,
        id: FlowInstanceId,
        operator: crate::flow_store::local_operator::LocalOperator,
        request_id: Uuid,
    ) -> io::Result<Option<crate::flow_store::history_operation::HistoryOperationRecord>> {
        if let Some(worker) = self.loaded_flow(&id) {
            return worker
                .operator_history_operation(operator, request_id, |_| Ok(()))
                .await;
        }
        self.request(|reply| CatalogCommand::ObserveOperatorHistory {
            id,
            operator,
            request_id,
            reply,
        })
        .await
    }

    /// Prefer acknowledged owner receipts; unopened flows use registered read-only inspection.
    pub(crate) async fn observe_history_operation(
        &self,
        id: FlowInstanceId,
        sender: crate::flow_store::inbox::SenderIdentity,
        request_id: Uuid,
    ) -> io::Result<Option<crate::flow_store::history_operation::HistoryOperationRecord>> {
        if let Some(worker) = self.loaded_flow(&id) {
            return worker
                .history_operation(sender, request_id, |_| Ok(()))
                .await;
        }
        self.request(|reply| CatalogCommand::ObserveHistory {
            id,
            sender,
            request_id,
            reply,
        })
        .await
    }

    /// Get a live owner without opening a journal or installing execution.
    pub fn loaded_flow(&self, id: &FlowInstanceId) -> Option<FlowWorker> {
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
                .request(|store| InstanceObservation::live(store).map(Some))
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
    publication: CatalogPublication,
    host: WeakRuntimeHost,
    services: HostServices,
    mut commands: mpsc::Receiver<CatalogEnvelope>,
) {
    let CatalogPublication {
        title,
        catalog: catalog_state,
        scope_epoch,
        input_preferences,
    } = publication;
    let input_preferences = InputPreferences::start(store.projection(), input_preferences).await;
    let Ok((returned, _)) = input_preferences.refresh(store, false, |_, _| {}).await else {
        return;
    };
    store = returned;
    publish_catalog(&store, &catalog_state);
    // Catalog lifetime owns observation, independently of execution and frontend handles.
    let mut observers: HashMap<FlowInstanceId, AbortOnDropHandle<()>> = HashMap::new();
    let mut schedulers: HashMap<FlowInstanceId, FlowScheduler> = HashMap::new();
    // Keep discarded creation identities until host shutdown so a delayed retry cannot resurrect them.
    let mut transient_requests: HashMap<FlowInstanceId, Uuid> = HashMap::new();
    let mut discarded: HashMap<FlowInstanceId, FlowPosition> = HashMap::new();
    let mut retentions = retention::CatalogRetention::default();
    let mut installed_handoff = None;
    let mut retirement_tick = tokio::time::interval(std::time::Duration::from_secs(1));
    retirement_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let command = tokio::select! {
            command = commands.recv() => match command { Some(command) => command, None => break },
            _ = retirement_tick.tick(), if retentions.has_retired_memory() => {
                retentions.release_unobserved().await;
                continue;
            }
        };
        let CatalogEnvelope {
            command,
            admission: _admission,
            authority: _authority,
        } = command;
        let shutdown_authority = match &command {
            CatalogCommand::ShutdownIdle { authority, .. } => Some(authority.clone()),
            CatalogCommand::ReleaseClosed { .. } => Some(_authority.clone()),
            _ => None,
        };
        let command = match command {
            CatalogCommand::ShutdownIdle { reply, .. } => CatalogCommand::Shutdown(reply),
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
                publish_catalog(&store, &catalog_state);
                if let Err(error) = result {
                    let _ = reply.send(Err(io::Error::other(format!("{error:#}"))));
                    continue;
                }
                CatalogCommand::Shutdown(reply)
            }
            command => command,
        };
        match command {
            CatalogCommand::InstallRecoveredHandoff {
                request_id,
                source_generation,
                stores,
                terminal,
                reply,
            } => {
                let owner = match host.upgrade() {
                    Ok(owner) => owner,
                    Err(error) => {
                        let _ = reply.send(Err(error));
                        continue;
                    }
                };
                let mut recovered = Vec::with_capacity(stores.len());
                let mut error = None;
                for flow in stores {
                    let preferences = input_preferences.for_flow(store.projection(), &flow);
                    match FlowWorker::start_owned(
                        flow,
                        Some(host.clone()),
                        services.redactor.clone(),
                        preferences,
                    )
                    .await
                    {
                        Ok(worker) => {
                            let id = worker.address().instance_id.clone();
                            match worker.handoff_snapshot().await {
                                Ok((_, cursor, _)) => {
                                    let scheduling = terminal.get(&id).cloned();
                                    let status = match &scheduling {
                                        Some(HandoffScheduling::Stopped) => {
                                            Some(SchedulingStatus::Stopped)
                                        }
                                        Some(HandoffScheduling::Failed { error }) => {
                                            Some(SchedulingStatus::Failed(error.clone()))
                                        }
                                        _ => None,
                                    };
                                    if let Some(status) = status {
                                        schedulers.insert(
                                            id.clone(),
                                            FlowScheduler::observe_terminal(status)
                                                .expect("terminal recovery never executes"),
                                        );
                                    }
                                    recovered.push(HandoffFlow {
                                        instance_id: id.clone(),
                                        cursor,
                                        scheduling,
                                    });
                                    if let Some(clients) = services.permission_clients.upgrade() {
                                        clients.register(
                                            worker.address().clone(),
                                            &worker.context().permission_broker(),
                                        );
                                    }
                                    flows.lock().unwrap().insert(id.clone(), worker.clone());
                                    observers.insert(
                                        id,
                                        super::observation::start(worker, host.clone()),
                                    );
                                }
                                Err(failure) => {
                                    if !worker.is_stopped() {
                                        let _ = worker
                                            .shutdown_prepared(None, _authority.clone())
                                            .await;
                                    }
                                    error = Some(failure);
                                    break;
                                }
                            }
                        }
                        Err(failure) => {
                            error = Some(failure);
                            break;
                        }
                    }
                }
                if let Some(error) = error {
                    for (_, observer) in observers.drain() {
                        observer.abort();
                    }
                    for (_, scheduler) in schedulers.drain() {
                        let _ = scheduler.stop().await;
                    }
                    let partial: Vec<_> = flows
                        .lock()
                        .unwrap()
                        .drain()
                        .map(|(_, worker)| worker)
                        .collect();
                    for worker in partial {
                        if !worker.is_stopped() {
                            let _ = worker.shutdown_prepared(None, _authority.clone()).await;
                        }
                    }
                    let id = store.projection().descriptor.id;
                    commands.close();
                    drop(commands);
                    drop(store);
                    owner.directory.sessions.lock().unwrap().remove(&id);
                    owner.changed();
                    let _ = reply.send(Err(error));
                    return;
                }
                recovered.sort_by_key(|flow| flow.instance_id.0.0);
                let receipt = SessionHandoffReceipt {
                    request_id,
                    source_generation,
                    session_id: store.projection().descriptor.id,
                    catalog: store.cursor().clone(),
                    flows: recovered,
                };
                installed_handoff = Some(request_id);
                owner
                    .directory
                    .acquired_sessions
                    .lock()
                    .unwrap()
                    .insert(receipt.session_id, receipt.clone());
                owner
                    .directory
                    .released_sessions
                    .lock()
                    .unwrap()
                    .remove(&receipt.session_id);
                owner.changed();
                let _ = reply.send(Ok(receipt));
            }
            CatalogCommand::RetentionPending(reply) => {
                let _ = reply.send(Ok(retentions.blocks_handoff()));
            }
            CatalogCommand::InstallHandoffScheduling { receipt, reply } => {
                let result = if let Some(original) = installed_handoff {
                    if original == receipt.request_id {
                        Ok(())
                    } else {
                        Err(invalid(
                            "another handoff was installed on this Session owner",
                        ))
                    }
                } else if receipt.session_id != store.projection().descriptor.id {
                    Err(invalid("handoff scheduling belongs to another Session"))
                } else {
                    for flow in &receipt.flows {
                        if schedulers.contains_key(&flow.instance_id) {
                            continue;
                        }
                        let status = match &flow.scheduling {
                            Some(HandoffScheduling::Stopped) => Some(SchedulingStatus::Stopped),
                            Some(HandoffScheduling::Failed { error }) => {
                                Some(SchedulingStatus::Failed(error.clone()))
                            }
                            _ => None,
                        };
                        if let Some(status) = status {
                            let controller = FlowScheduler::observe_terminal(status)
                                .expect("terminal handoff observation cannot start execution");
                            schedulers.insert(flow.instance_id.clone(), controller);
                        }
                    }
                    installed_handoff = Some(receipt.request_id);
                    Ok(())
                };
                let _ = reply.send(result);
            }
            CatalogCommand::Handoff {
                request_id,
                roster,
                reply,
            } => {
                let owner = match host.upgrade() {
                    Ok(host) => host,
                    Err(error) => {
                        let _ = reply.send(Err(error));
                        continue;
                    }
                };
                let id = store.projection().descriptor.id;
                if roster.catalog != *store.cursor() {
                    let _ = reply.send(Ok(handoff::pending(
                        &owner,
                        id,
                        request_id,
                        vec![HandoffBlocker::Admission],
                    )));
                    continue;
                }
                let session = match owner.session(id) {
                    Ok(session) => session,
                    Err(error) => {
                        let _ = reply.send(Err(error));
                        continue;
                    }
                };
                let retention_pending = retentions.blocks_handoff();
                let mut prepared = match handoff::prepare_release(
                    &owner,
                    &session,
                    &flows,
                    &mut schedulers,
                    retention_pending,
                )
                .await
                {
                    Ok(prepared) => prepared,
                    Err(blockers) => {
                        let _ = reply.send(Ok(handoff::pending(&owner, id, request_id, blockers)));
                        continue;
                    }
                };
                prepared.merge_roster(roster.cold);
                if prepared.flows.len() != store.projection().flows.len() {
                    let restored = prepared.restore_scheduling(&flows, &mut schedulers);
                    let _ = reply.send(restored.map(|()| {
                        handoff::pending(&owner, id, request_id, vec![HandoffBlocker::Admission])
                    }));
                    continue;
                }
                let suspended = match prepared.suspend_workers(&flows).await {
                    Ok(suspended) => suspended,
                    Err(error) => {
                        let error = match prepared.restore_scheduling(&flows, &mut schedulers) {
                            Ok(()) => error,
                            Err(restore) => io::Error::other(format!(
                                "{error}; scheduler restoration: {restore}"
                            )),
                        };
                        let _ = reply.send(Err(error));
                        continue;
                    }
                };
                for (_, observer) in observers.drain() {
                    observer.abort();
                    let _ = observer.await;
                }
                flows.lock().unwrap().clear();
                // Commit has no fallible file operation; all original stores remain owned here.
                let flow_stores: Vec<_> =
                    suspended.into_iter().map(|owner| owner.commit()).collect();
                let receipt = SessionHandoffReceipt {
                    request_id,
                    source_generation: owner.generation(),
                    session_id: id,
                    catalog: store.cursor().clone(),
                    flows: prepared.flows,
                };
                // Fence implicit reopens before actually releasing the original writer.
                owner
                    .directory
                    .released_sessions
                    .lock()
                    .unwrap()
                    .insert(id, receipt.clone());
                commands.close();
                drop(commands);
                drop(flow_stores);
                drop(store);
                owner
                    .directory
                    .handoffs
                    .lock()
                    .unwrap()
                    .insert(request_id, receipt.clone());
                owner
                    .directory
                    .acquired_sessions
                    .lock()
                    .unwrap()
                    .remove(&id);
                owner.directory.sessions.lock().unwrap().remove(&id);
                prepared.freeze.release();
                owner.changed();
                let _ = reply.send(Ok(SessionHandoffOutcome::Released { receipt }));
                return;
            }
            CatalogCommand::Keep { request, reply } => {
                let result = retentions
                    .keep(
                        store,
                        request,
                        retention::CatalogBindings {
                            flows: &flows,
                            access: &access,
                            observers: &mut observers,
                            schedulers: &mut schedulers,
                            transient_requests: &mut transient_requests,
                            host: &host,
                            services: &services,
                        },
                    )
                    .await;
                let (returned, result) = match result {
                    Ok(result) => result,
                    Err(error) => {
                        let _ = reply.send(Err(io::Error::other(error)));
                        return;
                    }
                };
                store = returned;
                publish_catalog(&store, &catalog_state);
                let _ = reply.send(result);
            }
            CatalogCommand::AcknowledgeRetention {
                receipt,
                authorize,
                reply,
            } => {
                let result = retentions.acknowledge(receipt, authorize, &store).await;
                let _ = reply.send(result);
            }
            CatalogCommand::ResumeRetained { request, reply } => {
                let result = retentions
                    .resume_catalog(
                        store,
                        request,
                        retention::CatalogBindings {
                            flows: &flows,
                            access: &access,
                            observers: &mut observers,
                            schedulers: &mut schedulers,
                            transient_requests: &mut transient_requests,
                            host: &host,
                            services: &services,
                        },
                    )
                    .await;
                let (returned, result) = match result {
                    Ok(result) => result,
                    Err(error) => {
                        let _ = reply.send(Err(io::Error::other(error)));
                        return;
                    }
                };
                store = returned;
                publish_catalog(&store, &catalog_state);
                let _ = reply.send(result);
            }
            CatalogCommand::ConfirmDiscarded {
                id,
                expected,
                reply,
            } => {
                let result = match discarded.get(&id) {
                    Some(position) if position.same_history(&expected) => Ok(()),
                    Some(_) => Err(invalid("temporary conversation identity changed")),
                    None => Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "temporary conversation is unavailable",
                    )),
                };
                let _ = reply.send(result);
            }
            CatalogCommand::ForgetTransient { worker, reply } => {
                let id = &worker.address().instance_id;
                let current = flows
                    .lock()
                    .unwrap()
                    .get(id)
                    .is_some_and(|current| current.commands.same_channel(&worker.commands));
                let result = if !transient_requests.contains_key(id) || !worker.is_stopped() {
                    Err(invalid("temporary owner must stop before disposal"))
                } else if !current {
                    Err(invalid("temporary owner was replaced before disposal"))
                } else {
                    discarded.insert(id.clone(), worker.commits.borrow().clone());
                    if let Some(observer) = observers.remove(id) {
                        observer.abort();
                    }
                    flows.lock().unwrap().remove(id);
                    if let Ok(host) = host.upgrade() {
                        host.changed();
                    }
                    Ok(())
                };
                let _ = reply.send(result);
            }
            CatalogCommand::PrepareRemoval {
                data,
                request,
                reply,
            } => {
                let _stage_authority =
                    match catalog_authority(&host, store.projection().descriptor.id).await {
                        Ok(authority) => authority,
                        Err(error) => {
                            let _ = reply.send(Err(error));
                            continue;
                        }
                    };
                let checked = tokio::task::spawn_blocking(move || {
                    let result = store.prepare_removal(&data, &request);
                    (store, result)
                })
                .await;
                let Ok((returned, result)) = checked else {
                    return;
                };
                store = returned;
                publish_catalog(&store, &catalog_state);
                let _ = reply.send(result.map_err(|error| io::Error::other(format!("{error:#}"))));
            }
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
                let idle = shutdown_authority;
                let results = futures::future::join_all(
                    workers
                        .into_iter()
                        .filter(|worker| !worker.is_stopped())
                        .map(|worker| {
                            let idle = idle.clone();
                            async move {
                                let address = worker.address().clone();
                                let stopped = match idle {
                                    Some(authority) => {
                                        worker.shutdown_prepared(None, authority).await.map(drop)
                                    }
                                    None => worker.stop_execution().await,
                                };
                                stopped.map_err(|error| {
                                    format!("flow {}: {error}", address.instance_id)
                                })
                            }
                        }),
                )
                .await;
                errors.extend(results.into_iter().filter_map(Result::err));
                retentions.shutdown().await;
                // Release the catalog owner before acknowledging that another host can open it.
                drop(store);
                let _ = reply.send(if errors.is_empty() {
                    Ok(())
                } else {
                    Err(io::Error::other(errors.join("; ")))
                });
                return;
            }
            CatalogCommand::ShutdownIdle { .. } => {
                unreachable!("idle shutdown normalized before dispatch")
            }
            CatalogCommand::Describe { id, reply } => {
                let existing = flows
                    .lock()
                    .unwrap()
                    .get(&id)
                    .filter(|worker| !worker.is_stopped())
                    .cloned();
                let result = if let Some(worker) = existing {
                    worker.request(InstanceObservation::live).await
                } else {
                    let returned = tokio::task::spawn_blocking(move || {
                        let result = store.inspect_flow(&id).map(InstanceObservation::recorded);
                        (store, result)
                    })
                    .await;
                    let Ok((returned, result)) = returned else {
                        return;
                    };
                    store = returned;
                    publish_catalog(&store, &catalog_state);
                    result
                };
                let _ = reply.send(result);
            }
            CatalogCommand::ObserveOperatorInput {
                id,
                operator,
                request_id,
                reply,
            } => {
                let existing = flows
                    .lock()
                    .unwrap()
                    .get(&id)
                    .filter(|worker| !worker.is_stopped())
                    .cloned();
                let result = if let Some(worker) = existing {
                    worker.observe_operator_input(operator, request_id).await
                } else {
                    let returned = tokio::task::spawn_blocking(move || {
                        let result = store.inspect_operator_input(&id, operator, request_id);
                        (store, result)
                    })
                    .await;
                    let Ok((returned, result)) = returned else {
                        return;
                    };
                    store = returned;
                    publish_catalog(&store, &catalog_state);
                    result
                };
                let _ = reply.send(result);
            }
            CatalogCommand::ObserveInput {
                id,
                sender,
                request_id,
                reply,
            } => {
                let existing = flows
                    .lock()
                    .unwrap()
                    .get(&id)
                    .filter(|worker| !worker.is_stopped())
                    .cloned();
                let result = if let Some(worker) = existing {
                    worker
                        .request(move |store| store.observe_input_request(&sender, request_id))
                        .await
                } else {
                    let returned = tokio::task::spawn_blocking(move || {
                        let result = store.inspect_input_request(&id, &sender, request_id);
                        (store, result)
                    })
                    .await;
                    let Ok((returned, result)) = returned else {
                        return;
                    };
                    store = returned;
                    publish_catalog(&store, &catalog_state);
                    result
                };
                let _ = reply.send(result);
            }
            CatalogCommand::ObserveOperatorHistory {
                id,
                operator,
                request_id,
                reply,
            } => {
                let existing = flows
                    .lock()
                    .unwrap()
                    .get(&id)
                    .filter(|worker| !worker.is_stopped())
                    .cloned();
                let result = if let Some(worker) = existing {
                    worker
                        .operator_history_operation(operator, request_id, |_| Ok(()))
                        .await
                } else {
                    let returned = tokio::task::spawn_blocking(move || {
                        let result =
                            store.inspect_operator_history_operation(&id, operator, request_id);
                        (store, result)
                    })
                    .await;
                    let Ok((returned, result)) = returned else {
                        return;
                    };
                    store = returned;
                    publish_catalog(&store, &catalog_state);
                    result
                };
                let _ = reply.send(result);
            }
            CatalogCommand::ObserveHistory {
                id,
                sender,
                request_id,
                reply,
            } => {
                let existing = flows
                    .lock()
                    .unwrap()
                    .get(&id)
                    .filter(|worker| !worker.is_stopped())
                    .cloned();
                let result = if let Some(worker) = existing {
                    worker
                        .history_operation(sender, request_id, |_| Ok(()))
                        .await
                } else {
                    let returned = tokio::task::spawn_blocking(move || {
                        let result = store.inspect_history_operation(&id, &sender, request_id);
                        (store, result)
                    })
                    .await;
                    let Ok((returned, result)) = returned else {
                        return;
                    };
                    store = returned;
                    publish_catalog(&store, &catalog_state);
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
                if retentions.blocks_scheduling(&store, &id) {
                    let _ = reply.send(Err(invalid("retained conversation requires an explicit new submission before scheduling")));
                    continue;
                }
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
                if retentions.blocks_scheduling(&store, id) {
                    let _ = controller.stop().await;
                    let _ = reply.send(Err(invalid("retained conversation requires an explicit new submission before scheduling")));
                    continue;
                }
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
                publish_catalog(&store, &catalog_state);
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
            CatalogCommand::RefreshInputPreferences(reply) => {
                let directory = flows.clone();
                let Ok((returned, result)) = input_preferences
                    .refresh(store, true, move |store, effect| {
                        install_recovered_preference_model(&directory, store, effect);
                    })
                    .await
                else {
                    return;
                };
                store = returned;
                publish_catalog(&store, &catalog_state);
                let _ = reply.send(result);
            }
            CatalogCommand::SetInputPreference { edit, bound, reply } => {
                let config = match &input_preferences.config {
                    Ok(Some(config)) => config.clone(),
                    Ok(None) => {
                        let _ = reply.send(if bound.is_some() {
                            Ok(PreferenceRejection::InvalidTarget.into())
                        } else {
                            Err(invalid(
                                "Session has no recorded input configuration source",
                            ))
                        });
                        continue;
                    }
                    Err(error) => {
                        let _ = reply.send(if bound.is_some() {
                            Ok(PreferenceRejection::Unavailable.into())
                        } else {
                            Err(io::Error::other(error.clone()))
                        });
                        continue;
                    }
                };
                let directory = flows.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let before = store.pending_preference_effect();
                    if let Err(error) = store.reconcile_owned_catalog() {
                        return (store, Err(error), None);
                    }
                    let recovering = before.or_else(|| store.pending_preference_effect());
                    let result = if let Some((request, bound)) = bound {
                        let id = &bound.intent.flow;
                        let catalog = store.projection();
                        if let Some(outcome) =
                            catalog.previous_preference_attempt(request, &bound.intent)
                        {
                            Ok((outcome, None, None))
                        } else if directory.lock().unwrap().get(id).is_none_or(|current| {
                            current.preference_binding != bound.worker.preference_binding
                                || current.is_stopping()
                                || current.is_stopped()
                        }) || catalog.archived_flows.contains(id)
                            || catalog
                                .flows
                                .iter()
                                .find(|row| &row.id == id)
                                .is_none_or(|row| row.retention.is_some())
                        {
                            Ok((PreferenceRejection::InvalidTarget.into(), None, None))
                        } else {
                            store
                                .set_bound_input_preference(&config, request, &bound)
                                .map(|(outcome, settings)| {
                                    let model_context = matches!(edit, PreferenceEdit::Model(_))
                                        .then(|| bound.worker.context());
                                    (outcome, settings, model_context)
                                })
                        }
                    } else {
                        store
                            .set_input_preference(&config, edit)
                            .map(|settings| (PreferenceOutcome::Applied, Some(settings), None))
                    };
                    let recovered = recovering.and_then(|effect| {
                        if !install_recovered_preference_model(&directory, &store, effect) {
                            return None;
                        }
                        let (reasoning, trust) = config.input_defaults().ok()?;
                        Some(
                            store
                                .projection()
                                .input_preferences
                                .resolve(reasoning, trust),
                        )
                    });
                    let result = result.map(|(outcome, settings, model_context)| {
                        if let Some(context) = model_context
                            && let Some(model) = settings
                                .as_ref()
                                .and_then(|settings| settings.model.as_ref())
                        {
                            context.set_current_model(model.clone());
                        }
                        (outcome, settings)
                    });
                    (store, result, recovered)
                })
                .await;
                let Ok((returned, result, recovered)) = result else {
                    return;
                };
                store = returned;
                if let Some(settings) = recovered {
                    input_preferences.publish(settings);
                }
                let result = result.map(|(outcome, settings)| {
                    if let Some(settings) = settings {
                        input_preferences.publish(settings);
                    }
                    outcome
                });
                publish_catalog(&store, &catalog_state);
                let _ = reply.send(result);
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
                publish_catalog(&store, &catalog_state);
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
                publish_catalog(&store, &catalog_state);
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
                publish_catalog(&store, &catalog_state);
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
                let _stage_authority =
                    match catalog_authority(&host, store.projection().descriptor.id).await {
                        Ok(authority) => authority,
                        Err(error) => {
                            let _ = reply.send(Err(error));
                            continue;
                        }
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
                publish_catalog(&store, &catalog_state);
                let _ = reply.send(result);
            }
            CatalogCommand::Flow { operation, reply } => {
                let id = match &operation {
                    FlowOperation::Create(descriptor, _, _)
                    | FlowOperation::CreateTransient(descriptor, _, _) => {
                        descriptor.address.instance_id.clone()
                    }
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
                        FlowOperation::Create(_, _, _) if transient_requests.contains_key(&id) => {
                            Err(invalid(
                                "temporary conversation requires explicit retention before durable creation",
                            ))
                        }
                        FlowOperation::CreateTransient(_, _, _)
                            if !transient_requests.contains_key(&id) =>
                        {
                            Err(invalid("flow identity already belongs to durable history"))
                        }
                        FlowOperation::CreateTransient(descriptor, _, project) => worker
                            .request(move |store| {
                                if store.descriptor() != descriptor.as_ref()
                                    || store.project() != project.as_ref()
                                {
                                    return Err(invalid(
                                        "temporary creation payload or project changed",
                                    ));
                                }
                                Ok(())
                            })
                            .await
                            .map(|()| worker.clone()),
                        FlowOperation::Create(descriptor, _, _) => worker
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
                let transient_request = match &operation {
                    FlowOperation::CreateTransient(descriptor, _, _) => {
                        if transient_requests.contains_key(&id)
                            || transient_requests
                                .values()
                                .any(|request| *request == descriptor.creation_request)
                        {
                            let _ = reply.send(Err(invalid(
                                "temporary creation was disposed or belongs to another flow",
                            )));
                            continue;
                        }
                        Some(descriptor.creation_request)
                    }
                    _ => None,
                };
                if let Some(controller) = schedulers.remove(&id) {
                    let _ = controller.stop().await;
                }
                let _stage_authority =
                    match catalog_authority(&host, store.projection().descriptor.id).await {
                        Ok(authority) => authority,
                        Err(error) => {
                            let _ = reply.send(Err(error));
                            continue;
                        }
                    };
                scope_epoch.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                flows.lock().unwrap().remove(&id);
                let creating_host = host.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let result = match operation {
                        FlowOperation::Create(descriptor, expected, grant) => {
                            store.create_flow_with_access(*descriptor, expected.as_ref(), grant)
                        }
                        FlowOperation::CreateTransient(descriptor, expected, project) => {
                            creating_host
                                .upgrade()
                                .and_then(|host| host.transient_retention(&descriptor.address))
                                .and_then(|retention| {
                                    store.create_transient_flow_in(
                                        *descriptor,
                                        expected.as_ref(),
                                        retention,
                                        project,
                                    )
                                })
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
                publish_catalog(&store, &catalog_state);
                let result = match result {
                    Ok(flow) => {
                        *access.write().unwrap() = store.projection().access.clone();
                        let preferences = input_preferences.for_flow(store.projection(), &flow);
                        FlowWorker::start_owned(
                            flow,
                            Some(host.clone()),
                            services.redactor.clone(),
                            preferences,
                        )
                        .await
                    }
                    Err(error) => Err(error),
                };
                if let Ok(worker) = &result {
                    if let Some(request) = transient_request {
                        transient_requests.insert(id.clone(), request);
                    }
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
