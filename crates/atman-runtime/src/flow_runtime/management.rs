//! Profile ownership and publication of acknowledged management authority.

use std::{io, sync::Arc};

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};
use uuid::Uuid;

use super::{RuntimeHost, SessionHandle, WeakRuntimeHost};
use crate::event_log::journal::CommitReceipt;
use crate::flow_runtime::peer_profile::{ProfileControlPort, ProfileOperation, ProfileReply};
use crate::flow_store::{
    FlowRef, SessionConfiguration,
    access::{AccessTarget, FlowAction, FlowGrant},
    inbox::SenderIdentity,
    invalid,
    profile::{
        ControlBatchRecord, ControlBatchRequest, ControlBatchUpdate, ProfileSnapshot, ProfileStore,
        ReplaceManagement,
    },
};
use crate::peer_authority::{
    AuthorityGuard, AuthorityKey, AuthorityMode, AuthorityPlan, PeerAuthority,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProfileView {
    pub profile: ProfileSnapshot,
    /// An invalid binding or uncertain write disables its grants until successfully reconciled.
    pub unavailable: Option<String>,
}

#[derive(Clone)]
pub struct ProfileHandle {
    data_dir: std::path::PathBuf,
    commands: mpsc::Sender<ProfileCommand>,
    state: watch::Receiver<ProfileView>,
    host: WeakRuntimeHost,
    authority: PeerAuthority,
    remote: Option<RemoteProfile>,
    control_evidence: watch::Receiver<Option<PublishedControls>>,
    ready: watch::Receiver<bool>,
}

#[derive(Clone)]
struct PublishedControls {
    cursor: CommitReceipt,
    records: Arc<[ControlBatchRecord]>,
}

#[derive(Clone)]
struct RemoteProfile {
    port: Arc<dyn ProfileControlPort>,
    published: watch::Sender<ProfileView>,
    cancel: tokio_util::sync::CancellationToken,
}

struct ProfileCommand {
    command: Command,
    authority: Option<Arc<AuthorityGuard>>,
}

struct ProfileOwnerServices {
    state: watch::Sender<ProfileView>,
    controls: watch::Sender<Option<PublishedControls>>,
    host: WeakRuntimeHost,
    domain: PeerAuthority,
    initial_authority: Option<Arc<AuthorityGuard>>,
    ready: watch::Sender<bool>,
}

type ProfileEdit = Box<dyn FnOnce(&mut ProfileStore) -> io::Result<CommitReceipt> + Send>;
type ControlWork = Box<dyn FnOnce(&mut ProfileStore) -> Box<dyn FnOnce() + Send> + Send>;
type ProjectPinResult = io::Result<(crate::project_catalog::ProjectRecord, String)>;

enum Command {
    Snapshot(oneshot::Sender<io::Result<ProfileView>>),
    Bootstrap(
        SessionConfiguration,
        oneshot::Sender<io::Result<ProfileView>>,
    ),
    Edit(ProfileEdit, oneshot::Sender<io::Result<CommitReceipt>>),
    SetProjectPinned {
        fingerprint: String,
        expected_revision: String,
        pinned: bool,
        reply: oneshot::Sender<ProjectPinResult>,
        operation: crate::flow_runtime::host::HostOperationToken,
    },
    Control(ControlWork),
    Shutdown(oneshot::Sender<io::Result<()>>),
}

impl RuntimeHost {
    /// Return the existing profile owner without initializing storage or acquiring another lock.
    pub fn profile_handle(&self) -> Option<ProfileHandle> {
        self.directory
            .profile
            .lock()
            .unwrap()
            .as_ref()
            .filter(|profile| profile.is_local_owner())
            .cloned()
    }

    /// Return the existing local writer or remote facade without opening storage.
    pub fn profile_control_port(&self) -> Option<ProfileHandle> {
        self.directory.profile.lock().unwrap().clone()
    }

    /// Retire only a remote presentation facade or a closed writer before explicit owner recovery.
    pub fn retire_profile_port(&self, expected_profile: Uuid) -> io::Result<()> {
        let _sessions = self.directory.sessions.lock().unwrap();
        let mut profile = self.directory.profile.lock().unwrap();
        let Some(current) = profile.as_ref() else {
            return Ok(());
        };
        if current.authority.profile_id() != expected_profile || current.is_local_owner() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "the original Profile writer is still attached",
            ));
        }
        if let Some(remote) = &current.remote {
            remote.cancel.cancel();
        }
        *profile = None;
        self.changed();
        Ok(())
    }

    /// Install an authenticated remote port. Its published view is for presentation only.
    pub fn install_profile_port(
        &self,
        data_dir: std::path::PathBuf,
        port: Arc<dyn ProfileControlPort>,
        initial: ProfileView,
    ) -> io::Result<ProfileHandle> {
        let runtime = tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
        if initial.profile.profile_id.is_nil()
            || initial.profile.cursor.journal_id != initial.profile.profile_id
        {
            return Err(invalid("remote Profile identity is invalid"));
        }
        let authority = PeerAuthority::new(data_dir.clone(), initial.profile.profile_id)?;
        self.bind_peer_authority(authority.clone())?;
        let (commands, receiver) = mpsc::channel(1);
        drop(receiver);
        let (published, state) = watch::channel(initial);
        let (_, control_evidence) = watch::channel(None);
        let (_, ready) = watch::channel(true);
        let handle = ProfileHandle {
            data_dir,
            commands,
            state,
            host: WeakRuntimeHost(Arc::downgrade(&self.directory)),
            authority,
            control_evidence,
            ready,
            remote: Some(RemoteProfile {
                port,
                published,
                cancel: tokio_util::sync::CancellationToken::new(),
            }),
        };
        {
            let _sessions = self.directory.sessions.lock().unwrap();
            if self
                .directory
                .stopping
                .load(std::sync::atomic::Ordering::Acquire)
            {
                return Err(invalid("instance host is stopping"));
            }
            let mut current = self.directory.profile.lock().unwrap();
            if let Some(current) = current.as_ref() {
                if current.remote.is_none()
                    || current.authority.profile_id() != handle.authority.profile_id()
                {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "Profile port is already attached",
                    ));
                }
                current.remote.as_ref().unwrap().cancel.cancel();
            }
            *current = Some(handle.clone());
        }
        let mirror = handle.clone();
        runtime.spawn(async move {
            loop {
                if mirror.host.upgrade().is_err() {
                    break;
                }
                match mirror.snapshot().await {
                    Ok(_) => {}
                    Err(error) => mirror
                        .remote
                        .as_ref()
                        .unwrap()
                        .published
                        .send_modify(|view| view.unavailable = Some(error.to_string())),
                }
                tokio::select! {
                    _ = mirror.remote.as_ref().unwrap().cancel.cancelled() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {},
                }
            }
        });
        self.changed();
        Ok(handle)
    }

    /// Open one shared profile owner. Accepted initialization survives a cancelled waiter;
    /// failed ownership acquisition remains retryable and is never cached as an owner.
    pub async fn open_profile(&self, data: &std::path::Path) -> io::Result<ProfileHandle> {
        let host = self.clone();
        let data = data.to_owned();
        tokio::spawn(async move {
            // Initialization can establish the domain itself. Publish that owner before shutdown derives its authority fence.
            let _finalizing = host.directory.finalizing.lock().await;
            if let Some(profile) = host
                .profile_control_port()
                .filter(|profile| profile.remote.is_none() && profile.commands.is_closed())
            {
                host.retire_profile_port(profile.authority.profile_id())?;
            }
            if let Some(profile) = host
                .profile_control_port()
                .filter(|profile| !profile.is_local_owner())
            {
                let expected = profile.data_dir.clone();
                let supplied = data.clone();
                let matches = tokio::task::spawn_blocking(move || {
                    Ok::<_, io::Error>(expected.canonicalize()? == supplied.canonicalize()?)
                })
                .await
                .map_err(io::Error::other)??;
                if !matches {
                    return Err(invalid("Profile port belongs to another storage root"));
                }
                profile.snapshot().await?;
                return Ok(profile);
            }
            let _recovery_authority = if host.profile_handle().is_none() {
                let source = data.clone();
                match tokio::task::spawn_blocking(move || {
                    Ok::<_, io::Error>((
                        ProfileStore::inspect_identity(&source)?,
                        source.canonicalize()?,
                    ))
                })
                .await
                .map_err(io::Error::other)?
                {
                    Ok((profile, canonical_data)) => {
                        let authority = PeerAuthority::new(canonical_data, profile)?;
                        host.bind_peer_authority(authority.clone())?;
                        Some(Arc::new(
                            authority
                                .acquire(
                                    AuthorityPlan::new([(
                                        AuthorityKey::Profile,
                                        AuthorityMode::Exclusive,
                                    )])?,
                                    tokio_util::sync::CancellationToken::new(),
                                    std::time::Instant::now() + std::time::Duration::from_secs(30),
                                )
                                .await?,
                        ))
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                    Err(error) => return Err(error),
                }
            } else {
                None
            };
            let _opening = host.directory.opening.lock().await;
            let original = host.profile_control_port();
            let known = original.as_ref().map(|profile| profile.data_dir.clone());
            let store = tokio::task::spawn_blocking(move || {
                let runtime = crate::flow_store::RuntimeStore::open(&data)?;
                let data = data.canonicalize()?;
                if let Some(known) = known {
                    if known.canonicalize()? != data {
                        return Err(invalid("profile owner belongs to another storage root"));
                    }
                    Ok(None)
                } else {
                    ProfileStore::open(&runtime).map(Some)
                }
            })
            .await
            .map_err(io::Error::other)??;
            let profile = match store {
                Some(store) => host.attach_profile_prepared(store, _recovery_authority.clone())?,
                None => original.expect("existing profile was checked"),
            };
            profile.snapshot().await?;
            Ok(profile)
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Attach one exclusive profile owner. Validation is serialized before its grants become usable; no session or flow is implicitly opened.
    pub fn attach_profile(&self, store: ProfileStore) -> io::Result<ProfileHandle> {
        self.attach_profile_prepared(store, None)
    }

    fn attach_profile_prepared(
        &self,
        store: ProfileStore,
        initial_authority: Option<Arc<AuthorityGuard>>,
    ) -> io::Result<ProfileHandle> {
        let runtime = tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
        let authority =
            PeerAuthority::new(store.data_dir().to_path_buf(), store.snapshot().profile_id)?;
        self.bind_peer_authority(authority.clone())?;
        if let Some(prepared) = &initial_authority {
            if !prepared.belongs_to(&authority)
                || !prepared.covers(AuthorityKey::Profile, AuthorityMode::Exclusive)
            {
                return Err(invalid("initial Profile authority bundle is incomplete"));
            }
        }
        // Share the session-attachment gate with shutdown.
        let _sessions = self.directory.sessions.lock().unwrap();
        if _sessions
            .values()
            .any(|session| session.admission.is_frozen())
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Session ownership is being transferred",
            ));
        }
        if self
            .directory
            .stopping
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(invalid("instance host is stopping"));
        }
        let mut profile = self.directory.profile.lock().unwrap();
        if profile.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "profile is already attached",
            ));
        }
        let (commands, receiver) = mpsc::channel(64);
        let (state, published) = watch::channel(ProfileView {
            profile: store.snapshot().clone(),
            unavailable: Some("profile validation is pending".into()),
        });
        let (controls, control_evidence) = watch::channel(None);
        let (ready, readiness) = watch::channel(false);
        let handle = ProfileHandle {
            data_dir: store.data_dir().to_path_buf(),
            commands,
            state: published,
            host: WeakRuntimeHost(Arc::downgrade(&self.directory)),
            authority,
            remote: None,
            control_evidence,
            ready: readiness,
        };
        runtime.spawn(serve(
            store,
            receiver,
            ProfileOwnerServices {
                state,
                controls,
                host: WeakRuntimeHost(Arc::downgrade(&self.directory)),
                domain: handle.authority.clone(),
                initial_authority,
                ready,
            },
        ));
        *profile = Some(handle.clone());
        Ok(handle)
    }

    pub(crate) fn allows_access(
        &self,
        session: &SessionHandle,
        source: &FlowRef,
        target: &FlowRef,
        action: FlowAction,
    ) -> bool {
        self.allows_scope(session, source, &AccessTarget::Flow(target.clone()), action)
    }

    pub(crate) fn allows_scope(
        &self,
        session: &SessionHandle,
        source: &FlowRef,
        target: &AccessTarget,
        action: FlowAction,
    ) -> bool {
        if session.is_stopped() {
            return false;
        }
        if self.is_transient(source) && target != &AccessTarget::Flow(source.clone()) {
            return false;
        }
        if let AccessTarget::Flow(target) = target
            && target != source
            && self.is_transient(target)
        {
            return false;
        }
        if session
            .access
            .read()
            .unwrap()
            .allows_scope(source, target, action)
        {
            return true;
        }
        self.directory
            .profile
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|profile| {
                let view = profile.state.borrow();
                !profile.commands.is_closed()
                    && view.unavailable.is_none()
                    && view
                        .profile
                        .management
                        .as_ref()
                        .is_some_and(|binding| binding.allows_scope(source, target, action))
            })
    }

    pub fn session_catalog(&self) -> super::super::catalog::SessionCatalog {
        let data = self
            .directory
            .profile
            .lock()
            .unwrap()
            .as_ref()
            .map(|profile| profile.data_dir.clone());
        super::super::catalog::SessionCatalog::new(Some(self.clone()), data)
    }

    pub(crate) async fn management_view(&self) -> Option<ProfileView> {
        let profile = self.directory.profile.lock().unwrap().clone()?;
        profile.snapshot().await.ok()
    }
}

impl ProfileHandle {
    pub fn is_local_owner(&self) -> bool {
        self.remote.is_none() && !self.commands.is_closed()
    }

    pub fn is_ready(&self) -> bool {
        *self.ready.borrow() || self.commands.is_closed()
    }

    async fn remote_call(&self, operation: ProfileOperation) -> io::Result<ProfileReply> {
        self.host.upgrade()?;
        let remote = self
            .remote
            .clone()
            .ok_or_else(|| invalid("Profile port is local"))?;
        let profile = self.authority.profile_id();
        tokio::spawn(async move {
            let reply = remote.port.call(operation).await?;
            if let ProfileReply::View(view) = &reply {
                if view.profile.profile_id != profile || view.profile.cursor.journal_id != profile {
                    return Err(invalid("remote Profile identity changed"));
                }
                remote.published.send_replace(view.clone());
            }
            Ok(reply)
        })
        .await
        .map_err(io::Error::other)?
    }
    pub(super) fn control_source(&self) -> std::path::PathBuf {
        self.data_dir.clone()
    }

    pub(super) fn subscribe_profile(&self) -> watch::Receiver<ProfileView> {
        self.state.clone()
    }

    pub(super) async fn pending_control_snapshot(
        &self,
    ) -> io::Result<(CommitReceipt, Vec<ControlBatchRecord>)> {
        self.control_work(|store| {
            Ok((
                store.snapshot().cursor.clone(),
                store.pending_controls().into_iter().cloned().collect(),
            ))
        })
        .await
    }
    async fn control_work<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut ProfileStore) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        self.request(|reply| {
            Command::Control(Box::new(move |store| {
                let result = work(store);
                Box::new(move || {
                    let _ = reply.send(result);
                })
            }))
        })
        .await
    }

    /// Authenticate admission under the profile owner, including matching retries. Queue acceptance survives a dropped waiting caller; this method does not deliver target commands.
    pub async fn submit_controls(
        &self,
        request: ControlBatchRequest,
        authorize: impl FnOnce(&ControlBatchRequest) -> io::Result<()> + Send + 'static,
    ) -> io::Result<ControlBatchRecord> {
        if self.remote.is_some() {
            if !matches!(&request.sender, SenderIdentity::User(_)) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "remote Flow control batches require the original peer principal",
                ));
            }
            authorize(&request)?;
            return match self
                .remote_call(ProfileOperation::SubmitUserControls(request))
                .await?
            {
                ProfileReply::Submitted(record) => Ok(record),
                _ => Err(invalid("unexpected Profile submit reply")),
            };
        }
        let host = self.host.clone();
        self.control_work(move |store| {
            authorize(&request)?;
            let record = store.submit_controls(request)?;
            if let Ok(host) = host.upgrade() {
                host.changed();
            }
            Ok(record)
        })
        .await
    }

    /// Read one batch after the host has authorized access to its actor's control history.
    pub async fn control_batch(
        &self,
        sender: SenderIdentity,
        id: Uuid,
    ) -> io::Result<Option<ControlBatchRecord>> {
        if self.remote.is_some() {
            return match self
                .remote_call(ProfileOperation::UserControlBatch { sender, id })
                .await?
            {
                ProfileReply::Batch(record) => Ok(record),
                _ => Err(invalid("unexpected Profile batch reply")),
            };
        }
        self.control_work(move |store| Ok(store.control_batch(&sender, id).cloned()))
            .await
    }

    /// Read a bounded control-history page after the host has authorized the actor.
    pub async fn control_batches(
        &self,
        sender: SenderIdentity,
        before: Option<Uuid>,
        limit: usize,
    ) -> io::Result<crate::flow_store::profile::ControlBatchPage> {
        if self.remote.is_some() {
            return match self
                .remote_call(ProfileOperation::UserControlHistory {
                    sender,
                    before,
                    limit,
                })
                .await?
            {
                ProfileReply::History(page) => Ok(page),
                _ => Err(invalid("unexpected Profile history reply")),
            };
        }
        self.control_work(move |store| store.control_batches(&sender, before, limit))
            .await
    }

    /// Host reconciliation input, independent of whether the management flow is executable.
    pub async fn pending_controls(&self) -> io::Result<Vec<ControlBatchRecord>> {
        if self.remote.is_some() {
            return Err(invalid(
                "only the original Profile owner reconciles pending controls",
            ));
        }
        self.control_work(|store| Ok(store.pending_controls().into_iter().cloned().collect()))
            .await
    }

    /// Persist trusted target evidence without changing or revalidating management grants.
    pub async fn advance_control(&self, update: ControlBatchUpdate) -> io::Result<CommitReceipt> {
        if self.remote.is_some() {
            return Err(invalid(
                "only the original Profile owner publishes control progress",
            ));
        }
        self.control_work(move |store| store.advance_control(update))
            .await
    }

    async fn request<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<io::Result<T>>) -> Command,
    ) -> io::Result<T> {
        self.host.upgrade()?;
        self.send(make).await
    }

    async fn send<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<io::Result<T>>) -> Command,
    ) -> io::Result<T> {
        self.send_prepared(make, None).await
    }

    async fn send_prepared<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<io::Result<T>>) -> Command,
        authority: Option<Arc<AuthorityGuard>>,
    ) -> io::Result<T> {
        if authority.is_some() && !*self.ready.borrow() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Profile initialization has not published authority",
            ));
        }
        let mut ready = self.ready.clone();
        ready
            .wait_for(|ready| *ready)
            .await
            .map_err(|_| io::Error::other("Profile initialization stopped"))?;
        let (reply, received) = oneshot::channel();
        let command = make(reply);
        let authority = if matches!(&command, Command::Snapshot(_)) {
            authority
        } else if let Some(guard) = authority {
            self.check_authority(&guard)?;
            Some(guard)
        } else {
            Some(Arc::new(
                self.authority
                    .acquire(
                        AuthorityPlan::new([(AuthorityKey::Profile, AuthorityMode::Exclusive)])?,
                        tokio_util::sync::CancellationToken::new(),
                        std::time::Instant::now() + std::time::Duration::from_secs(30),
                    )
                    .await?,
            ))
        };
        if !matches!(&command, Command::Shutdown(_)) {
            self.host.upgrade()?;
        }
        self.commands
            .send(ProfileCommand { command, authority })
            .await
            .map_err(|_| io::Error::other("profile owner stopped"))?;
        received
            .await
            .map_err(|_| io::Error::other("profile owner stopped before acknowledgement"))?
    }

    pub async fn snapshot(&self) -> io::Result<ProfileView> {
        if self.remote.is_some() {
            return match self.remote_call(ProfileOperation::Snapshot).await? {
                ProfileReply::View(view) => Ok(view),
                _ => Err(invalid("unexpected Profile snapshot reply")),
            };
        }
        self.request(Command::Snapshot).await
    }

    pub async fn bootstrap(&self, configuration: SessionConfiguration) -> io::Result<ProfileView> {
        if self.remote.is_some() {
            return match self
                .remote_call(ProfileOperation::Bootstrap { configuration })
                .await?
            {
                ProfileReply::View(view) => Ok(view),
                _ => Err(invalid("unexpected Profile bootstrap reply")),
            };
        }
        self.request(|reply| Command::Bootstrap(configuration, reply))
            .await
    }

    pub async fn replace(&self, request: ReplaceManagement) -> io::Result<CommitReceipt> {
        if self.remote.is_some() {
            return match self.remote_call(ProfileOperation::Replace(request)).await? {
                ProfileReply::Receipt(receipt) => Ok(receipt),
                _ => Err(invalid("unexpected Profile replace reply")),
            };
        }
        self.request(|reply| Command::Edit(Box::new(move |store| store.replace(request)), reply))
            .await
    }

    /// Pin an observed Project at the original Profile owner, without opening a Session.
    pub async fn set_project_pinned(
        &self,
        fingerprint: String,
        expected_revision: String,
        pinned: bool,
        operation: crate::flow_runtime::host::HostOperationToken,
    ) -> ProjectPinResult {
        if !self.is_local_owner() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "Project Pin requires the current Profile owner",
            ));
        }
        self.request(|reply| Command::SetProjectPinned {
            fingerprint,
            expected_revision,
            pinned,
            reply,
            operation,
        })
        .await
    }

    pub async fn select(
        &self,
        request_id: Uuid,
        expected: CommitReceipt,
        session_id: Uuid,
        grants: Vec<FlowGrant>,
    ) -> io::Result<CommitReceipt> {
        if self.remote.is_some() {
            return match self
                .remote_call(ProfileOperation::Select {
                    request_id,
                    expected,
                    session_id,
                    grants,
                })
                .await?
            {
                ProfileReply::Receipt(receipt) => Ok(receipt),
                _ => Err(invalid("unexpected Profile select reply")),
            };
        }
        self.request(|reply| {
            Command::Edit(
                Box::new(move |store| store.select(request_id, expected, session_id, grants)),
                reply,
            )
        })
        .await
    }

    pub(super) async fn shutdown(&self) -> io::Result<()> {
        if let Some(remote) = &self.remote {
            remote.cancel.cancel();
            return Ok(());
        }
        self.send(Command::Shutdown).await
    }

    pub(in crate::flow_runtime) async fn shutdown_with_authority(
        &self,
        authority: Arc<AuthorityGuard>,
    ) -> io::Result<()> {
        if let Some(remote) = &self.remote {
            remote.cancel.cancel();
            return Ok(());
        }
        self.check_authority(&authority)?;
        if self.commands.is_closed() {
            return Ok(());
        }
        self.send_prepared(Command::Shutdown, Some(authority)).await
    }

    fn check_authority(&self, guard: &AuthorityGuard) -> io::Result<()> {
        if guard.belongs_to(&self.authority)
            && guard.covers(AuthorityKey::Profile, AuthorityMode::Exclusive)
        {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Profile authority bundle is incomplete",
            ))
        }
    }

    pub async fn submit_controls_with_authority(
        &self,
        authority: Arc<AuthorityGuard>,
        request: ControlBatchRequest,
        authorize: impl FnOnce(&ControlBatchRequest) -> io::Result<()> + Send + 'static,
    ) -> io::Result<ControlBatchRecord> {
        if self.remote.is_some() {
            return Err(invalid("prepared Profile effect belongs to another owner"));
        }
        let host = self.host.clone();
        self.send_prepared(
            |reply| {
                Command::Control(Box::new(move |store| {
                    let result = (|| {
                        authorize(&request)?;
                        let record = store.submit_controls(request)?;
                        if let Ok(host) = host.upgrade() {
                            host.changed();
                        }
                        Ok(record)
                    })();
                    Box::new(move || {
                        let _ = reply.send(result);
                    })
                }))
            },
            Some(authority),
        )
        .await
    }

    /// Pure published evidence; validators never queue behind a Profile writer.
    pub fn published_view(&self) -> io::Result<ProfileView> {
        if self.remote.is_some() || self.commands.is_closed() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "Profile evidence belongs to another owner",
            ));
        }
        Ok(self.state.borrow().clone())
    }

    pub(crate) fn published_control(
        &self,
        sender: &SenderIdentity,
        id: Uuid,
    ) -> io::Result<ControlBatchRecord> {
        let view = self.published_view()?;
        let evidence = self.control_evidence.borrow();
        let evidence = evidence
            .as_ref()
            .filter(|evidence| evidence.cursor == view.profile.cursor)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "Profile control evidence is being published",
                )
            })?;
        evidence
            .records
            .iter()
            .find(|record| &record.sender == sender && record.id == id)
            .cloned()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "Profile control batch does not exist",
                )
            })
    }
}

fn publish(
    store: &ProfileStore,
    unavailable: Option<String>,
    state: &watch::Sender<ProfileView>,
    host: &WeakRuntimeHost,
    controls: &watch::Sender<Option<PublishedControls>>,
) {
    let refresh = controls
        .borrow()
        .as_ref()
        .is_none_or(|current| current.cursor != store.snapshot().cursor);
    if refresh {
        controls.send_replace(Some(PublishedControls {
            cursor: store.snapshot().cursor.clone(),
            records: store.control_records().into(),
        }));
    }
    let changed = state.send_if_modified(|view| {
        if view.profile.cursor == store.snapshot().cursor && view.unavailable == unavailable {
            return false;
        }
        *view = ProfileView {
            profile: store.snapshot().clone(),
            unavailable,
        };
        true
    });
    if changed {
        if let Ok(host) = host.upgrade() {
            host.changed();
        }
    }
}

async fn serve(
    store: ProfileStore,
    mut commands: mpsc::Receiver<ProfileCommand>,
    services: ProfileOwnerServices,
) {
    let ProfileOwnerServices {
        state,
        controls,
        host,
        domain,
        initial_authority,
        ready,
    } = services;
    let authority = match initial_authority {
        Some(authority) => authority,
        None => match domain
            .acquire(
                AuthorityPlan::new([(AuthorityKey::Profile, AuthorityMode::Exclusive)])
                    .expect("nonempty Profile plan"),
                tokio_util::sync::CancellationToken::new(),
                std::time::Instant::now() + std::time::Duration::from_secs(30),
            )
            .await
        {
            Ok(authority) => Arc::new(authority),
            Err(_) => return,
        },
    };
    let initial_state = state.clone();
    let initial_controls = controls.clone();
    let initial_host = host.clone();
    let Ok(mut store) = tokio::task::spawn_blocking(move || {
        let error = store
            .validate_management()
            .err()
            .map(|error| error.to_string());
        publish(
            &store,
            error,
            &initial_state,
            &initial_host,
            &initial_controls,
        );
        drop(authority);
        store
    })
    .await
    else {
        return;
    };
    ready.send_replace(true);
    while let Some(ProfileCommand { command, authority }) = commands.recv().await {
        match command {
            Command::Control(work) => {
                let published = state.clone();
                let owner = host.clone();
                let control_evidence = controls.clone();
                let Ok((returned, acknowledge, authority)) =
                    tokio::task::spawn_blocking(move || {
                        let acknowledge = work(&mut store);
                        let unavailable = if store.write_failed() {
                            Some("profile write failed; reopen to reconcile the journal".into())
                        } else {
                            // Control progress cannot repair or validate a broken management binding.
                            published.borrow().unavailable.clone()
                        };
                        publish(&store, unavailable, &published, &owner, &control_evidence);
                        (store, acknowledge, authority)
                    })
                    .await
                else {
                    return;
                };
                store = returned;
                acknowledge();
                drop(authority);
            }
            Command::Snapshot(reply) => {
                let _ = reply.send(Ok(state.borrow().clone()));
            }
            Command::SetProjectPinned {
                fingerprint,
                expected_revision,
                pinned,
                reply,
                operation,
            } => {
                let projects = crate::project_catalog::ProjectCatalogStore::new(store.data_dir());
                let result = tokio::task::spawn_blocking(move || {
                    let _leases = (authority, operation);
                    let result =
                        projects.set_pinned_observed(&fingerprint, &expected_revision, pinned);
                    let _ = reply.send(result);
                })
                .await;
                if result.is_err() {
                    return;
                }
            }
            Command::Shutdown(reply) => {
                commands.close();
                drop(commands);
                drop(store);
                let _ = reply.send(Ok(()));
                drop(authority);
                return;
            }
            Command::Bootstrap(configuration, reply) => {
                let published = state.clone();
                let owner = host.clone();
                let control_evidence = controls.clone();
                let Ok((returned, result, authority)) = tokio::task::spawn_blocking(move || {
                    let result = store.bootstrap(configuration);
                    publish(
                        &store,
                        result.as_ref().err().map(ToString::to_string),
                        &published,
                        &owner,
                        &control_evidence,
                    );
                    (store, result.map(|_| published.borrow().clone()), authority)
                })
                .await
                else {
                    return;
                };
                store = returned;
                let _ = reply.send(result);
                drop(authority);
            }
            Command::Edit(edit, reply) => {
                let published = state.clone();
                let owner = host.clone();
                let control_evidence = controls.clone();
                let Ok((returned, result, authority)) = tokio::task::spawn_blocking(move || {
                    let before = store.snapshot().cursor.clone();
                    let result = edit(&mut store);
                    let error = if store.write_failed() {
                        Some("profile write failed; reopen to reconcile the journal".into())
                    } else if result.is_ok() && before != store.snapshot().cursor {
                        None
                    } else {
                        published.borrow().unavailable.clone()
                    };
                    publish(&store, error, &published, &owner, &control_evidence);
                    (store, result, authority)
                })
                .await
                else {
                    return;
                };
                store = returned;
                let _ = reply.send(result);
                drop(authority);
            }
        }
    }
}
