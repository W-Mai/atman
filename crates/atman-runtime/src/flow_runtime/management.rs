//! Profile ownership and publication of acknowledged management authority.

use std::{io, sync::Arc};

use serde::Serialize;
use tokio::sync::{mpsc, oneshot, watch};
use uuid::Uuid;

use super::{RuntimeHost, SessionHandle, WeakRuntimeHost};
use crate::event_log::journal::CommitReceipt;
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

#[derive(Clone, Debug, Serialize)]
pub struct ProfileView {
    pub profile: ProfileSnapshot,
    /// An invalid binding or uncertain write disables its grants until successfully reconciled.
    pub unavailable: Option<String>,
}

#[derive(Clone)]
pub struct ProfileHandle {
    data_dir: std::path::PathBuf,
    commands: mpsc::Sender<Command>,
    state: watch::Receiver<ProfileView>,
    host: WeakRuntimeHost,
}

type ProfileEdit = Box<dyn FnOnce(&mut ProfileStore) -> io::Result<CommitReceipt> + Send>;
type ControlWork = Box<dyn FnOnce(&mut ProfileStore) -> Box<dyn FnOnce() + Send> + Send>;

enum Command {
    Snapshot(oneshot::Sender<io::Result<ProfileView>>),
    Bootstrap(
        SessionConfiguration,
        oneshot::Sender<io::Result<ProfileView>>,
    ),
    Edit(ProfileEdit, oneshot::Sender<io::Result<CommitReceipt>>),
    Control(ControlWork),
    Shutdown(oneshot::Sender<io::Result<()>>),
}

impl RuntimeHost {
    /// Attach one exclusive profile owner. Validation is serialized before its grants become usable; no session or flow is implicitly opened.
    pub fn attach_profile(&self, store: ProfileStore) -> io::Result<ProfileHandle> {
        let runtime = tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
        // Share the session-attachment gate with shutdown.
        let _sessions = self.directory.sessions.lock().unwrap();
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
        let handle = ProfileHandle {
            data_dir: store.data_dir().to_path_buf(),
            commands,
            state: published,
            host: WeakRuntimeHost(Arc::downgrade(&self.directory)),
        };
        runtime.spawn(serve(
            store,
            receiver,
            state,
            WeakRuntimeHost(Arc::downgrade(&self.directory)),
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
        self.control_work(move |store| store.control_batches(&sender, before, limit))
            .await
    }

    /// Host reconciliation input, independent of whether the management flow is executable.
    pub async fn pending_controls(&self) -> io::Result<Vec<ControlBatchRecord>> {
        self.control_work(|store| Ok(store.pending_controls().into_iter().cloned().collect()))
            .await
    }

    /// Persist trusted target evidence without changing or revalidating management grants.
    pub async fn advance_control(&self, update: ControlBatchUpdate) -> io::Result<CommitReceipt> {
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
        let (reply, received) = oneshot::channel();
        self.commands
            .send(make(reply))
            .await
            .map_err(|_| io::Error::other("profile owner stopped"))?;
        received
            .await
            .map_err(|_| io::Error::other("profile owner stopped before acknowledgement"))?
    }

    pub async fn snapshot(&self) -> io::Result<ProfileView> {
        self.request(Command::Snapshot).await
    }

    pub async fn bootstrap(&self, configuration: SessionConfiguration) -> io::Result<ProfileView> {
        self.request(|reply| Command::Bootstrap(configuration, reply))
            .await
    }

    pub async fn replace(&self, request: ReplaceManagement) -> io::Result<CommitReceipt> {
        self.request(|reply| Command::Edit(Box::new(move |store| store.replace(request)), reply))
            .await
    }

    pub async fn select(
        &self,
        request_id: Uuid,
        expected: CommitReceipt,
        session_id: Uuid,
        grants: Vec<FlowGrant>,
    ) -> io::Result<CommitReceipt> {
        self.request(|reply| {
            Command::Edit(
                Box::new(move |store| store.select(request_id, expected, session_id, grants)),
                reply,
            )
        })
        .await
    }

    pub(super) async fn shutdown(&self) -> io::Result<()> {
        self.send(Command::Shutdown).await
    }
}

fn publish(
    store: &ProfileStore,
    unavailable: Option<String>,
    state: &watch::Sender<ProfileView>,
    host: &WeakRuntimeHost,
) {
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
    mut commands: mpsc::Receiver<Command>,
    state: watch::Sender<ProfileView>,
    host: WeakRuntimeHost,
) {
    let Ok((mut store, error)) = tokio::task::spawn_blocking(move || {
        let error = store
            .validate_management()
            .err()
            .map(|error| error.to_string());
        (store, error)
    })
    .await
    else {
        return;
    };
    publish(&store, error, &state, &host);
    while let Some(command) = commands.recv().await {
        match command {
            Command::Control(work) => {
                let published = state.clone();
                let owner = host.clone();
                let Ok((returned, acknowledge)) = tokio::task::spawn_blocking(move || {
                    let acknowledge = work(&mut store);
                    let unavailable = if store.write_failed() {
                        Some("profile write failed; reopen to reconcile the journal".into())
                    } else {
                        // Control progress cannot repair or validate a broken management binding.
                        published.borrow().unavailable.clone()
                    };
                    publish(&store, unavailable, &published, &owner);
                    (store, acknowledge)
                })
                .await
                else {
                    return;
                };
                store = returned;
                acknowledge();
            }
            Command::Snapshot(reply) => {
                let _ = reply.send(Ok(state.borrow().clone()));
            }
            Command::Shutdown(reply) => {
                commands.close();
                drop(commands);
                drop(store);
                let _ = reply.send(Ok(()));
                return;
            }
            Command::Bootstrap(configuration, reply) => {
                let published = state.clone();
                let owner = host.clone();
                let Ok((returned, result)) = tokio::task::spawn_blocking(move || {
                    let result = store.bootstrap(configuration);
                    publish(
                        &store,
                        result.as_ref().err().map(ToString::to_string),
                        &published,
                        &owner,
                    );
                    (store, result.map(|_| published.borrow().clone()))
                })
                .await
                else {
                    return;
                };
                store = returned;
                let _ = reply.send(result);
            }
            Command::Edit(edit, reply) => {
                let published = state.clone();
                let owner = host.clone();
                let Ok((returned, result)) = tokio::task::spawn_blocking(move || {
                    let before = store.snapshot().cursor.clone();
                    let result = edit(&mut store);
                    let error = if store.write_failed() {
                        Some("profile write failed; reopen to reconcile the journal".into())
                    } else if result.is_ok() && before != store.snapshot().cursor {
                        None
                    } else {
                        published.borrow().unavailable.clone()
                    };
                    publish(&store, error, &published, &owner);
                    (store, result)
                })
                .await
                else {
                    return;
                };
                store = returned;
                let _ = reply.send(result);
            }
        }
    }
}
