//! Host-owned control delivery and reconciliation independent of invoking flow execution.

use std::{
    collections::{HashMap, HashSet},
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use atman_rt::{InstanceActivity, InstanceState};
use tokio::{sync::watch, task::JoinSet};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use uuid::Uuid;

use super::{ProfileHandle, RuntimeHost, SessionHandle, WeakRuntimeHost};
use crate::event_log::journal::CommitReceipt;
use crate::flow_runtime::FlowWorker;
use crate::flow_store::{
    FlowRef,
    access::FlowAction,
    control::{CommittedControl, ControlAction, ControlRequest},
    inbox::SenderIdentity,
    invalid,
    profile::{ControlBatchRecord, ControlBatchRequest, ControlBatchUpdate, ControlProgress},
};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ControlServiceProgress {
    pub through: Option<CommitReceipt>,
    pub active: usize,
    pub errors: Vec<String>,
}

pub(super) struct ControlService {
    started: AtomicBool,
    cancel: CancellationToken,
    tasks: TaskTracker,
    progress: watch::Sender<ControlServiceProgress>,
}

impl Default for ControlService {
    fn default() -> Self {
        Self {
            started: AtomicBool::new(false),
            cancel: CancellationToken::new(),
            tasks: TaskTracker::new(),
            progress: watch::channel(ControlServiceProgress::default()).0,
        }
    }
}

impl ControlService {
    pub(super) async fn stop(&self) {
        self.cancel.cancel();
        self.tasks.close();
        self.tasks.wait().await;
    }
}

impl RuntimeHost {
    /// Start one event-driven reconciler for recorded control batches. This does not start a flow entry or model call.
    pub fn start_control_service(&self) -> io::Result<()> {
        tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
        let _gate = self.directory.sessions.lock().unwrap();
        if self.directory.stopping.load(Ordering::Acquire) {
            return Err(invalid("instance host is stopping"));
        }
        let profile = self
            .directory
            .profile
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| invalid("control batches require an attached profile"))?;
        let service = &self.directory.controls;
        if service.started.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        service.tasks.spawn(supervise(
            WeakRuntimeHost(Arc::downgrade(&self.directory)),
            profile,
            service.cancel.clone(),
            service.progress.clone(),
        ));
        Ok(())
    }

    /// Admit a trusted actor's immutable target snapshot. The service retains execution independently of the caller.
    pub async fn submit_controls(
        &self,
        request: ControlBatchRequest,
        authorize: impl FnOnce(&ControlBatchRequest) -> io::Result<()> + Send + 'static,
    ) -> io::Result<ControlBatchRecord> {
        self.start_control_service()?;
        let profile = self.directory.profile.lock().unwrap().clone().unwrap();
        profile.submit_controls(request, authorize).await
    }

    pub fn subscribe_controls(&self) -> watch::Receiver<ControlServiceProgress> {
        self.directory.controls.progress.subscribe()
    }

    pub(in crate::flow_runtime) fn control_work_pending(&self) -> io::Result<bool> {
        if !self.directory.controls.started.load(Ordering::Acquire) {
            return Ok(false);
        }
        let progress = self.directory.controls.progress.borrow();
        let profile = self
            .directory
            .profile
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| invalid("control profile is unavailable"))?;
        let view = profile.subscribe_profile();
        if !progress.errors.is_empty() {
            return Err(io::Error::other(progress.errors.join("; ")));
        }
        if progress.through.as_ref() != Some(&view.borrow().profile.cursor) {
            return Ok(true);
        }
        Ok(progress.active != 0)
    }

    async fn control_session(
        &self,
        profile: &ProfileHandle,
        id: Uuid,
    ) -> io::Result<SessionHandle> {
        self.open_session(&profile.control_source(), id).await
    }

    async fn control_authorizer(
        &self,
        profile: &ProfileHandle,
        request: &ControlRequest,
    ) -> io::Result<Option<SessionHandle>> {
        match &request.sender {
            SenderIdentity::Flow(source) => Ok(Some(
                self.control_session(profile, source.session_id.0).await?,
            )),
            SenderIdentity::User(_) | SenderIdentity::Host(_) => Ok(None),
        }
    }
}

type ItemKey = (SenderIdentity, Uuid, FlowRef);

async fn supervise(
    host: WeakRuntimeHost,
    profile: ProfileHandle,
    cancel: CancellationToken,
    published: watch::Sender<ControlServiceProgress>,
) {
    let mut profile_changes = profile.subscribe_profile();
    let Ok(owner) = host.upgrade() else {
        return;
    };
    let mut directory_changes = owner.changes();
    drop(owner);
    let mut running = HashSet::<ItemKey>::new();
    let mut failed = HashMap::<ItemKey, String>::new();
    let mut jobs = JoinSet::new();
    loop {
        if cancel.is_cancelled() {
            break;
        }
        if profile_changes.has_changed().unwrap_or(false)
            || directory_changes.has_changed().unwrap_or(false)
        {
            failed.clear();
        }
        profile_changes.borrow_and_update();
        directory_changes.borrow_and_update();
        let Ok(owner) = host.upgrade() else {
            break;
        };
        let snapshot = tokio::select! { biased; _ = cancel.cancelled() => break, result = profile.pending_control_snapshot() => result };
        let mut progress = ControlServiceProgress::default();
        match snapshot {
            Ok((through, batches)) => {
                progress.through = Some(through);
                let mut pending = HashSet::new();
                for batch in batches {
                    for item in batch.items.into_iter().filter(|item| !item.is_terminal()) {
                        let key = (batch.sender.clone(), batch.id, item.request.target.clone());
                        pending.insert(key.clone());
                        if running.contains(&key) || failed.contains_key(&key) {
                            continue;
                        }
                        running.insert(key.clone());
                        let owner = owner.clone();
                        let profile = profile.clone();
                        jobs.spawn(async move {
                            let result = reconcile(&owner, &profile, batch.id, item.request).await;
                            (key, result)
                        });
                    }
                }
                failed.retain(|key, _| pending.contains(key));
            }
            Err(error) => progress.errors.push(error.to_string()),
        }
        progress.active = running.len();
        progress.errors.extend(failed.values().cloned());
        progress.errors.sort();
        published.send_if_modified(|old| {
            if *old == progress {
                false
            } else {
                *old = progress;
                true
            }
        });
        drop(owner);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            done = jobs.join_next(), if !jobs.is_empty() => {
                match done {
                    Some(Ok((key, result))) => {
                        running.remove(&key);
                        if let Err(error) = result {
                            let message = format!("control {} for {}/{}: {error}", key.1, key.2.session_id, key.2.instance_id);
                            failed.insert(key, message);
                        }
                    },
                    Some(Err(error)) => { published.send_modify(|progress| progress.errors.push(error.to_string())); break; },
                    None => (),
                }
            }
            changed = profile_changes.changed() => { if changed.is_err() { break; } failed.clear(); },
            changed = directory_changes.changed() => { if changed.is_err() { break; } failed.clear(); },
        }
    }
    jobs.abort_all();
    while jobs.join_next().await.is_some() {}
}

async fn reconcile(
    host: &RuntimeHost,
    profile: &ProfileHandle,
    batch_id: Uuid,
    request: ControlRequest,
) -> io::Result<()> {
    let update = |progress| ControlBatchUpdate {
        batch_id,
        sender: request.sender.clone(),
        target: request.target.clone(),
        progress,
    };
    let result = async {
        let session = host
            .control_session(profile, request.target.session_id.0)
            .await?;
        let worker = session
            .open_flow(request.target.instance_id.clone())
            .await?;
        let previous = request.clone();
        let recorded = worker
            .request(move |store| store.recorded_control(&previous))
            .await?;
        let accepted = match recorded {
            Some(record) => record,
            None => {
                let source = host.control_authorizer(profile, &request).await?;
                let owner = host.clone();
                let sender = request.sender.clone();
                worker
                    .control(request.clone(), move |target| {
                        if let (SenderIdentity::Flow(source), Some(session)) = (&sender, &source)
                            && !owner.allows_access(session, source, target, FlowAction::Control)
                        {
                            return Err(io::Error::new(
                                io::ErrorKind::PermissionDenied,
                                "control permission was revoked",
                            ));
                        }
                        Ok(())
                    })
                    .await?
            }
        };
        profile
            .advance_control(update(ControlProgress::Accepted {
                receipt: accepted.receipt.clone(),
            }))
            .await?;
        if request.action == ControlAction::Close {
            worker.finish_close().await?;
        }
        let receipt = applied(&worker, &accepted).await?;
        profile
            .advance_control(update(ControlProgress::Applied { receipt }))
            .await?;
        Ok::<_, io::Error>(())
    }
    .await;
    match result {
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::InvalidInput
                    | io::ErrorKind::InvalidData
                    | io::ErrorKind::PermissionDenied
                    | io::ErrorKind::NotFound
            ) =>
        {
            profile
                .advance_control(update(ControlProgress::Failed {
                    reason: error.to_string(),
                }))
                .await?;
            Ok(())
        }
        other => other,
    }
}

async fn applied(worker: &FlowWorker, accepted: &CommittedControl) -> io::Result<CommitReceipt> {
    let mut changed = worker.subscribe_commits();
    let mut close_error = worker.subscribe_close_error();
    loop {
        changed.borrow_and_update();
        close_error.borrow_and_update();
        let accepted = accepted.clone();
        let receipt = worker
            .request(move |store| {
                Ok(match accepted.update.request.action {
                    ControlAction::Close => (store.state().state() == &InstanceState::Closed)
                        .then(|| store.cursor().clone()),
                    ControlAction::CancelTurn => match accepted.execution.state() {
                        InstanceState::Open(InstanceActivity::Active(active)) => store
                            .completion(Some(&active.lease.turn))
                            .filter(|done| done.completion.lease == active.lease)
                            .map(|done| done.receipt.clone()),
                        _ => Some(accepted.receipt),
                    },
                    _ => Some(accepted.receipt),
                })
            })
            .await?;
        if let Some(receipt) = receipt {
            return Ok(receipt);
        }
        if let Some(error) = close_error.borrow().as_ref() {
            return Err(io::Error::other(error.clone()));
        }
        tokio::select! {
            result = changed.changed() => result.map_err(io::Error::other)?,
            result = close_error.changed() => result.map_err(io::Error::other)?,
        }
    }
}
