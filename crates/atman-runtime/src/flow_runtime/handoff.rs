//! Session ownership transfer at an observed idle boundary.

use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex},
};

use atman_rt::{InstanceActivity, InstanceState};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use uuid::Uuid;

use super::{
    CatalogCommand, CatalogEnvelope, FlowDirectory, FlowInstanceId, FlowWorker, RuntimeHost,
    SessionHandle,
};
use crate::event_log::journal::CommitReceipt;
use crate::flow_runtime::scheduler::{FlowScheduler, SchedulerBinding, SchedulingStatus};
use crate::flow_store::{invalid, position::FlowPosition};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum HandoffScheduling {
    Running,
    Stopped,
    Failed { error: String },
}

impl From<SchedulingStatus> for HandoffScheduling {
    fn from(status: SchedulingStatus) -> Self {
        match status {
            SchedulingStatus::Running => Self::Running,
            SchedulingStatus::Stopped => Self::Stopped,
            SchedulingStatus::Failed(error) => Self::Failed { error },
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HandoffFlow {
    pub instance_id: FlowInstanceId,
    pub cursor: FlowPosition,
    pub scheduling: Option<HandoffScheduling>,
}

/// Original durable cursors and scheduling observations, captured before ownership release.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionHandoffReceipt {
    pub request_id: Uuid,
    pub source_generation: Uuid,
    pub session_id: Uuid,
    pub catalog: CommitReceipt,
    pub flows: Vec<HandoffFlow>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HandoffBlocker {
    Admission,
    Active { instance_id: FlowInstanceId },
    Resources { instance_id: FlowInstanceId },
    Interactions { instance_id: FlowInstanceId },
    Transient { instance_id: FlowInstanceId },
    Retention,
    SourceCapture,
    ProfileAuthority,
    Controls,
    Observation { instance_id: FlowInstanceId },
    Continuation { instance_id: FlowInstanceId },
    Unavailable { message: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionHandoffOutcome {
    Pending {
        request_id: Uuid,
        source_generation: Uuid,
        session_id: Uuid,
        blockers: Vec<HandoffBlocker>,
    },
    Released {
        receipt: SessionHandoffReceipt,
    },
}

/// A stale native handle cannot acquire a new execution owner implicitly.
#[derive(Clone, Debug)]
pub struct SessionOwnerReleased {
    pub session_id: Uuid,
    pub source_generation: Uuid,
}

impl std::fmt::Display for SessionOwnerReleased {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "session {} was released by host {}",
            self.session_id, self.source_generation
        )
    }
}

impl std::error::Error for SessionOwnerReleased {}

#[derive(Default)]
struct AdmissionState {
    active: usize,
    frozen: bool,
    released: bool,
}

/// Permits stay with accepted owner commands through their publication acknowledgement.
#[derive(Default)]
pub(in crate::flow_runtime) struct SessionAdmission {
    pub(super) identity: Option<SessionOwnerReleased>,
    pub(super) authority: Arc<std::sync::OnceLock<crate::peer_authority::PeerAuthority>>,
    state: Mutex<AdmissionState>,
    changed: Notify,
}

impl SessionAdmission {
    pub(super) fn new(session_id: Uuid, source_generation: Uuid) -> Self {
        Self {
            identity: Some(SessionOwnerReleased {
                session_id,
                source_generation,
            }),
            ..Self::default()
        }
    }

    pub(super) fn with_authority(
        session_id: Uuid,
        source_generation: Uuid,
        authority: Arc<std::sync::OnceLock<crate::peer_authority::PeerAuthority>>,
    ) -> Self {
        Self {
            authority,
            ..Self::new(session_id, source_generation)
        }
    }

    pub(in crate::flow_runtime) async fn enter(self: &Arc<Self>) -> io::Result<AdmissionPermit> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(admission) = self.try_enter()? {
                return Ok(admission);
            }
            changed.await;
        }
    }

    pub(in crate::flow_runtime) fn try_enter(
        self: &Arc<Self>,
    ) -> io::Result<Option<AdmissionPermit>> {
        let mut state = self.state.lock().unwrap();
        if state.released {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                self.identity
                    .clone()
                    .expect("released admission has an owner"),
            ));
        }
        if state.frozen {
            return Ok(None);
        }
        state.active = state
            .active
            .checked_add(1)
            .ok_or_else(|| invalid("session admission capacity exhausted"))?;
        Ok(Some(AdmissionPermit(self.clone())))
    }

    pub(super) fn try_freeze(self: &Arc<Self>) -> Option<AdmissionFreeze> {
        let mut state = self.state.lock().unwrap();
        if state.frozen || state.released || state.active != 0 {
            return None;
        }
        state.frozen = true;
        Some(AdmissionFreeze {
            admission: self.clone(),
            released: false,
        })
    }

    pub(super) fn is_frozen(&self) -> bool {
        self.state.lock().unwrap().frozen
    }
}

pub(crate) struct AdmissionPermit(Arc<SessionAdmission>);

/// Retain the original Session owner through host-side preparation and acknowledgement. This pin grants no peer authority.
pub struct SessionOperationPin {
    _admission: AdmissionPermit,
}

impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.active -= 1;
    }
}

pub(super) struct AdmissionFreeze {
    admission: Arc<SessionAdmission>,
    released: bool,
}

impl AdmissionFreeze {
    pub(super) fn release(mut self) {
        {
            let mut state = self.admission.state.lock().unwrap();
            state.released = true;
        }
        self.released = true;
        self.admission.changed.notify_waiters();
    }
}

impl Drop for AdmissionFreeze {
    fn drop(&mut self) {
        if !self.released {
            self.admission.state.lock().unwrap().frozen = false;
            self.admission.changed.notify_waiters();
        }
    }
}

impl SessionHandle {
    /// Keep accepted host-side preparation on its original owner until its real result is installed.
    pub async fn retain_operation(&self) -> io::Result<SessionOperationPin> {
        Ok(SessionOperationPin {
            _admission: self.admission.enter().await?,
        })
    }

    pub(in crate::flow_runtime) fn admission(&self) -> Arc<SessionAdmission> {
        self.admission.clone()
    }
}

impl RuntimeHost {
    /// Release only this idle owner; the caller still revalidates untouched history with discard_unused_session.
    pub async fn release_unused_session(
        &self,
        request: &crate::flow_store::SessionRemoval,
    ) -> io::Result<bool> {
        let session = match self.session(request.session_id) {
            Ok(session) => session,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
            Err(error) => return Err(error),
        };
        if session.snapshot().await?.cursor != request.expected {
            return Ok(false);
        }
        let mut identity = blake3::Hasher::new();
        identity.update(b"atman.unused.owner.release\0");
        identity.update(self.generation().as_bytes());
        identity.update(request.session_id.as_bytes());
        identity.update(request.expected.digest.as_bytes());
        let request_id = Uuid::from_bytes(identity.finalize().as_bytes()[..16].try_into().unwrap());
        match self.handoff_session(request.session_id, request_id).await? {
            SessionHandoffOutcome::Released { receipt } => Ok(receipt.catalog == request.expected),
            SessionHandoffOutcome::Pending { .. } => Ok(false),
        }
    }

    /// Confirm the exact receipt currently installed on this owner, without opening execution.
    pub fn installed_handoff(
        &self,
        session_id: Uuid,
        request_id: Uuid,
        source_generation: Uuid,
    ) -> io::Result<Option<SessionHandoffReceipt>> {
        let sessions = self.directory.sessions.lock().unwrap();
        if self
            .directory
            .stopping
            .load(std::sync::atomic::Ordering::Acquire)
            || !sessions.contains_key(&session_id)
        {
            return Ok(None);
        }
        let installed = self
            .directory
            .acquired_sessions
            .lock()
            .unwrap()
            .get(&session_id)
            .cloned();
        match installed {
            Some(receipt)
                if receipt.request_id == request_id
                    && receipt.source_generation == source_generation =>
            {
                Ok(Some(receipt))
            }
            Some(_) => Err(invalid(
                "another handoff receipt is installed on this Session",
            )),
            None => Ok(None),
        }
    }

    /// Observe every original owner obligation before a detached frontend requests shutdown.
    pub async fn shutdown_readiness(&self) -> io::Result<Vec<HandoffBlocker>> {
        self.shutdown_readiness_inner(false).await
    }

    pub(super) async fn shutdown_readiness_inner(
        &self,
        frozen: bool,
    ) -> io::Result<Vec<HandoffBlocker>> {
        let mut blockers = Vec::new();
        if self
            .profile_handle()
            .is_some_and(|profile| !profile.is_ready())
        {
            blockers.push(HandoffBlocker::ProfileAuthority);
        }
        if self.control_work_pending()? {
            blockers.push(HandoffBlocker::Controls);
        }
        for session_id in self.sessions() {
            let session = self.session(session_id)?;
            if session.admission.state.lock().unwrap().active != 0 {
                blockers.push(HandoffBlocker::Admission);
            }
            if !frozen && session.source_reads.clone().try_write_owned().is_err() {
                blockers.push(HandoffBlocker::SourceCapture);
            }
            for worker in session.flows.lock().unwrap().values() {
                if matches!(
                    worker.state.borrow().state(),
                    InstanceState::Open(InstanceActivity::Active(_))
                        | InstanceState::Closing { .. }
                ) {
                    blockers.push(HandoffBlocker::Active {
                        instance_id: worker.address().instance_id.clone(),
                    });
                }
                if worker.is_transient() {
                    blockers.push(HandoffBlocker::Transient {
                        instance_id: worker.address().instance_id.clone(),
                    });
                }
                blockers.extend(host_local_blockers(worker));
                if worker
                    .context()
                    .compact_lock_handle()
                    .try_lock_owned()
                    .is_err()
                {
                    blockers.push(HandoffBlocker::Interactions {
                        instance_id: worker.address().instance_id.clone(),
                    });
                }
            }
            let retained = if frozen {
                session.frozen_retention_pending().await?
            } else {
                session.retention_pending().await?
            };
            if retained {
                blockers.push(HandoffBlocker::Retention);
            }
        }
        blockers.extend(self.idle_background_readiness(frozen).await?);
        Ok(blockers)
    }

    /// Try to relinquish one Session without cancelling active work. Pending attempts leave
    /// all execution and scheduling intact. Retry the same identity until release is confirmed.
    pub async fn handoff_session(
        &self,
        session_id: Uuid,
        request_id: Uuid,
    ) -> io::Result<SessionHandoffOutcome> {
        if session_id.is_nil() || request_id.is_nil() {
            return Err(invalid("handoff identities must not be nil"));
        }
        let host = self.clone();
        tokio::spawn(async move {
            {
                let mut requests = host.directory.handoff_requests.lock().unwrap();
                if let Some(existing) = requests.get(&request_id) {
                    if *existing != session_id {
                        return Err(invalid("handoff request belongs to another Session"));
                    }
                } else {
                    requests.insert(request_id, session_id);
                }
            }
            if let Some(receipt) = host
                .directory
                .handoffs
                .lock()
                .unwrap()
                .get(&request_id)
                .cloned()
            {
                if receipt.session_id != session_id {
                    return Err(invalid("handoff request belongs to another Session"));
                }
                return Ok(SessionHandoffOutcome::Released { receipt });
            }
            let session = host.session(session_id)?;
            let _operation = session.handoff.lock().await;
            if let Some(receipt) = host
                .directory
                .handoffs
                .lock()
                .unwrap()
                .get(&request_id)
                .cloned()
            {
                return Ok(SessionHandoffOutcome::Released { receipt });
            }
            let mut blockers = Vec::new();
            if host.profile_handle().is_some()
                && (host.peer_port().is_none() || host.authority_domain().is_none())
            {
                blockers.push(HandoffBlocker::ProfileAuthority);
            }
            if host.peer_port().is_none() || host.authority_domain().is_none() {
                match host.control_work_pending() {
                    Ok(false) => {}
                    _ => blockers.push(HandoffBlocker::Controls),
                }
            }
            let captures = session.source_reads.clone().try_write_owned();
            let Ok(_captures) = captures else {
                blockers.push(HandoffBlocker::SourceCapture);
                return Ok(pending(&host, session_id, request_id, blockers));
            };
            if !blockers.is_empty() {
                return Ok(pending(&host, session_id, request_id, blockers));
            }
            for worker in session.flows.lock().unwrap().values() {
                if worker.is_transient() {
                    blockers.push(HandoffBlocker::Transient {
                        instance_id: worker.address().instance_id.clone(),
                    });
                }
                if matches!(
                    worker.state.borrow().state(),
                    InstanceState::Open(InstanceActivity::Active(_))
                        | InstanceState::Closing { .. }
                ) {
                    blockers.push(HandoffBlocker::Active {
                        instance_id: worker.address().instance_id.clone(),
                    });
                }
                blockers.extend(host_local_blockers(worker));
            }
            if !blockers.is_empty() {
                return Ok(pending(&host, session_id, request_id, blockers));
            }
            drop(_captures);
            let roster = prepare_cold_roster(&host, &session).await?;
            let Ok(_captures) = session.source_reads.clone().try_write_owned() else {
                return Ok(pending(
                    &host,
                    session_id,
                    request_id,
                    vec![HandoffBlocker::SourceCapture],
                ));
            };
            let (reply, received) = tokio::sync::oneshot::channel();
            session
                .commands
                .send(CatalogEnvelope {
                    command: CatalogCommand::Handoff {
                        request_id,
                        roster,
                        reply,
                    },
                    admission: None,
                    authority: None,
                })
                .await
                .map_err(|_| invalid("Session owner stopped before handoff"))?;
            received
                .await
                .map_err(|_| invalid("Session handoff did not acknowledge ownership"))?
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Explicitly acquire a released Session through its original writer lock and catalog receipt.
    /// The caller authenticates the source and restores only the receipt's previously running bindings.
    pub async fn acquire_handoff(
        &self,
        data: &std::path::Path,
        receipt: &SessionHandoffReceipt,
    ) -> io::Result<SessionHandle> {
        if receipt.session_id.is_nil()
            || receipt.request_id.is_nil()
            || receipt.catalog.journal_id != receipt.session_id
            || receipt
                .flows
                .iter()
                .any(|flow| flow.cursor.require_durable().is_err())
        {
            return Err(invalid("invalid Session handoff receipt"));
        }
        let session = self
            .acquire_session(data, receipt.session_id, None, Some(receipt.clone()))
            .await?;
        session
            .request(|reply| CatalogCommand::InstallHandoffScheduling {
                receipt: receipt.clone(),
                reply,
            })
            .await?;
        Ok(session)
    }
}

pub(super) fn pending(
    host: &RuntimeHost,
    session_id: Uuid,
    request_id: Uuid,
    blockers: Vec<HandoffBlocker>,
) -> SessionHandoffOutcome {
    SessionHandoffOutcome::Pending {
        request_id,
        source_generation: host.generation(),
        session_id,
        blockers,
    }
}

pub(super) struct PreparedRelease {
    pub freeze: AdmissionFreeze,
    pub flows: Vec<HandoffFlow>,
    bindings: Vec<(FlowInstanceId, SchedulerBinding)>,
    _compact: Vec<tokio::sync::OwnedMutexGuard<()>>,
    scheduling: HashMap<FlowInstanceId, HandoffScheduling>,
    _authority: Option<Arc<crate::peer_authority::AuthorityGuard>>,
}

pub(super) struct PreparedRoster {
    pub catalog: CommitReceipt,
    pub cold: Vec<HandoffFlow>,
}

/// Preparation runs while normal admission and the catalog actor remain available.
async fn prepare_cold_roster(
    host: &RuntimeHost,
    session: &SessionHandle,
) -> io::Result<PreparedRoster> {
    let loaded: std::collections::HashSet<_> = session
        .flows
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, worker)| !worker.is_stopped())
        .map(|(id, _)| id.clone())
        .collect();
    let data = host
        .directory
        .storage_roots
        .lock()
        .unwrap()
        .get(&session.id())
        .cloned()
        .ok_or_else(|| invalid("Session storage root is unavailable"))?;
    let id = session.id();
    tokio::task::spawn_blocking(move || {
        let inspected = crate::flow_store::RuntimeStore::inspect_session(&data, id)?;
        if inspected.journal.trailing_bytes != 0 {
            return Err(invalid("handoff catalog has incomplete trailing data"));
        }
        let catalog = inspected
            .journal
            .cursor
            .clone()
            .ok_or_else(|| invalid("handoff catalog cursor is unavailable"))?;
        let cold = inspected
            .projection
            .flows
            .iter()
            .filter(|flow| !loaded.contains(&flow.id))
            .map(|flow| {
                let recorded = inspected.inspect_flow(&flow.id)?;
                if recorded.journal.trailing_bytes != 0 {
                    return Err(invalid("handoff Flow journal has incomplete trailing data"));
                }
                Ok(HandoffFlow {
                    instance_id: flow.id.clone(),
                    cursor: recorded
                        .journal
                        .cursor
                        .ok_or_else(|| invalid("handoff Flow cursor is unavailable"))?
                        .into(),
                    scheduling: None,
                })
            })
            .collect::<io::Result<Vec<_>>>()?;
        Ok(PreparedRoster { catalog, cold })
    })
    .await
    .map_err(io::Error::other)?
}

/// Runs inside the catalog actor; its special envelope owns no normal admission permit.
pub(super) async fn prepare_release(
    host: &RuntimeHost,
    session: &SessionHandle,
    flows: &FlowDirectory,
    schedulers: &mut HashMap<FlowInstanceId, FlowScheduler>,
    retention_pending: bool,
) -> Result<PreparedRelease, Vec<HandoffBlocker>> {
    let freeze = {
        let _sessions = host.directory.sessions.lock().unwrap();
        if host.profile_handle().is_some()
            && (host.peer_port().is_none() || host.authority_domain().is_none())
        {
            return Err(vec![HandoffBlocker::ProfileAuthority]);
        }
        let Some(freeze) = session.admission.try_freeze() else {
            return Err(vec![HandoffBlocker::Admission]);
        };
        freeze
    };
    let authority = match host.authority_domain() {
        Some(authority) => {
            let plan = crate::peer_authority::AuthorityPlan::new([(
                crate::peer_authority::AuthorityKey::Session(session.id()),
                crate::peer_authority::AuthorityMode::Exclusive,
            )])
            .map_err(|error| {
                vec![HandoffBlocker::Unavailable {
                    message: error.to_string(),
                }]
            })?;
            match authority.try_acquire(plan).await {
                Ok(Some(guard)) => Some(Arc::new(guard)),
                Ok(None) => return Err(vec![HandoffBlocker::Admission]),
                Err(error) => {
                    return Err(vec![HandoffBlocker::Unavailable {
                        message: error.to_string(),
                    }]);
                }
            }
        }
        None => None,
    };
    let mut blockers = Vec::new();
    if retention_pending {
        blockers.push(HandoffBlocker::Retention);
    }
    let mut workers: Vec<_> = flows
        .lock()
        .unwrap()
        .values()
        .filter(|worker| !worker.is_stopped())
        .cloned()
        .collect();
    workers.sort_by_key(|worker| worker.address().instance_id.0.0);
    let mut observations = Vec::with_capacity(workers.len());
    let mut compact = Vec::with_capacity(workers.len());
    let scheduling: HashMap<_, HandoffScheduling> = schedulers
        .iter()
        .map(|(id, scheduler)| {
            (
                id.clone(),
                scheduler.subscribe_status().borrow().clone().into(),
            )
        })
        .collect();
    for (id, status) in &scheduling {
        if *status == HandoffScheduling::Running
            && !workers
                .iter()
                .any(|worker| &worker.address().instance_id == id)
        {
            blockers.push(HandoffBlocker::Observation {
                instance_id: id.clone(),
            });
        }
    }
    for worker in &workers {
        let id = worker.address().instance_id.clone();
        if worker
            .scheduler_owned
            .load(std::sync::atomic::Ordering::Acquire)
            && !schedulers.contains_key(&id)
        {
            blockers.push(HandoffBlocker::Observation {
                instance_id: id.clone(),
            });
        }
        if worker.is_transient() {
            blockers.push(HandoffBlocker::Transient { instance_id: id });
            continue;
        }
        match worker.handoff_snapshot().await {
            Ok((state, cursor, process_watches)) => {
                if process_watches {
                    blockers.push(HandoffBlocker::Resources {
                        instance_id: id.clone(),
                    });
                }
                if matches!(
                    state.state(),
                    InstanceState::Open(InstanceActivity::Active(_))
                        | InstanceState::Closing { .. }
                ) {
                    blockers.push(HandoffBlocker::Active {
                        instance_id: id.clone(),
                    });
                }
                if schedulers.get(&id).is_some_and(|controller| {
                    *controller.subscribe_status().borrow() == SchedulingStatus::Running
                }) {
                    match &*worker.scheduling_progress.borrow() {
                        crate::flow_runtime::scheduler::SchedulingProgress::Idle(
                            through,
                            sources,
                        ) if through == &cursor && !sources.has_changed() => {}
                        _ => blockers.push(HandoffBlocker::Observation {
                            instance_id: id.clone(),
                        }),
                    }
                }
                observations.push(HandoffFlow {
                    instance_id: id.clone(),
                    cursor,
                    scheduling: schedulers
                        .get(&id)
                        .map(|scheduler| scheduler.subscribe_status().borrow().clone().into()),
                });
            }
            Err(error) => blockers.push(HandoffBlocker::Unavailable {
                message: error.to_string(),
            }),
        }
        blockers.extend(host_local_blockers(worker));
        match worker.context().compact_lock_handle().try_lock_owned() {
            Ok(guard) => compact.push(guard),
            Err(_) => blockers.push(HandoffBlocker::Interactions { instance_id: id }),
        }
    }
    if !blockers.is_empty() {
        return Err(blockers);
    }
    // Frozen admission prevents a prepared scheduler or direct start_turn from racing these reads.
    let mut suspended: Vec<(FlowInstanceId, SchedulerBinding)> = Vec::new();
    let mut controllers = std::mem::take(schedulers).into_iter();
    while let Some((id, scheduler)) = controllers.next() {
        match scheduler.suspend().await {
            Ok(binding) => suspended.push((id, binding)),
            Err(error) => {
                schedulers.extend(controllers);
                let restore = restore_scheduling(flows, schedulers, suspended);
                let message = match restore {
                    Ok(()) => error.to_string(),
                    Err(restore) => format!("{error}; scheduler restoration: {restore}"),
                };
                return Err(vec![HandoffBlocker::Unavailable { message }]);
            }
        }
    }
    Ok(PreparedRelease {
        freeze,
        flows: observations,
        bindings: suspended,
        _compact: compact,
        scheduling,
        _authority: authority,
    })
}

fn host_local_blockers(worker: &FlowWorker) -> Vec<HandoffBlocker> {
    let mut blockers = Vec::new();
    let id = worker.address().instance_id.clone();
    if !worker.resources().handoff_ready() {
        blockers.push(HandoffBlocker::Resources {
            instance_id: id.clone(),
        });
    }
    let context = worker.context();
    if !context.forms().list_pending().is_empty()
        || context.compact_reviews().list_pending().is_some()
        || context.permission_broker().has_pending_requests()
        || !context.subscribe_pending_approvals().borrow().is_empty()
        || context.deferred_form_inbox().pending_count() != 0
    {
        blockers.push(HandoffBlocker::Interactions {
            instance_id: id.clone(),
        });
    }
    if !worker.observation_issues.borrow().is_empty() || worker.close_error.borrow().is_some() {
        blockers.push(HandoffBlocker::Observation { instance_id: id });
    }
    blockers
}

impl PreparedRelease {
    pub(super) fn merge_roster(&mut self, cold: Vec<HandoffFlow>) {
        let observed: std::collections::HashSet<_> = self
            .flows
            .iter()
            .map(|flow| flow.instance_id.clone())
            .collect();
        self.flows.extend(
            cold.into_iter()
                .filter(|flow| !observed.contains(&flow.instance_id)),
        );
        for flow in &mut self.flows {
            if flow.scheduling.is_none() {
                flow.scheduling = self.scheduling.get(&flow.instance_id).cloned();
            }
        }
        self.flows.sort_by_key(|flow| flow.instance_id.0.0);
    }

    pub(super) fn restore_scheduling(
        &mut self,
        flows: &FlowDirectory,
        schedulers: &mut HashMap<FlowInstanceId, FlowScheduler>,
    ) -> io::Result<()> {
        restore_scheduling(flows, schedulers, std::mem::take(&mut self.bindings))
    }

    pub(super) async fn suspend_workers(
        &mut self,
        flows: &FlowDirectory,
    ) -> io::Result<Vec<crate::flow_runtime::handoff_storage::FlowHandoffSuspension>> {
        let workers: Vec<_> = flows
            .lock()
            .unwrap()
            .values()
            .filter(|worker| !worker.is_stopped())
            .cloned()
            .collect();
        let mut suspended = Vec::with_capacity(workers.len());
        for worker in workers {
            let owner = worker.suspend_for_handoff().await?;
            if let Some(flow) = self
                .flows
                .iter_mut()
                .find(|flow| flow.instance_id == worker.address().instance_id)
            {
                flow.cursor = owner.cursor().clone();
            }
            suspended.push(owner);
        }
        Ok(suspended)
    }
}

fn restore_scheduling(
    flows: &FlowDirectory,
    schedulers: &mut HashMap<FlowInstanceId, FlowScheduler>,
    bindings: Vec<(FlowInstanceId, SchedulerBinding)>,
) -> io::Result<()> {
    let mut errors = Vec::new();
    for (id, binding) in bindings {
        let worker = flows
            .lock()
            .unwrap()
            .get(&id)
            .filter(|worker| !worker.is_stopped())
            .cloned();
        let restored = worker
            .ok_or_else(|| invalid("Flow writer is unavailable during scheduler restoration"))
            .and_then(|worker| binding.restore(&worker));
        match restored {
            Ok(controller) => {
                schedulers.insert(id, controller);
            }
            Err(error) => errors.push(format!("flow {id}: {error}")),
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(errors.join("; ")))
    }
}

#[cfg(test)]
#[path = "handoff_tests.rs"]
mod tests;
