//! Private same-machine host transport, separate from authenticated User RPC.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::{self, Read, Seek, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use atman_proto::hosts::{HostPresentation, HostRole};
use atman_runtime::{
    flow_runtime::{
        host::{
            HostOperationToken, ProfileHandle, ProfileView, SessionHandoffOutcome,
            SessionHandoffReceipt,
        },
        peer::{
            PeerCall, PeerFailure, PeerIdentity, PeerPort, PeerReply, PeerTarget, decode_error,
            encode_error, handle_peer_call,
        },
        peer_profile::{ProfileControlPort, ProfileOperation, ProfileReply},
    },
    flow_store::profile::ProfileStore,
    platform::ExclusiveFileLease,
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::post,
};
use futures::{StreamExt, future::BoxFuture};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    DaemonState,
    host_directory::{self, HostRecord},
    peer_handoff_store::{self, HandoffRecord, Phase},
};

const REVISION: u32 = 1;
const MAX_BYTES: usize = 16 * 1024 * 1024;
const CALL_PATH: &str = "/internal/peer/call";
const HANDSHAKE_PATH: &str = "/internal/peer/handshake";

struct Credential(String);

#[derive(Clone)]
struct VerifiedPeer {
    identity: PeerIdentity,
    record: HostRecord,
    outbound: bool,
}

pub struct PeerTransport {
    state: Weak<DaemonState>,
    identity: PeerIdentity,
    credential: Credential,
    client: reqwest::Client,
    endpoint: OnceLock<SocketAddr>,
    verified: Mutex<HashMap<String, VerifiedPeer>>,
    handoffs: Mutex<HashMap<Uuid, HandoffBinding>>,
    completed_handoffs: Mutex<HashSet<Uuid>>,
    reported_handoffs: Mutex<HashSet<Uuid>>,
    pending_handoffs: Mutex<HashMap<Uuid, HostOperationToken>>,
    active_coordinators: AtomicUsize,
    recovery: Mutex<HashMap<Uuid, HandoffRecord>>,
}

struct CoordinatorPin {
    transport: Arc<PeerTransport>,
    _operation: HostOperationToken,
}
impl Drop for CoordinatorPin {
    fn drop(&mut self) {
        self.transport
            .active_coordinators
            .fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HandoffBinding {
    pub request: Uuid,
    pub session: Uuid,
    pub source: PeerIdentity,
    pub destination: PeerIdentity,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Handshake {
    revision: u32,
    request: Uuid,
    caller: PeerIdentity,
    expected_transport_generation: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PeerDescription {
    identity: PeerIdentity,
    sessions: Vec<Uuid>,
    profile_owner: bool,
    role: HostRole,
    presentation: HostPresentation,
    authority_revision: u32,
    pending_sessions: Vec<Uuid>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Envelope<T> {
    revision: u32,
    request: Uuid,
    caller: PeerIdentity,
    expected: PeerIdentity,
    operation: T,
}

#[derive(Deserialize, Serialize)]
#[serde(
    tag = "operation",
    content = "arguments",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum HostCall {
    Runtime(PeerCall),
    Prepare(HandoffBinding),
    CancelPrepared(HandoffBinding),
    Release(HandoffBinding),
    Acquire {
        binding: HandoffBinding,
        receipt: SessionHandoffReceipt,
    },
    Confirm(HandoffBinding),
    Acknowledge {
        binding: HandoffBinding,
        receipt: SessionHandoffReceipt,
    },
    Complete {
        binding: HandoffBinding,
        receipt: SessionHandoffReceipt,
    },
    Reassign {
        original: Uuid,
        destination: PeerIdentity,
    },
    Resume {
        original: Uuid,
    },
    RecoveryAcquire {
        original: Uuid,
        binding: HandoffBinding,
    },
}

#[derive(Deserialize, Serialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum HostReply {
    Runtime(Box<PeerReply>),
    Prepared,
    Aborted,
    Released(SessionHandoffOutcome),
    Installed(SessionHandoffReceipt),
    Confirmation(Option<SessionHandoffReceipt>),
    Acknowledged,
    Recovered(SessionHandoffReceipt),
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Reply<T> {
    revision: u32,
    request: Uuid,
    responder: PeerIdentity,
    result: Result<T, PeerFailure>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn unavailable(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, message)
}
fn preparation_cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "handoff preparation cancelled")
}

impl DaemonState {
    /// Prepare the original filesystem domain and a private credential on storage workers.
    pub async fn prepare_peer_transport(self: &Arc<Self>) -> io::Result<()> {
        if let Some(peer) = self.peer_transport.get() {
            self.runtime_host()
                .configure_peer(peer.identity.clone(), peer.clone())?;
            return peer.load_recovery().await;
        }
        let data = self.data_dir().to_owned();
        let identity = tokio::task::spawn_blocking(move || ProfileStore::inspect_identity(&data))
            .await
            .map_err(io::Error::other)?;
        let profile = match identity {
            Ok(id) => id,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.runtime_host()
                    .open_profile(self.data_dir())
                    .await?
                    .snapshot()
                    .await?
                    .profile
                    .profile_id
            }
            Err(error) => return Err(error),
        };
        let data = self.data_dir().to_owned();
        let (root, credential) = loop {
            let data = data.clone();
            match tokio::task::spawn_blocking(move || load_credential(&data))
                .await
                .map_err(io::Error::other)?
            {
                Ok(value) => break value,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    tokio::time::sleep(Duration::from_millis(5)).await
                }
                Err(error) => return Err(error),
            }
        };
        if self.host_routing.get().is_some_and(|routing| {
            crate::http::constant_time_eq(routing.token().as_bytes(), credential.as_bytes())
        }) {
            return Err(invalid("User and peer credentials must be independent"));
        }
        let identity = PeerIdentity {
            profile_id: profile,
            canonical_data_root: root.clone(),
            runtime_generation: self.runtime_host().generation(),
            transport_generation: self.daemon_generation().to_owned(),
        };
        let authority = atman_runtime::peer_authority::PeerAuthority::new(root, profile)?;
        self.runtime_host().bind_peer_authority(authority)?;
        let transport = Arc::new(PeerTransport {
            state: Arc::downgrade(self),
            identity: identity.clone(),
            credential: Credential(credential),
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(3))
                .timeout(Duration::from_secs(30))
                .build()
                .map_err(io::Error::other)?,
            endpoint: OnceLock::new(),
            verified: Default::default(),
            handoffs: Default::default(),
            completed_handoffs: Default::default(),
            reported_handoffs: Default::default(),
            pending_handoffs: Default::default(),
            active_coordinators: AtomicUsize::new(0),
            recovery: Default::default(),
        });
        let installed = match self.peer_transport.set(transport.clone()) {
            Ok(()) => transport,
            Err(_) => self.peer_transport.get().unwrap().clone(),
        };
        self.runtime_host()
            .configure_peer(identity, installed.clone())?;
        installed.load_recovery().await?;
        Ok(())
    }

    pub fn register_peer_endpoint(&self, endpoint: SocketAddr) -> io::Result<()> {
        if !endpoint.ip().is_loopback() || endpoint.port() == 0 {
            return Err(invalid("peer endpoint must be private loopback"));
        }
        if let Some(peer) = self.peer_transport.get() {
            if let Err(endpoint) = peer.endpoint.set(endpoint) {
                if peer.endpoint.get() != Some(&endpoint) {
                    return Err(invalid("peer endpoint is already bound"));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn peer_endpoint(&self) -> Option<SocketAddr> {
        self.peer_transport
            .get()
            .and_then(|peer| peer.endpoint.get().copied())
    }

    /// Unknown release/install outcomes retain the original coordinator until reconciled.
    pub fn handoff_pending(&self) -> bool {
        self.peer_transport.get().is_some_and(|peer| {
            peer.active_coordinators.load(Ordering::Acquire) != 0
                || !peer.pending_handoffs.lock().unwrap().is_empty()
        })
    }

    /// Reuse the actual Profile owner, installing only its presentation facade locally.
    pub async fn ensure_profile_port(self: &Arc<Self>) -> io::Result<ProfileHandle> {
        let peer = self
            .peer_transport
            .get()
            .ok_or_else(|| unavailable("peer transport is not prepared"))?
            .clone();
        if let Some(profile) = self.runtime_host().profile_handle() {
            self.runtime_host().start_control_service()?;
            return Ok(profile);
        }
        if let Some(profile) = self.runtime_host().profile_control_port() {
            if profile.snapshot().await.is_ok() {
                return Ok(profile);
            }
            self.runtime_host()
                .retire_profile_port(peer.identity.profile_id)?;
        }
        match peer.observe_profile().await {
            Ok((owner, initial))
                if owner.runtime_generation != peer.identity.runtime_generation =>
            {
                self.runtime_host().install_profile_port(
                    peer.identity.canonical_data_root.clone(),
                    Arc::new(RemoteProfilePort {
                        transport: Arc::downgrade(&peer),
                    }),
                    initial,
                )
            }
            Ok(_) => {
                let profile = self.runtime_host().open_profile(self.data_dir()).await?;
                self.runtime_host().start_control_service()?;
                Ok(profile)
            }
            Err(error) if error.kind() == io::ErrorKind::NotConnected => {
                let profile = self.runtime_host().open_profile(self.data_dir()).await?;
                self.runtime_host().start_control_service()?;
                Ok(profile)
            }
            Err(error) => Err(error),
        }
    }

    /// Wait asynchronously for one safe Session boundary, then install and confirm its owner.
    pub async fn claim_session(
        self: &Arc<Self>,
        session: Uuid,
        cancel: CancellationToken,
    ) -> io::Result<Option<SessionHandoffReceipt>> {
        let peer = self
            .peer_transport
            .get()
            .ok_or_else(|| unavailable("peer transport is not prepared"))?
            .clone();
        let state = self.clone();
        let pin = peer.pin_coordinator()?;
        let observed = cancel.clone();
        let task = tokio::spawn(async move {
            let _pin = pin;
            peer.claim_owned(&state, session, cancel, true).await
        });
        tokio::select! {
            result = task => result.map_err(io::Error::other)?,
            _ = observed.cancelled() => Err(preparation_cancelled()),
        }
    }

    /// A User requests the same verified host coordinator; it cannot directly release a writer.
    pub async fn request_handoff(
        self: &Arc<Self>,
        request: atman_proto::hosts::HandoffSessionRequest,
    ) -> io::Result<SessionHandoffOutcome> {
        let peer = self
            .peer_transport
            .get()
            .ok_or_else(|| unavailable("the internal host coordination protocol is unavailable"))?
            .clone();
        if request.source_generation != peer.identity.runtime_generation
            || request.destination_generation.is_nil()
            || request.destination_generation == request.source_generation
            || request.request_id.is_nil()
            || request.session_id.0.is_nil()
        {
            return Err(invalid("invalid host handoff identity"));
        }
        let pin = peer.pin_coordinator()?;
        let destination = peer
            .locate_target(PeerTarget::Runtime(request.destination_generation))
            .await?;
        let binding = HandoffBinding {
            request: request.request_id,
            session: request.session_id.0,
            source: peer.identity.clone(),
            destination,
        };
        peer.bind_handoff(&peer.identity, &binding, false)?;
        let state = self.clone();
        let (reply, observed) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _pin = pin;
            let mut reply = Some(reply);
            let result = async {
                peer.dispatch_host(peer.identity.clone(), HostCall::Prepare(binding.clone()))
                    .await?;
                match peer
                    .dispatch_host(peer.identity.clone(), HostCall::Release(binding.clone()))
                    .await?
                {
                    HostReply::Released(outcome @ SessionHandoffOutcome::Pending { .. }) => {
                        let _ = reply.take().unwrap().send(Ok(outcome));
                    }
                    HostReply::Released(SessionHandoffOutcome::Released { .. }) => {}
                    _ => return Err(invalid("unexpected coordinated handoff outcome")),
                }
                peer.transfer_binding(&state, binding, CancellationToken::new())
                    .await
                    .map(|receipt| SessionHandoffOutcome::Released { receipt })
            }
            .await;
            if let Some(reply) = reply {
                let _ = reply.send(result);
            } else if let Err(error) = result {
                atman_runtime::notify!(
                    warn,
                    location = Log,
                    "Session handoff remains pending: {error}"
                );
            }
        });
        observed.await.map_err(io::Error::other)?
    }

    /// Return each retained Session to a real daemon; active work keeps its current owner.
    pub async fn return_sessions_to_daemon(
        self: &Arc<Self>,
        cancel: CancellationToken,
    ) -> io::Result<()> {
        let peer = self
            .peer_transport
            .get()
            .ok_or_else(|| unavailable("peer transport is not prepared"))?
            .clone();
        let state = self.clone();
        let pin = peer.pin_coordinator()?;
        tokio::spawn(async move {
            let _pin = pin;
            let recover = {
                let requests = peer.handoffs.lock().unwrap();
                let pending = peer.pending_handoffs.lock().unwrap();
                requests
                    .values()
                    .filter(|binding| {
                        pending.contains_key(&binding.request)
                            && binding.destination == peer.identity
                    })
                    .cloned()
                    .collect::<Vec<_>>()
            };
            for binding in recover {
                peer.claim_owned(&state, binding.session, cancel.clone(), true)
                    .await?;
            }
            let mut daemons = peer
                .descriptions()
                .await?
                .into_iter()
                .filter(|(_, description)| description.role == HostRole::Daemon)
                .collect::<Vec<_>>();
            daemons.sort_by_key(|(record, _)| std::cmp::Reverse(record.generation));
            let Some((_, destination)) = daemons.first() else {
                let registrations = peer.records().await?;
                let mut abandoned = peer
                    .recovery
                    .lock()
                    .unwrap()
                    .values()
                    .filter(|record| {
                        record.binding.source == peer.identity
                            && record.phase >= Phase::Releasing
                            && record.phase < Phase::Completed
                            && !registrations.iter().any(|host| {
                                host.generation.to_string()
                                    == record.binding.destination.transport_generation
                            })
                    })
                    .map(|record| record.binding.session)
                    .collect::<Vec<_>>();
                abandoned.sort_unstable();
                abandoned.dedup();
                for session in abandoned {
                    peer.claim_owned(&state, session, cancel.clone(), true)
                        .await?;
                }
                return Err(unavailable("no daemon is available to retain Sessions"));
            };
            let mut sessions = state.runtime_host().sessions();
            {
                let requests = peer.handoffs.lock().unwrap();
                let completed = peer.completed_handoffs.lock().unwrap();
                sessions.extend(
                    requests
                        .values()
                        .filter(|binding| {
                            binding.source == peer.identity && !completed.contains(&binding.request)
                        })
                        .map(|binding| binding.session),
                );
            }
            sessions.sort_unstable();
            sessions.dedup();
            for session in sessions {
                peer.transfer(
                    &state,
                    session,
                    peer.identity.clone(),
                    destination.identity.clone(),
                    cancel.clone(),
                )
                .await?;
            }
            Ok(())
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Reconcile abandoned attempts through explicit original-writer recovery, never normal cold opening.
    pub async fn recover_handoffs(self: &Arc<Self>, cancel: CancellationToken) -> io::Result<()> {
        let peer = self
            .peer_transport
            .get()
            .ok_or_else(|| unavailable("peer transport is not prepared"))?
            .clone();
        let state = self.clone();
        let pin = peer.pin_coordinator()?;
        tokio::spawn(async move {
            let _pin = pin;
            peer.load_recovery().await?;
            let registrations = peer.records().await?;
            let records = peer
                .recovery
                .lock()
                .unwrap()
                .values()
                .filter(|record| {
                    record.phase >= Phase::Releasing
                        && record.phase < Phase::Completed
                        && !registrations.iter().any(|host| {
                            host.generation.to_string()
                                == record.binding.source.transport_generation
                                || host.generation.to_string()
                                    == record.binding.destination.transport_generation
                        })
                })
                .cloned()
                .collect::<Vec<_>>();
            let mut sessions = HashSet::new();
            for record in records.into_iter().rev() {
                if cancel.is_cancelled() {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "handoff recovery cancelled",
                    ));
                }
                if sessions.insert(record.binding.session) {
                    peer.claim_owned(&state, record.binding.session, cancel.clone(), false)
                        .await?;
                }
            }
            Ok(())
        })
        .await
        .map_err(io::Error::other)?
    }
}

struct RemoteProfilePort {
    transport: Weak<PeerTransport>,
}
impl ProfileControlPort for RemoteProfilePort {
    fn call(&self, operation: ProfileOperation) -> BoxFuture<'static, io::Result<ProfileReply>> {
        let peer = self.transport.upgrade();
        Box::pin(async move {
            let peer = peer.ok_or_else(|| unavailable("Profile transport stopped"))?;
            if matches!(operation, ProfileOperation::Snapshot) {
                return peer
                    .observe_profile()
                    .await
                    .map(|(_, view)| ProfileReply::View(view));
            }
            let owner = peer.locate_target(PeerTarget::Profile).await?;
            match peer
                .call_runtime(owner, PeerCall::Profile(operation))
                .await?
            {
                PeerReply::Profile(reply) => Ok(reply),
                _ => Err(invalid("unexpected Profile peer reply")),
            }
        })
    }
}

impl PeerPort for PeerTransport {
    fn identity(&self) -> PeerIdentity {
        self.identity.clone()
    }
    fn locate(&self, target: PeerTarget) -> BoxFuture<'static, io::Result<PeerIdentity>> {
        let state = self.state.clone();
        Box::pin(async move {
            let state = state
                .upgrade()
                .ok_or_else(|| unavailable("peer host stopped"))?;
            state
                .peer_transport
                .get()
                .ok_or_else(|| unavailable("peer transport stopped"))?
                .locate_target(target)
                .await
        })
    }
    fn call(
        &self,
        owner: PeerIdentity,
        call: PeerCall,
    ) -> BoxFuture<'static, io::Result<PeerReply>> {
        let state = self.state.clone();
        Box::pin(async move {
            let state = state
                .upgrade()
                .ok_or_else(|| unavailable("peer host stopped"))?;
            state
                .peer_transport
                .get()
                .ok_or_else(|| unavailable("peer transport stopped"))?
                .call_runtime(owner, call)
                .await
        })
    }
}

impl PeerTransport {
    async fn observe_profile(&self) -> io::Result<(PeerIdentity, ProfileView)> {
        self.observe_profile_from(self.locate_target(PeerTarget::Profile).await?)
            .await
    }

    /// A read can change its sampled owner only after authenticated discovery proves retirement.
    async fn observe_profile_from(
        &self,
        mut owner: PeerIdentity,
    ) -> io::Result<(PeerIdentity, ProfileView)> {
        loop {
            match self
                .call_runtime(owner.clone(), PeerCall::Profile(ProfileOperation::Snapshot))
                .await
            {
                Ok(PeerReply::Profile(ProfileReply::View(view))) => return Ok((owner, view)),
                Ok(_) => return Err(invalid("unexpected remote Profile snapshot")),
                Err(error) => match self.locate_target(PeerTarget::Profile).await {
                    Ok(current) if current != owner => owner = current,
                    Err(current) if current.kind() == io::ErrorKind::NotConnected => {
                        return Err(current);
                    }
                    _ => return Err(error),
                },
            }
        }
    }

    async fn claim_owned(
        &self,
        state: &Arc<DaemonState>,
        session: Uuid,
        cancel: CancellationToken,
        want_ownership: bool,
    ) -> io::Result<Option<SessionHandoffReceipt>> {
        self.load_recovery().await?;
        let pending = self
            .recovery
            .lock()
            .unwrap()
            .values()
            .filter(|record| {
                record.binding.session == session
                    && record.phase >= Phase::Releasing
                    && record.phase < Phase::Completed
            })
            .max_by_key(|record| record.binding.request)
            .cloned();
        if let Some(record) = pending {
            let registrations = self.records().await?;
            let source_live = record.binding.source == self.identity
                || registrations.iter().any(|registered| {
                    registered.generation.to_string() == record.binding.source.transport_generation
                });
            let destination_live = record.binding.destination == self.identity
                || registrations.iter().any(|registered| {
                    registered.generation.to_string()
                        == record.binding.destination.transport_generation
                });
            if !destination_live {
                if source_live && record.binding.source != self.identity {
                    return match self
                        .call_host(
                            record.binding.source.clone(),
                            HostCall::Reassign {
                                original: record.binding.request,
                                destination: self.identity.clone(),
                            },
                        )
                        .await?
                    {
                        HostReply::Recovered(receipt) => {
                            self.load_recovery().await?;
                            Ok(Some(receipt))
                        }
                        _ => Err(invalid("unexpected handoff replacement reply")),
                    };
                }
                return self
                    .recover_transfer(state, record, self.identity.clone())
                    .await
                    .map(Some);
            }
            if source_live {
                match Box::pin(self.call_host(
                    record.binding.source.clone(),
                    HostCall::Resume {
                        original: record.binding.request,
                    },
                ))
                .await?
                {
                    HostReply::Recovered(_) => {}
                    _ => return Err(invalid("unexpected handoff reconciliation reply")),
                }
            } else {
                match self
                    .call_host(
                        record.binding.destination.clone(),
                        HostCall::Confirm(record.binding.clone()),
                    )
                    .await?
                {
                    HostReply::Confirmation(Some(installed))
                        if record
                            .receipt
                            .as_ref()
                            .is_none_or(|expected| expected == &installed) =>
                    {
                        self.persist(
                            &record.binding,
                            Phase::Completed,
                            Some(installed),
                            record.supersedes,
                        )
                        .await?;
                        if let Some(previous) = record.supersedes {
                            let previous = self
                                .recovery
                                .lock()
                                .unwrap()
                                .get(&previous)
                                .cloned()
                                .ok_or_else(|| {
                                    invalid("handoff recovery predecessor is unavailable")
                                })?;
                            self.retire_replaced(&previous, record.binding.request)
                                .await?;
                        }
                    }
                    _ => {
                        return Err(unavailable(
                            "live destination has not confirmed the original handoff",
                        ));
                    }
                }
            }
        }
        if cancel.is_cancelled() {
            let prepared = self
                .recovery
                .lock()
                .unwrap()
                .values()
                .filter(|record| {
                    record.binding.session == session
                        && record.phase == Phase::Prepared
                        && (record.binding.source == self.identity
                            || record.binding.destination == self.identity)
                })
                .max_by_key(|record| record.binding.request)
                .map(|record| record.binding.clone());
            if let Some(prepared) = prepared
                && !self.cancel_prepared(&prepared).await?
            {
                return Box::pin(self.claim_owned(state, session, cancel, false)).await;
            }
            return Err(preparation_cancelled());
        }
        if !want_ownership {
            return Ok(None);
        }
        if state.runtime_host().session(session).is_ok() {
            return Ok(None);
        }
        let source = match self.locate_target(PeerTarget::Session(session)).await {
            Ok(source) => source,
            Err(error) if error.kind() == io::ErrorKind::NotConnected => return Ok(None),
            Err(error) => return Err(error),
        };
        self.transfer(state, session, source, self.identity.clone(), cancel)
            .await
            .map(Some)
    }
    async fn load_recovery(&self) -> io::Result<()> {
        let _operation = self.retain_host_operation()?;
        for record in peer_handoff_store::load(self.identity.canonical_data_root.clone()).await? {
            if !self.domain(&record.binding.source) || !self.domain(&record.binding.destination) {
                return Err(invalid("handoff recovery belongs to another Profile"));
            }
            if self
                .recovery
                .lock()
                .unwrap()
                .get(&record.binding.request)
                .is_some_and(|previous| {
                    previous.binding != record.binding
                        || (previous.receipt.is_some()
                            && record.receipt.is_some()
                            && previous.receipt != record.receipt)
                })
            {
                return Err(invalid(
                    "persisted handoff operation changed its original identity",
                ));
            }
            self.remember(record)?;
        }
        Ok(())
    }

    fn remember(&self, mut record: HandoffRecord) -> io::Result<()> {
        let mut known = self.recovery.lock().unwrap();
        if let Some(previous) = known.get(&record.binding.request) {
            if previous.phase > record.phase {
                record.phase = previous.phase;
            }
            if record.receipt.is_none() {
                record.receipt = previous.receipt.clone();
            }
            if record.replacement.is_none() {
                record.replacement = previous.replacement;
            }
        }
        let relevant =
            record.binding.source == self.identity || record.binding.destination == self.identity;
        if relevant {
            if record.phase < Phase::Completed {
                self.retain_pending(record.binding.request)?;
            } else {
                self.pending_handoffs
                    .lock()
                    .unwrap()
                    .remove(&record.binding.request);
            }
            self.handoffs
                .lock()
                .unwrap()
                .insert(record.binding.request, record.binding.clone());
            if record.phase >= Phase::Completed {
                self.completed_handoffs
                    .lock()
                    .unwrap()
                    .insert(record.binding.request);
            }
        }
        known.insert(record.binding.request, record);
        Ok(())
    }

    async fn persist(
        &self,
        binding: &HandoffBinding,
        phase: Phase,
        receipt: Option<SessionHandoffReceipt>,
        supersedes: Option<Uuid>,
    ) -> io::Result<()> {
        let _operation = self.retain_host_operation()?;
        let (supersedes, replacement) = self
            .recovery
            .lock()
            .unwrap()
            .get(&binding.request)
            .map_or((supersedes, None), |record| {
                (supersedes.or(record.supersedes), record.replacement)
            });
        let record = peer_handoff_store::update(
            self.identity.canonical_data_root.clone(),
            HandoffRecord {
                revision: REVISION,
                binding: binding.clone(),
                phase,
                receipt,
                supersedes,
                replacement,
            },
        )
        .await?;
        self.remember(record)
    }

    async fn retire_replaced(&self, original: &HandoffRecord, replacement: Uuid) -> io::Result<()> {
        let _operation = self.retain_host_operation()?;
        let mut completed = original.clone();
        completed.phase = Phase::Completed;
        completed.replacement = Some(replacement);
        let record =
            peer_handoff_store::update(self.identity.canonical_data_root.clone(), completed)
                .await?;
        self.remember(record)?;
        if let Some(parent) = original.supersedes {
            let previous = self
                .recovery
                .lock()
                .unwrap()
                .get(&parent)
                .cloned()
                .ok_or_else(|| invalid("handoff recovery predecessor is unavailable"))?;
            Box::pin(self.retire_replaced(&previous, original.binding.request)).await?;
        }
        Ok(())
    }
    fn retain_host_operation(&self) -> io::Result<HostOperationToken> {
        self.state
            .upgrade()
            .ok_or_else(|| unavailable("peer host stopped"))?
            .runtime_host()
            .retain_host_operation()
    }

    fn retain_pending(&self, request: Uuid) -> io::Result<()> {
        let mut pending = self.pending_handoffs.lock().unwrap();
        if let std::collections::hash_map::Entry::Vacant(entry) = pending.entry(request) {
            entry.insert(self.retain_host_operation()?);
        }
        Ok(())
    }

    fn pin_coordinator(self: &Arc<Self>) -> io::Result<CoordinatorPin> {
        let operation = self.retain_host_operation()?;
        self.active_coordinators.fetch_add(1, Ordering::AcqRel);
        Ok(CoordinatorPin {
            transport: self.clone(),
            _operation: operation,
        })
    }

    fn report_handoff(&self, binding: &HandoffBinding, role: HostRole) {
        if !self
            .reported_handoffs
            .lock()
            .unwrap()
            .insert(binding.request)
        {
            return;
        }
        let role = match role {
            HostRole::Tui => "TUI",
            HostRole::Daemon => "daemon",
        };
        atman_runtime::notify::global().emit(
            &atman_runtime::notify::Notification::new(
                atman_runtime::notify::NotifyLevel::Info,
                format!(
                    "Session {} taken over by {role} host {} (request {})",
                    binding.session, binding.destination.runtime_generation, binding.request
                ),
            )
            .with_location(atman_runtime::notify::NotifyLocation::Log)
            .with_target("host-handoff"),
        );
    }
    fn domain(&self, other: &PeerIdentity) -> bool {
        other.profile_id == self.identity.profile_id
            && other.canonical_data_root == self.identity.canonical_data_root
            && !other.runtime_generation.is_nil()
            && Uuid::parse_str(&other.transport_generation).is_ok_and(|id| !id.is_nil())
    }

    pub(crate) fn distinct_user_credential(&self, token: &str) -> io::Result<()> {
        if crate::http::constant_time_eq(token.as_bytes(), self.credential.0.as_bytes()) {
            Err(invalid("User and peer credentials must be independent"))
        } else {
            Ok(())
        }
    }

    async fn records(&self) -> io::Result<Vec<HostRecord>> {
        let data = self.identity.canonical_data_root.clone();
        tokio::task::spawn_blocking(move || host_directory::discover(&data))
            .await
            .map_err(io::Error::other)?
    }

    async fn handshake(&self, record: HostRecord) -> io::Result<PeerDescription> {
        let request = Uuid::new_v4();
        let payload = Handshake {
            revision: REVISION,
            request,
            caller: self.identity.clone(),
            expected_transport_generation: record.generation.to_string(),
        };
        let response = self
            .client
            .post(format!("http://{}{HANDSHAKE_PATH}", record.endpoint))
            .bearer_auth(&self.credential.0)
            .json(&payload)
            .send()
            .await
            .map_err(io::Error::other)?;
        let reply: Reply<PeerDescription> = read_reply(response).await?;
        if reply.revision != REVISION
            || reply.request != request
            || reply.responder.transport_generation != record.generation.to_string()
            || !self.domain(&reply.responder)
        {
            return Err(invalid("peer handshake identity changed"));
        }
        let description = reply.result.map_err(decode_error)?;
        if description.identity != reply.responder
            || description.authority_revision != REVISION
            || description.role != record.role
        {
            return Err(invalid(
                "peer does not implement the required authority protocol",
            ));
        }
        self.verified.lock().unwrap().insert(
            description.identity.transport_generation.clone(),
            VerifiedPeer {
                identity: description.identity.clone(),
                record,
                outbound: true,
            },
        );
        Ok(description)
    }

    async fn descriptions(&self) -> io::Result<Vec<(HostRecord, PeerDescription)>> {
        let mut descriptions = Vec::new();
        for record in self.records().await? {
            if record.generation.to_string() == self.identity.transport_generation {
                continue;
            }
            if let Ok(description) = self.handshake(record.clone()).await {
                descriptions.push((record, description));
            }
        }
        Ok(descriptions)
    }

    async fn authenticate_caller(&self, caller: &PeerIdentity) -> io::Result<()> {
        let record = self
            .records()
            .await?
            .into_iter()
            .find(|record| record.generation.to_string() == caller.transport_generation)
            .ok_or_else(|| unavailable("calling peer generation stopped"))?;
        let cached = self
            .verified
            .lock()
            .unwrap()
            .get(&caller.transport_generation)
            .is_some_and(|verified| {
                verified.outbound
                    && &verified.identity == caller
                    && same_registration(&verified.record, &record)
            });
        if cached {
            return Ok(());
        }
        let actual = self.handshake(record).await?;
        if &actual.identity != caller {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "calling peer runtime identity changed",
            ));
        }
        Ok(())
    }

    async fn locate_target(&self, target: PeerTarget) -> io::Result<PeerIdentity> {
        let state = self
            .state
            .upgrade()
            .ok_or_else(|| unavailable("peer host stopped"))?;
        let local = match &target {
            PeerTarget::Session(id) => state.runtime_host().session(*id).is_ok(),
            PeerTarget::Profile => state.runtime_host().profile_handle().is_some(),
            PeerTarget::Runtime(id) => *id == self.identity.runtime_generation,
        };
        if local {
            return Ok(self.identity.clone());
        }
        let mut matches = self
            .descriptions()
            .await?
            .into_iter()
            .filter_map(|(_, description)| {
                let matches = match target {
                    PeerTarget::Session(id) => description.sessions.contains(&id),
                    PeerTarget::Profile => description.profile_owner,
                    PeerTarget::Runtime(id) => description.identity.runtime_generation == id,
                };
                matches.then_some(description.identity)
            });
        let owner = matches
            .next()
            .ok_or_else(|| unavailable("no verified peer owns the requested authority"))?;
        if matches.next().is_some() {
            return Err(invalid("multiple peers advertise the same authority owner"));
        }
        Ok(owner)
    }

    async fn call_runtime(&self, owner: PeerIdentity, call: PeerCall) -> io::Result<PeerReply> {
        if owner == self.identity {
            let state = self
                .state
                .upgrade()
                .ok_or_else(|| unavailable("peer host stopped"))?;
            return handle_peer_call(state.runtime_host(), self.identity.clone(), call).await;
        }
        match self.call_host(owner, HostCall::Runtime(call)).await? {
            HostReply::Runtime(reply) => Ok(*reply),
            _ => Err(invalid("unexpected runtime peer reply")),
        }
    }

    async fn call_host(&self, owner: PeerIdentity, operation: HostCall) -> io::Result<HostReply> {
        if !self.domain(&owner) {
            return Err(invalid("peer belongs to another authority domain"));
        }
        if owner == self.identity {
            return self.dispatch_host(self.identity.clone(), operation).await;
        }
        let record = self
            .records()
            .await?
            .into_iter()
            .find(|record| record.generation.to_string() == owner.transport_generation)
            .ok_or_else(|| unavailable("peer generation is no longer registered"))?;
        let cached = self
            .verified
            .lock()
            .unwrap()
            .get(&owner.transport_generation)
            .is_some_and(|verified| {
                verified.outbound
                    && verified.identity == owner
                    && same_registration(&verified.record, &record)
            });
        if !cached && self.handshake(record.clone()).await?.identity != owner {
            return Err(unavailable("peer owner generation changed"));
        }
        let request = Uuid::new_v4();
        let envelope = Envelope {
            revision: REVISION,
            request,
            caller: self.identity.clone(),
            expected: owner.clone(),
            operation,
        };
        let response = self
            .client
            .post(format!("http://{}{CALL_PATH}", record.endpoint))
            .bearer_auth(&self.credential.0)
            .json(&envelope)
            .send()
            .await
            .map_err(io::Error::other)?;
        let reply: Reply<HostReply> = read_reply(response).await?;
        if reply.revision != REVISION || reply.request != request || reply.responder != owner {
            return Err(invalid("peer acknowledgement identity changed"));
        }
        reply.result.map_err(decode_error)
    }

    async fn dispatch_host(
        &self,
        caller: PeerIdentity,
        operation: HostCall,
    ) -> io::Result<HostReply> {
        let state = self
            .state
            .upgrade()
            .ok_or_else(|| unavailable("peer host stopped"))?;
        let host = state.runtime_host();
        match operation {
            HostCall::Runtime(call) => handle_peer_call(host, caller, call)
                .await
                .map(|reply| HostReply::Runtime(Box::new(reply))),
            HostCall::Prepare(binding) => {
                self.bind_handoff(&caller, &binding, false)?;
                self.load_recovery().await?;
                if !self.recovery.lock().unwrap().contains_key(&binding.request) {
                    host.session(binding.session)?;
                }
                self.persist(&binding, Phase::Prepared, None, None).await?;
                Ok(HostReply::Prepared)
            }
            HostCall::CancelPrepared(binding) => {
                self.bind_handoff(&caller, &binding, false)?;
                self.persist(&binding, Phase::Aborted, None, None).await?;
                Ok(HostReply::Aborted)
            }
            HostCall::Release(binding) => {
                self.bind_handoff(&caller, &binding, false)?;
                // Another host may have acknowledged an installation held by this destination.
                self.load_recovery().await?;
                let unsettled = self.recovery.lock().unwrap().values().any(|record| {
                    record.binding.session == binding.session
                        && record.binding.destination == self.identity
                        && record.binding.request != binding.request
                        && record.phase >= Phase::Releasing
                        && record.phase < Phase::Completed
                });
                if unsettled {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "previous Session installation must be confirmed before another handoff",
                    ));
                }
                self.persist(&binding, Phase::Releasing, None, None).await?;
                let outcome = host
                    .handoff_session(binding.session, binding.request)
                    .await?;
                match &outcome {
                    SessionHandoffOutcome::Released { receipt } => {
                        self.persist(&binding, Phase::Released, Some(receipt.clone()), None)
                            .await?
                    }
                    SessionHandoffOutcome::Pending { .. } => {}
                }
                Ok(HostReply::Released(outcome))
            }
            HostCall::Acquire { binding, receipt } => {
                self.bind_handoff(&caller, &binding, true)?;
                if receipt.session_id != binding.session
                    || receipt.request_id != binding.request
                    || receipt.source_generation != binding.source.runtime_generation
                {
                    return Err(invalid(
                        "handoff receipt does not match its prepared operation",
                    ));
                }
                crate::flow_service::install_handoff(&state, &receipt).await?;
                let installed = host
                    .installed_handoff(
                        binding.session,
                        binding.request,
                        binding.source.runtime_generation,
                    )?
                    .ok_or_else(|| unavailable("handoff was not installed"))?;
                if installed != receipt {
                    return Err(invalid("installed handoff receipt changed"));
                }
                self.persist(&binding, Phase::Installed, Some(installed.clone()), None)
                    .await?;
                Ok(HostReply::Installed(installed))
            }
            HostCall::Confirm(binding) => {
                self.bind_handoff(&caller, &binding, true)?;
                host.installed_handoff(
                    binding.session,
                    binding.request,
                    binding.source.runtime_generation,
                )
                .map(HostReply::Confirmation)
            }
            HostCall::Acknowledge { binding, receipt } => {
                self.bind_handoff(&caller, &binding, false)?;
                let original = match host
                    .handoff_session(binding.session, binding.request)
                    .await?
                {
                    SessionHandoffOutcome::Released { receipt } => receipt,
                    _ => return Err(invalid("handoff source has not released ownership")),
                };
                if original != receipt {
                    return Err(invalid(
                        "handoff acknowledgement changed the original receipt",
                    ));
                }
                match Box::pin(self.call_host(
                    binding.destination.clone(),
                    HostCall::Confirm(binding.clone()),
                ))
                .await?
                {
                    HostReply::Confirmation(Some(installed)) if installed == receipt => {}
                    _ => {
                        return Err(unavailable(
                            "handoff destination has not confirmed installation",
                        ));
                    }
                }
                self.persist(&binding, Phase::Completed, Some(receipt.clone()), None)
                    .await?;
                let destination_role = if binding.destination == self.identity {
                    Some(state.host_role())
                } else {
                    self.verified
                        .lock()
                        .unwrap()
                        .get(&binding.destination.transport_generation)
                        .filter(|peer| peer.identity == binding.destination)
                        .map(|peer| peer.record.role)
                };
                if let Some(role) = destination_role {
                    self.report_handoff(&binding, role);
                }
                if !matches!(
                    Box::pin(self.call_host(
                        binding.destination.clone(),
                        HostCall::Complete { binding, receipt }
                    ))
                    .await?,
                    HostReply::Acknowledged
                ) {
                    return Err(invalid(
                        "destination did not reconcile the original source acknowledgement",
                    ));
                }
                Ok(HostReply::Acknowledged)
            }
            HostCall::Complete { binding, receipt } => {
                self.bind_handoff(&caller, &binding, true)?;
                self.load_recovery().await?;
                let original = self
                    .recovery
                    .lock()
                    .unwrap()
                    .get(&binding.request)
                    .cloned()
                    .ok_or_else(|| unavailable("source acknowledgement is not recorded"))?;
                if original.binding != binding
                    || original.phase != Phase::Completed
                    || original.receipt.as_ref() != Some(&receipt)
                {
                    return Err(invalid(
                        "source has not acknowledged this exact installation",
                    ));
                }
                Ok(HostReply::Acknowledged)
            }
            HostCall::Resume { original } => {
                self.load_recovery().await?;
                let mut record = self
                    .recovery
                    .lock()
                    .unwrap()
                    .get(&original)
                    .cloned()
                    .ok_or_else(|| unavailable("original handoff operation is unavailable"))?;
                if record.phase == Phase::Aborted {
                    return Err(preparation_cancelled());
                }
                if record.binding.source != self.identity {
                    return Err(invalid("handoff reconciliation belongs to another source"));
                }
                if record.phase == Phase::Completed {
                    let mut seen = HashSet::new();
                    while let Some(replacement) = record.replacement {
                        if !seen.insert(replacement) {
                            return Err(invalid("handoff replacement chain contains a cycle"));
                        }
                        record = self
                            .recovery
                            .lock()
                            .unwrap()
                            .get(&replacement)
                            .cloned()
                            .ok_or_else(|| {
                                unavailable("handoff replacement receipt is unavailable")
                            })?;
                    }
                    return record
                        .receipt
                        .map(HostReply::Recovered)
                        .ok_or_else(|| unavailable("completed handoff receipt is unavailable"));
                }
                let _pin = state.peer_transport.get().unwrap().pin_coordinator()?;
                Box::pin(self.transfer(
                    &state,
                    record.binding.session,
                    record.binding.source,
                    record.binding.destination,
                    CancellationToken::new(),
                ))
                .await
                .map(HostReply::Recovered)
            }
            HostCall::Reassign {
                original,
                destination,
            } => {
                self.load_recovery().await?;
                let record = self
                    .recovery
                    .lock()
                    .unwrap()
                    .get(&original)
                    .cloned()
                    .ok_or_else(|| unavailable("original handoff operation is unavailable"))?;
                if record.phase == Phase::Aborted {
                    return Err(preparation_cancelled());
                }
                if record.binding.source != self.identity
                    || (caller != destination && caller != self.identity)
                {
                    return Err(invalid(
                        "handoff replacement is not bound to its source or destination",
                    ));
                }
                if record.phase == Phase::Completed {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "original handoff was already confirmed; discover the current owner",
                    ));
                }
                let _pin = state.peer_transport.get().unwrap().pin_coordinator()?;
                Box::pin(self.recover_transfer(&state, record, destination))
                    .await
                    .map(HostReply::Recovered)
            }
            HostCall::RecoveryAcquire { original, binding } => {
                self.load_recovery().await?;
                let previous = self
                    .recovery
                    .lock()
                    .unwrap()
                    .get(&original)
                    .cloned()
                    .ok_or_else(|| unavailable("original recovery operation is unavailable"))?;
                if previous.phase == Phase::Aborted {
                    return Err(preparation_cancelled());
                }
                if binding.request == original
                    || binding.session != previous.binding.session
                    || binding.source != previous.binding.source
                    || binding.destination != self.identity
                    || (caller != binding.source && caller != self.identity)
                {
                    return Err(invalid(
                        "replacement handoff identities do not match the original operation",
                    ));
                }
                let registrations = self.records().await?;
                if registrations.iter().any(|record| {
                    record.generation.to_string()
                        == previous.binding.destination.transport_generation
                }) {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "previous destination generation is still registered",
                    ));
                }
                if caller == self.identity
                    && previous.binding.source != self.identity
                    && registrations.iter().any(|record| {
                        record.generation.to_string()
                            == previous.binding.source.transport_generation
                    })
                {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "live original source must coordinate its recovery",
                    ));
                }
                self.bind_handoff(&caller, &binding, true)?;
                self.persist(&binding, Phase::Releasing, None, Some(original))
                    .await?;
                let receipt = host
                    .recover_handoff(
                        state.data_dir(),
                        binding.session,
                        binding.source.runtime_generation,
                        binding.request,
                        previous.receipt.as_ref(),
                    )
                    .await?;
                self.persist(
                    &binding,
                    Phase::Installed,
                    Some(receipt.clone()),
                    Some(original),
                )
                .await?;
                Ok(HostReply::Installed(receipt))
            }
        }
    }

    fn bind_handoff(
        &self,
        caller: &PeerIdentity,
        binding: &HandoffBinding,
        destination: bool,
    ) -> io::Result<()> {
        if binding.request.is_nil()
            || binding.session.is_nil()
            || !self.domain(&binding.source)
            || !self.domain(&binding.destination)
            || (binding.source.runtime_generation == binding.destination.runtime_generation
                && self
                    .recovery
                    .lock()
                    .unwrap()
                    .get(&binding.request)
                    .is_none_or(|record| record.supersedes.is_none()))
            || (destination && binding.destination != self.identity)
            || (!destination && binding.source != self.identity)
            || (caller != &binding.source && caller != &binding.destination)
        {
            return Err(invalid("handoff host identities are invalid"));
        }
        let mut requests = self.handoffs.lock().unwrap();
        match requests.get(&binding.request) {
            Some(original) if original != binding => {
                Err(invalid("handoff request identity was reused"))
            }
            Some(_) => Ok(()),
            None => {
                requests.insert(binding.request, binding.clone());
                Ok(())
            }
        }
    }

    async fn transfer(
        &self,
        state: &Arc<DaemonState>,
        session: Uuid,
        source: PeerIdentity,
        destination: PeerIdentity,
        cancel: CancellationToken,
    ) -> io::Result<SessionHandoffReceipt> {
        let previous = self
            .recovery
            .lock()
            .unwrap()
            .values()
            .filter(|record| {
                record.binding.session == session
                    && record.binding.source == source
                    && record.phase >= Phase::Releasing
                    && record.phase < Phase::Completed
            })
            .max_by_key(|record| record.binding.request)
            .cloned();
        if let Some(previous) = previous {
            if previous.binding.destination != destination {
                if !self.records().await?.iter().any(|record| {
                    record.generation.to_string()
                        == previous.binding.destination.transport_generation
                }) {
                    return Box::pin(self.recover_transfer(state, previous, destination)).await;
                }
                return Box::pin(self.transfer(
                    state,
                    session,
                    source,
                    previous.binding.destination,
                    cancel,
                ))
                .await;
            }
        }
        let binding = {
            let requests = self.handoffs.lock().unwrap();
            let completed = self.completed_handoffs.lock().unwrap();
            requests
                .values()
                .find(|binding| {
                    binding.session == session
                        && binding.source == source
                        && binding.destination == destination
                        && !completed.contains(&binding.request)
                })
                .cloned()
        }
        .unwrap_or_else(|| HandoffBinding {
            request: Uuid::now_v7(),
            session,
            source,
            destination,
        });
        self.transfer_binding(state, binding, cancel).await
    }

    async fn transfer_binding(
        &self,
        state: &Arc<DaemonState>,
        binding: HandoffBinding,
        cancel: CancellationToken,
    ) -> io::Result<SessionHandoffReceipt> {
        loop {
            match self
                .transfer_binding_once(state, binding.clone(), cancel.clone())
                .await
            {
                Err(error)
                    if error.kind() == io::ErrorKind::NotConnected
                        || error.get_ref().is_some_and(|cause| {
                            cause.is::<reqwest::Error>()
                                || cause
                                    .downcast_ref::<serde_json::Error>()
                                    .is_some_and(serde_json::Error::is_eof)
                        }) =>
                {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                result => return result,
            }
        }
    }

    async fn transfer_binding_once(
        &self,
        state: &Arc<DaemonState>,
        binding: HandoffBinding,
        cancel: CancellationToken,
    ) -> io::Result<SessionHandoffReceipt> {
        self.load_recovery().await?;
        let original = self.recovery.lock().unwrap().get(&binding.request).cloned();
        if let Some(record) = original {
            if record.binding != binding {
                return Err(invalid("handoff request identity changed"));
            }
            if record.phase == Phase::Aborted {
                return Err(preparation_cancelled());
            }
            if record.phase == Phase::Completed {
                if record.replacement.is_some() {
                    return Err(invalid("handoff was recovered by a different operation"));
                }
                let receipt = record
                    .receipt
                    .ok_or_else(|| invalid("completed handoff has no original receipt"))?;
                if !matches!(
                    self.call_host(
                        binding.destination.clone(),
                        HostCall::Complete {
                            binding,
                            receipt: receipt.clone()
                        }
                    )
                    .await?,
                    HostReply::Acknowledged
                ) {
                    return Err(invalid(
                        "destination did not reconcile the original source acknowledgement",
                    ));
                }
                return Ok(receipt);
            }
        }
        self.handoffs
            .lock()
            .unwrap()
            .insert(binding.request, binding.clone());
        let accepted = self
            .recovery
            .lock()
            .unwrap()
            .get(&binding.request)
            .is_some_and(|record| record.phase >= Phase::Releasing);
        if cancel.is_cancelled() && !accepted {
            let prepared = self.recovery.lock().unwrap().contains_key(&binding.request);
            let uncertain = self
                .pending_handoffs
                .lock()
                .unwrap()
                .contains_key(&binding.request);
            if (!prepared && !uncertain) || (prepared && self.cancel_prepared(&binding).await?) {
                return Err(preparation_cancelled());
            }
        }
        self.retain_pending(binding.request)?;
        match self
            .call_host(binding.source.clone(), HostCall::Prepare(binding.clone()))
            .await
        {
            Ok(HostReply::Prepared) => {}
            Err(error) => {
                self.load_recovery().await?;
                if error.kind() == io::ErrorKind::NotFound
                    && !self.recovery.lock().unwrap().contains_key(&binding.request)
                {
                    self.pending_handoffs
                        .lock()
                        .unwrap()
                        .remove(&binding.request);
                }
                return Err(error);
            }
            Ok(_) => return Err(invalid("unexpected handoff preparation reply")),
        }
        self.load_recovery().await?;
        if cancel.is_cancelled() && !accepted && self.cancel_prepared(&binding).await? {
            return Err(preparation_cancelled());
        }
        let receipt = loop {
            match self
                .call_host(binding.source.clone(), HostCall::Release(binding.clone()))
                .await?
            {
                HostReply::Released(SessionHandoffOutcome::Released { receipt }) => break receipt,
                HostReply::Released(SessionHandoffOutcome::Pending { .. }) => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                _ => return Err(invalid("unexpected handoff release reply")),
            }
        };
        // Release is irreversible here. Continue the original owned installation even if the UI waiter closes.
        match self
            .call_host(
                binding.destination.clone(),
                HostCall::Acquire {
                    binding: binding.clone(),
                    receipt: receipt.clone(),
                },
            )
            .await?
        {
            HostReply::Installed(installed) if installed == receipt => {}
            _ => return Err(invalid("destination did not install the exact handoff")),
        }
        match self
            .call_host(
                binding.destination.clone(),
                HostCall::Confirm(binding.clone()),
            )
            .await?
        {
            HostReply::Confirmation(Some(installed)) if installed == receipt => {}
            _ => {
                return Err(unavailable(
                    "destination handoff confirmation is unavailable",
                ));
            }
        }
        if !matches!(
            self.call_host(
                binding.source.clone(),
                HostCall::Acknowledge {
                    binding: binding.clone(),
                    receipt: receipt.clone()
                }
            )
            .await?,
            HostReply::Acknowledged
        ) {
            return Err(invalid("source handoff acknowledgement is unavailable"));
        }
        self.persist(&binding, Phase::Completed, Some(receipt.clone()), None)
            .await?;
        let _ = state;
        Ok(receipt)
    }

    async fn cancel_prepared(&self, binding: &HandoffBinding) -> io::Result<bool> {
        let result = self
            .call_host(
                binding.source.clone(),
                HostCall::CancelPrepared(binding.clone()),
            )
            .await;
        match result {
            Ok(HostReply::Aborted) => {
                self.load_recovery().await?;
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                self.load_recovery().await?;
                Ok(false)
            }
            Err(error) => Err(error),
            Ok(_) => Err(invalid("unexpected handoff cancellation reply")),
        }
    }

    async fn recover_transfer(
        &self,
        state: &Arc<DaemonState>,
        mut original: HandoffRecord,
        destination: PeerIdentity,
    ) -> io::Result<SessionHandoffReceipt> {
        if original.binding.source == self.identity {
            let mut base = original.clone();
            let mut seen = HashSet::new();
            while let Some(previous) = base.supersedes {
                if !seen.insert(previous) {
                    return Err(invalid("handoff recovery ancestry contains a cycle"));
                }
                base = self
                    .recovery
                    .lock()
                    .unwrap()
                    .get(&previous)
                    .cloned()
                    .ok_or_else(|| invalid("handoff recovery predecessor is unavailable"))?;
            }
            loop {
                match state
                    .runtime_host()
                    .handoff_session(base.binding.session, base.binding.request)
                    .await?
                {
                    SessionHandoffOutcome::Released { receipt } => {
                        if original.binding.request == base.binding.request {
                            self.persist(
                                &original.binding,
                                Phase::Released,
                                Some(receipt.clone()),
                                original.supersedes,
                            )
                            .await?;
                            original.receipt = Some(receipt);
                            original.phase = Phase::Released;
                        }
                        break;
                    }
                    SessionHandoffOutcome::Pending { .. } => {
                        tokio::time::sleep(Duration::from_millis(100)).await
                    }
                }
            }
        }
        if !self.domain(&destination) || original.phase < Phase::Releasing {
            return Err(invalid(
                "handoff recovery is not bound to an attempted release",
            ));
        }
        if self.records().await?.iter().any(|record| {
            record.generation.to_string() == original.binding.destination.transport_generation
        }) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "previous destination is still registered",
            ));
        }
        let replacement = self
            .recovery
            .lock()
            .unwrap()
            .values()
            .find(|record| {
                record.supersedes == Some(original.binding.request)
                    && record.binding.destination == destination
            })
            .cloned();
        let binding = replacement
            .map(|record| record.binding)
            .unwrap_or_else(|| HandoffBinding {
                request: Uuid::now_v7(),
                session: original.binding.session,
                source: original.binding.source.clone(),
                destination,
            });
        self.persist(
            &binding,
            Phase::Releasing,
            None,
            Some(original.binding.request),
        )
        .await?;
        let receipt = match Box::pin(self.call_host(
            binding.destination.clone(),
            HostCall::RecoveryAcquire {
                original: original.binding.request,
                binding: binding.clone(),
            },
        ))
        .await?
        {
            HostReply::Installed(receipt) => receipt,
            _ => return Err(invalid("replacement did not install recovered ownership")),
        };
        match self
            .call_host(
                binding.destination.clone(),
                HostCall::Confirm(binding.clone()),
            )
            .await?
        {
            HostReply::Confirmation(Some(installed)) if installed == receipt => {}
            _ => {
                return Err(unavailable(
                    "replacement has not confirmed its recovered receipt",
                ));
            }
        }
        self.persist(
            &binding,
            Phase::Completed,
            Some(receipt.clone()),
            Some(original.binding.request),
        )
        .await?;
        self.retire_replaced(&original, binding.request).await?;
        let _ = state;
        Ok(receipt)
    }
}

async fn read_reply<T: DeserializeOwned>(response: reqwest::Response) -> io::Result<T> {
    if !response.status().is_success() {
        return Err(unavailable("peer transport rejected the request"));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_BYTES as u64)
    {
        return Err(invalid("peer reply exceeds the bounded transport size"));
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(io::Error::other)?;
        if bytes.len().saturating_add(chunk.len()) > MAX_BYTES {
            return Err(invalid("peer reply exceeds the bounded transport size"));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

pub(crate) fn router(state: Arc<DaemonState>) -> Router {
    Router::new()
        .route(HANDSHAKE_PATH, post(handshake_handler))
        .route(CALL_PATH, post(call_handler))
        .layer(DefaultBodyLimit::max(MAX_BYTES))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_peer))
        .with_state(state)
}

async fn require_peer(
    State(state): State<Arc<DaemonState>>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let Some(peer) = state.peer_transport.get() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let valid = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|token| {
            crate::http::constant_time_eq(token.as_bytes(), peer.credential.0.as_bytes())
        });
    if !valid {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(request).await
}

async fn handshake_handler(
    State(state): State<Arc<DaemonState>>,
    Json(request): Json<Handshake>,
) -> Response {
    let peer = state.peer_transport.get().unwrap();
    let result = async {
        if request.revision != REVISION
            || request.request.is_nil()
            || request.expected_transport_generation != peer.identity.transport_generation
            || !peer.domain(&request.caller)
        {
            return Err(invalid(
                "peer handshake domain, protocol or generation changed",
            ));
        }
        let record = peer
            .records()
            .await?
            .into_iter()
            .find(|record| record.generation.to_string() == request.caller.transport_generation)
            .ok_or_else(|| unavailable("calling peer generation is not registered"))?;
        let mut verified = peer.verified.lock().unwrap();
        verified
            .entry(request.caller.transport_generation.clone())
            .or_insert(VerifiedPeer {
                identity: request.caller,
                record,
                outbound: false,
            });
        Ok(PeerDescription {
            identity: peer.identity.clone(),
            sessions: state.runtime_host().sessions(),
            profile_owner: state.runtime_host().profile_handle().is_some(),
            role: state.host_role(),
            presentation: state.presentation(),
            authority_revision: REVISION,
            pending_sessions: peer
                .recovery
                .lock()
                .unwrap()
                .values()
                .filter(|record| {
                    record.binding.source == peer.identity
                        && record.phase >= Phase::Releasing
                        && record.phase < Phase::Completed
                })
                .map(|record| record.binding.session)
                .collect(),
        })
    }
    .await;
    Json(Reply {
        revision: REVISION,
        request: request.request,
        responder: peer.identity.clone(),
        result: result.map_err(encode_error),
    })
    .into_response()
}

async fn call_handler(
    State(state): State<Arc<DaemonState>>,
    Json(request): Json<Envelope<HostCall>>,
) -> Response {
    let peer = state.peer_transport.get().unwrap().clone();
    let request_id = request.request;
    let responder = peer.identity.clone();
    let result = async {
        if request.revision != REVISION
            || request.request.is_nil()
            || request.expected != peer.identity
            || !peer.domain(&request.caller)
        {
            return Err(invalid(
                "peer request domain, protocol or generation changed",
            ));
        }
        peer.authenticate_caller(&request.caller).await?;
        let peer = peer.clone();
        let retained = (!matches!(&request.operation, HostCall::Runtime(_)))
            .then(|| peer.pin_coordinator())
            .transpose()?;
        tokio::spawn(async move {
            let _retained = retained;
            peer.dispatch_host(request.caller, request.operation).await
        })
        .await
        .map_err(io::Error::other)?
    }
    .await;
    Json(Reply {
        revision: REVISION,
        request: request_id,
        responder,
        result: result.map_err(encode_error),
    })
    .into_response()
}

fn same_registration(left: &HostRecord, right: &HostRecord) -> bool {
    left.generation == right.generation
        && left.endpoint == right.endpoint
        && left.process_id == right.process_id
        && left.role == right.role
}

fn load_credential(data: &Path) -> io::Result<(PathBuf, String)> {
    let data = data.canonicalize()?;
    let run = data.join("run");
    credential_directory(&run, false)?;
    let root = run.join("peer");
    credential_directory(&root, true)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let lock_path = root.join("credential.lock");
    let file = options.create(true).truncate(false).open(&lock_path)?;
    private_file(&lock_path, &file)?;
    let _lock = ExclusiveFileLease::try_acquire(file)?;
    let path = root.join("credential");
    let mut file = match options.create_new(true).open(&path) {
        Ok(mut file) => {
            let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
            file.write_all(token.as_bytes())?;
            file.sync_all()?;
            file
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            options.create_new(false).create(false).open(&path)?
        }
        Err(error) => return Err(error),
    };
    private_file(&path, &file)?;
    file.rewind()?;
    let mut token = String::new();
    file.take(65).read_to_string(&mut token)?;
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid("peer credential is incomplete or invalid"));
    }
    Ok((data, token))
}

pub(crate) fn credential_directory(path: &Path, private: bool) -> io::Result<()> {
    let mut options = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        options.mode(0o700);
    }
    match options.create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(invalid(
            "peer credential directory must be a regular local directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & if private { 0o077 } else { 0o022 } != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "peer credential directory is not private",
            ));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(invalid(
                "peer credential directory must not be a reparse point",
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = private;
    Ok(())
}

pub(crate) fn private_file(path: &Path, file: &fs::File) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || file.metadata()?.len() > 1024 {
        return Err(invalid("peer credential must be a regular private file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.nlink() != 1
            || metadata.permissions().mode() & 0o077 != 0
            || metadata.ino() != file.metadata()?.ino()
            || metadata.dev() != file.metadata()?.dev()
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "peer credential file is not private",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EmbeddedRpcServer, SharedHostServices};
    use atman_runtime::{flow_runtime::host::RuntimeHost, flow_store::SessionDescriptor};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    const USER_TOKEN: &str = "peer-fixture-user-credential";

    async fn serve(
        state: &Arc<DaemonState>,
    ) -> (EmbeddedRpcServer, host_directory::HostRegistration) {
        state.prepare_peer_transport().await.unwrap();
        let server = EmbeddedRpcServer::bind(state.clone(), USER_TOKEN.into())
            .await
            .unwrap();
        let registration = host_directory::HostRegistration::register(
            state.data_dir(),
            HostRecord {
                generation: state.daemon_generation().parse().unwrap(),
                process_id: std::process::id(),
                role: state.host_role(),
                endpoint: server.endpoint(),
                sessions: state.runtime_host().sessions(),
                presentation: state.presentation(),
            },
        )
        .unwrap();
        state.ensure_profile_port().await.unwrap();
        (server, registration)
    }

    async fn advance_fixture(
        state: &DaemonState,
        session: Uuid,
    ) -> atman_runtime::flow_runtime::FlowWorker {
        let worker = state
            .runtime_host()
            .session(session)
            .unwrap()
            .create_flow(atman_runtime::flow_store::FlowDescriptor {
                address: atman_runtime::flow_store::FlowRef {
                    session_id: session.into(),
                    instance_id: atman_rt::FlowInstanceId::now(),
                },
                creation_request: Uuid::now_v7(),
                created_by: None,
                content: atman_runtime::flow_store::EntrySnapshot {
                    name: "main".into(),
                    path: "main.at".into(),
                    bundle: atman_runtime::source_program::RevisionBundle {
                        entry: "entry:main.at".into(),
                        edges: Default::default(),
                        sources: [(
                            "entry:main.at".into(),
                            "flow main(input: string) -> string { return input }".into(),
                        )]
                        .into(),
                    },
                }
                .into(),
            })
            .await
            .unwrap();
        worker
            .transition(0, atman_runtime::flow_store::FlowTransition::Activated)
            .await
            .unwrap();
        worker
    }

    #[test]
    fn private_credentials_are_stable_and_separate_from_user_configuration() {
        let root = tempfile::tempdir().unwrap();
        let (domain, first) = load_credential(root.path()).unwrap();
        let (_, second) = load_credential(root.path()).unwrap();
        assert_eq!(domain, root.path().canonicalize().unwrap());
        assert_eq!(first, second);
        assert_ne!(first, USER_TOKEN);
        assert!(!root.path().join("daemon.toml").exists());
        let path = root.path().join("run/peer/credential");
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let original = fs::metadata(&path).unwrap();
            assert_eq!(original.permissions().mode() & 0o077, 0);
            load_credential(root.path()).unwrap();
            assert_eq!(fs::metadata(&path).unwrap().ino(), original.ino());
        }
        fs::write(path, b"incomplete").unwrap();
        assert_eq!(
            load_credential(root.path()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[tokio::test]
    async fn profile_observation_reconciles_retirement_after_authenticated_discovery() {
        let root = tempfile::tempdir().unwrap();
        let source = Arc::new(DaemonState::new(root.path().to_owned()));
        let (source_server, source_registration) = serve(&source).await;
        let identity = source
            .runtime_host()
            .profile_handle()
            .unwrap()
            .snapshot()
            .await
            .unwrap()
            .profile
            .profile_id;
        let host = RuntimeHost::default();
        let target = Arc::new(DaemonState::with_shared_host(
            root.path().to_owned(),
            Uuid::now_v7().to_string(),
            SharedHostServices {
                host: host.clone(),
                permissions: Arc::new(atman_runtime::permission::PermissionClients::default()),
                operator: Uuid::from_bytes(*b"atman-local-user"),
                prompts: None,
            },
        ));
        let (server, _registration) = serve(&target).await;
        let cached = host.profile_control_port().unwrap();
        assert!(!cached.is_local_owner());
        let peer = target.peer_transport.get().unwrap();
        let sampled = peer.locate_target(PeerTarget::Profile).await.unwrap();
        assert_eq!(sampled, source.peer_transport.get().unwrap().identity);
        // The authenticated discovery succeeded, but the original listener retires before Snapshot.
        source_server.shutdown().await.unwrap();
        source.shutdown().await.unwrap();
        assert_eq!(
            peer.observe_profile_from(sampled).await.unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
        let recovered = target.ensure_profile_port().await.unwrap();
        assert!(recovered.is_local_owner());
        assert_eq!(
            recovered.snapshot().await.unwrap().profile.profile_id,
            identity
        );
        assert_eq!(
            cached.snapshot().await.unwrap().profile.profile_id,
            identity
        );
        assert!(
            ProfileStore::open(
                &atman_runtime::flow_store::RuntimeStore::open(root.path()).unwrap()
            )
            .is_err()
        );
        drop(source_registration);
        server.shutdown().await.unwrap();
        host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn prepared_handoff_retains_idle_host_after_the_http_coordinator_returns() {
        let root = tempfile::tempdir().unwrap();
        let source = Arc::new(DaemonState::new(root.path().to_owned()));
        let (source_server, _source_registration) = serve(&source).await;
        let session = Uuid::now_v7();
        source
            .runtime_host()
            .create_session(
                root.path(),
                SessionDescriptor {
                    id: session,
                    title: "Prepared handoff".into(),
                    project: None,
                    configuration: Default::default(),
                },
            )
            .await
            .unwrap();
        let host = RuntimeHost::default();
        let target = Arc::new(DaemonState::with_shared_host(
            root.path().to_owned(),
            Uuid::now_v7().to_string(),
            SharedHostServices {
                host: host.clone(),
                permissions: Arc::new(atman_runtime::permission::PermissionClients::default()),
                operator: Uuid::from_bytes(*b"atman-local-user"),
                prompts: None,
            },
        ));
        let (target_server, _target_registration) = serve(&target).await;
        let source_peer = source.peer_transport.get().unwrap();
        let peer = target.peer_transport.get().unwrap();
        let binding = HandoffBinding {
            request: Uuid::now_v7(),
            session,
            source: source_peer.identity.clone(),
            destination: peer.identity.clone(),
        };
        assert!(!source.handoff_pending());
        assert!(matches!(
            peer.call_host(binding.source.clone(), HostCall::Prepare(binding.clone()))
                .await
                .unwrap(),
            HostReply::Prepared
        ));
        tokio::time::timeout(Duration::from_secs(5), async {
            while source_peer.active_coordinators.load(Ordering::Acquire) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(source.handoff_pending());
        assert!(
            !source.runtime_host().try_shutdown_idle().await.unwrap(),
            "the HTTP response cannot retire the original prepared handoff"
        );
        peer.transfer_binding(&target, binding, CancellationToken::new())
            .await
            .unwrap();
        assert!(!source.handoff_pending());
        assert!(!target.handoff_pending());
        assert!(source.runtime_host().try_shutdown_idle().await.unwrap());
        assert_eq!(
            source_peer.pin_coordinator().err().unwrap().kind(),
            io::ErrorKind::NotConnected
        );
        source_server.shutdown().await.unwrap();
        target_server.shutdown().await.unwrap();
        source.shutdown().await.unwrap();
        host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn prepared_cancel_and_release_reconcile_the_original_binding_in_both_gate_orders() {
        use atman_runtime::flow_store::inbox::{
            DeliveryMode, DeliveryRequest, InboxPayload, SenderIdentity, UserContent,
        };

        for cancel_first in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let source = Arc::new(DaemonState::new(root.path().to_owned()));
            let (source_server, _source_registration) = serve(&source).await;
            let session = Uuid::now_v7();
            source
                .runtime_host()
                .create_session(
                    root.path(),
                    SessionDescriptor {
                        id: session,
                        title: "Cancellation boundary".into(),
                        project: None,
                        configuration: Default::default(),
                    },
                )
                .await
                .unwrap();
            let worker = advance_fixture(&source, session).await;
            let input = worker
                .accept_input(
                    DeliveryRequest {
                        expected_head: None,
                        request_id: Uuid::now_v7(),
                        sender: SenderIdentity::User(Uuid::now_v7()),
                        recipient: worker.address().clone(),
                        delivery: DeliveryMode::NextTurn,
                        cause: None,
                        correlation: None,
                        scope: None,
                        payload: InboxPayload::UserSubmission {
                            content: UserContent {
                                text: "retained input".into(),
                                images: vec![],
                                presentation: None,
                            },
                            invocation: Default::default(),
                        },
                    },
                    |_| Ok(()),
                )
                .await
                .unwrap();
            let host = RuntimeHost::default();
            let target = Arc::new(DaemonState::with_shared_host(
                root.path().to_owned(),
                Uuid::now_v7().to_string(),
                SharedHostServices {
                    host: host.clone(),
                    permissions: Arc::new(atman_runtime::permission::PermissionClients::default()),
                    operator: Uuid::from_bytes(*b"atman-local-user"),
                    prompts: None,
                },
            ));
            let (target_server, _target_registration) = serve(&target).await;
            let source_peer = source.peer_transport.get().unwrap();
            let peer = target.peer_transport.get().unwrap();
            let binding = HandoffBinding {
                request: Uuid::now_v7(),
                session,
                source: source_peer.identity.clone(),
                destination: peer.identity.clone(),
            };
            peer.call_host(binding.source.clone(), HostCall::Prepare(binding.clone()))
                .await
                .unwrap();
            peer.load_recovery().await.unwrap();
            let compact = worker.context().compact_lock_handle().lock_owned().await;
            let operation = if cancel_first {
                HostCall::CancelPrepared(binding.clone())
            } else {
                HostCall::Release(binding.clone())
            };
            // Discard the original HTTP acknowledgement after the record gate chooses the winner.
            let lost = peer
                .client
                .post(format!("http://{}{CALL_PATH}", source_server.endpoint()))
                .bearer_auth(&peer.credential.0)
                .json(&Envelope {
                    revision: REVISION,
                    request: Uuid::now_v7(),
                    caller: peer.identity.clone(),
                    expected: binding.source.clone(),
                    operation,
                })
                .send()
                .await
                .unwrap();
            assert!(lost.status().is_success());
            drop(lost);
            assert!(
                target.handoff_pending(),
                "an unobserved outcome retains its coordinator"
            );
            assert_eq!(
                worker.snapshot().await.unwrap().pending[0].id,
                input.message_id
            );
            if cancel_first {
                assert!(peer.cancel_prepared(&binding).await.unwrap());
                assert_eq!(
                    peer.call_host(binding.source.clone(), HostCall::Release(binding.clone()))
                        .await
                        .err()
                        .unwrap()
                        .kind(),
                    io::ErrorKind::Interrupted
                );
                let record = source_peer.recovery.lock().unwrap()[&binding.request].clone();
                assert_eq!(record.phase, Phase::Aborted);
                assert!(record.receipt.is_none());
                assert_eq!(record.binding, binding);
                assert!(source.runtime_host().session(session).is_ok());
                assert!(host.sessions().is_empty());
                assert!(!source.handoff_pending());
                assert!(!target.handoff_pending());
                peer.load_recovery().await.unwrap();
                assert!(
                    !target.handoff_pending(),
                    "a tombstone cannot regain retention on load"
                );
                let mut reused = binding.clone();
                reused.session = Uuid::now_v7();
                assert!(
                    peer.call_host(binding.source.clone(), HostCall::Prepare(reused))
                        .await
                        .is_err()
                );
                assert_eq!(
                    worker.snapshot().await.unwrap().pending[0].id,
                    input.message_id
                );
                drop(compact);
            } else {
                assert!(!peer.cancel_prepared(&binding).await.unwrap());
                assert_eq!(
                    source_peer.recovery.lock().unwrap()[&binding.request].phase,
                    Phase::Releasing
                );
                assert!(source.handoff_pending());
                let cancel = CancellationToken::new();
                cancel.cancel();
                let mut continued =
                    Box::pin(peer.transfer_binding(&target, binding.clone(), cancel));
                assert!(futures::poll!(continued.as_mut()).is_pending());
                drop(compact);
                let receipt = tokio::time::timeout(Duration::from_secs(5), continued)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(receipt.request_id, binding.request);
                assert_eq!(receipt.session_id, session);
                let installed = host
                    .session(session)
                    .unwrap()
                    .open_flow(worker.address().instance_id.clone())
                    .await
                    .unwrap();
                assert_eq!(
                    installed.snapshot().await.unwrap().pending[0].id,
                    input.message_id
                );
                assert!(!source.handoff_pending());
                assert!(!target.handoff_pending());
                assert_eq!(
                    peer.transfer_binding(&target, binding.clone(), CancellationToken::new())
                        .await
                        .unwrap(),
                    receipt
                );
            }
            assert_eq!(
                peer_handoff_store::load(root.path().canonicalize().unwrap())
                    .await
                    .unwrap()
                    .len(),
                1
            );
            source_server.shutdown().await.unwrap();
            target_server.shutdown().await.unwrap();
            source.shutdown().await.unwrap();
            host.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn user_authentication_and_stale_peer_envelopes_cannot_reach_internal_effects() {
        let root = tempfile::tempdir().unwrap();
        let state = Arc::new(DaemonState::new(root.path().to_owned()));
        let (server, _registration) = serve(&state).await;
        let peer = state.peer_transport.get().unwrap();
        let response = peer
            .client
            .post(format!("http://{}{CALL_PATH}", server.endpoint()))
            .bearer_auth(USER_TOKEN)
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
        let mut expected = peer.identity.clone();
        expected.runtime_generation = Uuid::new_v4();
        let request = Envelope {
            revision: REVISION,
            request: Uuid::new_v4(),
            caller: peer.identity.clone(),
            expected,
            operation: HostCall::Runtime(PeerCall::Profile(ProfileOperation::Snapshot)),
        };
        let response = peer
            .client
            .post(format!("http://{}{CALL_PATH}", server.endpoint()))
            .bearer_auth(&peer.credential.0)
            .json(&request)
            .send()
            .await
            .unwrap();
        let reply: Reply<HostReply> = read_reply(response).await.unwrap();
        assert!(reply.result.is_err());
        assert!(state.runtime_host().sessions().is_empty());
        assert!(!state.handoff_pending());
        server.shutdown().await.unwrap();
        state.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn public_handoff_requires_a_verified_destination_and_returns_only_after_installation() {
        #[derive(Default)]
        struct Notifications(Mutex<Vec<atman_runtime::notify::Notification>>);
        impl atman_runtime::notify::NotifySink for Notifications {
            fn emit(&self, note: &atman_runtime::notify::Notification) {
                self.0.lock().unwrap().push(note.clone());
            }
        }
        let notifications = Arc::new(Notifications::default());
        let _sink = atman_runtime::notify::ScopedSink::replace_with(notifications.clone());
        let root = tempfile::tempdir().unwrap();
        let source = Arc::new(DaemonState::new(root.path().to_owned()));
        let (source_server, _source_registration) = serve(&source).await;
        let session = Uuid::now_v7();
        source
            .runtime_host()
            .create_session(
                root.path(),
                SessionDescriptor {
                    id: session,
                    title: "User handoff".into(),
                    project: None,
                    configuration: Default::default(),
                },
            )
            .await
            .unwrap();
        let client = reqwest::Client::new();
        let source_generation = source.runtime_host().generation();
        let missing = atman_proto::hosts::HandoffSessionRequest {
            session_id: atman_proto::SessionId(session),
            request_id: Uuid::now_v7(),
            source_generation,
            destination_generation: Uuid::now_v7(),
        };
        let response: serde_json::Value = client
            .post(format!("http://{}/rpc", source_server.endpoint()))
            .bearer_auth(USER_TOKEN)
            .json(&atman_proto::JsonRpcRequest::new(
                "missing",
                atman_proto::methods::HOST_HANDOFF,
                serde_json::to_value(missing).unwrap(),
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(response["error"].is_object());
        assert!(source.runtime_host().session(session).is_ok());
        assert!(!source.handoff_pending());

        let target_host = RuntimeHost::default();
        let target = Arc::new(DaemonState::with_shared_host(
            root.path().to_owned(),
            Uuid::now_v7().to_string(),
            SharedHostServices {
                host: target_host.clone(),
                permissions: Arc::new(atman_runtime::permission::PermissionClients::default()),
                operator: Uuid::from_bytes(*b"atman-local-user"),
                prompts: None,
            },
        ));
        let (target_server, _target_registration) = serve(&target).await;
        let request_id = Uuid::now_v7();
        let peer = source.peer_transport.get().unwrap();
        peer.dispatch_host(
            peer.identity.clone(),
            HostCall::Prepare(HandoffBinding {
                request: request_id,
                session,
                source: peer.identity.clone(),
                destination: target.peer_transport.get().unwrap().identity.clone(),
            }),
        )
        .await
        .unwrap();
        assert!(
            !notifications
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|note| note.target.as_deref() == Some("host-handoff")
                    && note.message.contains(&request_id.to_string()))
        );
        let request = atman_proto::JsonRpcRequest::new(
            "installed",
            atman_proto::methods::HOST_HANDOFF,
            serde_json::to_value(atman_proto::hosts::HandoffSessionRequest {
                session_id: atman_proto::SessionId(session),
                request_id,
                source_generation,
                destination_generation: target_host.generation(),
            })
            .unwrap(),
        );
        let mut replies = Vec::new();
        for _ in 0..2 {
            replies.push(
                client
                    .post(format!("http://{}/rpc", source_server.endpoint()))
                    .bearer_auth(USER_TOKEN)
                    .json(&request)
                    .send()
                    .await
                    .unwrap()
                    .json::<serde_json::Value>()
                    .await
                    .unwrap(),
            );
        }
        assert_eq!(replies[0], replies[1]);
        assert_eq!(replies[0]["result"]["state"], "installed");
        assert!(source.runtime_host().session(session).is_err());
        assert!(target_host.session(session).is_ok());
        assert!(!source.handoff_pending());
        let original = peer.recovery.lock().unwrap()[&request_id].clone();
        peer.dispatch_host(
            peer.identity.clone(),
            HostCall::Acknowledge {
                binding: original.binding,
                receipt: original.receipt.unwrap(),
            },
        )
        .await
        .unwrap();
        let logged: Vec<_> = notifications
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|note| {
                note.target.as_deref() == Some("host-handoff")
                    && note.message.contains(&request_id.to_string())
            })
            .cloned()
            .collect();
        assert_eq!(
            logged.len(),
            1,
            "retrying a confirmed handoff cannot duplicate its log"
        );
        assert_eq!(
            logged[0].location,
            atman_runtime::notify::NotifyLocation::Log
        );
        assert!(logged[0].message.contains(&session.to_string()));
        assert!(logged[0].message.contains("taken over by TUI host"));
        assert!(
            logged[0]
                .message
                .contains(&target_host.generation().to_string())
        );
        // A source acknowledgement also permits the destination to release this Session.
        target
            .return_sessions_to_daemon(CancellationToken::new())
            .await
            .unwrap();
        assert!(target_host.sessions().is_empty());
        assert!(source.runtime_host().session(session).is_ok());
        let repeated: serde_json::Value = client
            .post(format!("http://{}/rpc", source_server.endpoint()))
            .bearer_auth(USER_TOKEN)
            .json(&request)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(repeated, replies[0]);
        assert!(
            target_host.sessions().is_empty(),
            "an old completed request moved the Session again"
        );
        assert!(source.runtime_host().session(session).is_ok());
        target_server.shutdown().await.unwrap();
        target_host.shutdown().await.unwrap();
        source_server.shutdown().await.unwrap();
        source.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn repeated_acquire_reconciles_partial_multiflow_installation_and_lost_replies() {
        use atman_runtime::{
            config_hub::ConfigHub,
            flow_runtime::scheduler::SchedulingStatus,
            flow_store::{
                SessionConfiguration,
                control::{ControlAction, ControlRequest},
                inbox::{DeliveryMode, DeliveryRequest, InboxPayload, SenderIdentity},
            },
        };
        async fn rpc(state: &Arc<DaemonState>, method: &str, params: serde_json::Value) {
            let response = crate::dispatch(
                state.clone(),
                atman_proto::JsonRpcRequest::new("handoff-fixture", method, params),
            )
            .await;
            assert!(response.error.is_none(), "{method}: {:?}", response.error);
        }
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config");
        fs::create_dir(&config).unwrap();
        fs::write(config.join("config.toml"), "").unwrap();
        let path = config.join("main.at");
        fs::write(
            &path,
            "flow main(input: string = \"\") -> string { return input }",
        )
        .unwrap();
        let source = Arc::new(DaemonState::new(root.path().to_owned()));
        let (source_server, _source_registration) = serve(&source).await;
        let session = Uuid::now_v7();
        let owner = source
            .runtime_host()
            .create_session(
                root.path(),
                SessionDescriptor {
                    id: session,
                    title: "Multiple schedulers".into(),
                    project: None,
                    configuration: SessionConfiguration::Atman(
                        ConfigHub::from_config_dir(&config).source(),
                    ),
                },
            )
            .await
            .unwrap();
        let flows: Vec<_> = (0..3).map(|_| atman_rt::FlowInstanceId::now()).collect();
        for flow in &flows {
            rpc(
                &source,
                atman_proto::methods::FLOW_LAUNCH,
                serde_json::json!({
                    "flow": {"session_id":session,"instance_id":flow.to_string()},
                    "request_id":Uuid::now_v7(),
                    "expected_catalog_sequence":owner.snapshot().await.unwrap().cursor.sequence,
                    "path":path, "entry":"main", "start":"waiting",
                }),
            )
            .await;
        }
        let destination_host = RuntimeHost::default();
        let destination = Arc::new(DaemonState::with_shared_host(
            root.path().to_owned(),
            Uuid::now_v7().to_string(),
            SharedHostServices {
                host: destination_host.clone(),
                permissions: Arc::new(atman_runtime::permission::PermissionClients::default()),
                operator: Uuid::from_bytes(*b"atman-local-user"),
                prompts: None,
            },
        ));
        let (destination_server, _destination_registration) = serve(&destination).await;
        let peer = source.peer_transport.get().unwrap();
        let binding = HandoffBinding {
            request: Uuid::now_v7(),
            session,
            source: peer.identity.clone(),
            destination: destination.peer_transport.get().unwrap().identity.clone(),
        };
        peer.dispatch_host(peer.identity.clone(), HostCall::Prepare(binding.clone()))
            .await
            .unwrap();
        let receipt = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                match peer
                    .dispatch_host(peer.identity.clone(), HostCall::Release(binding.clone()))
                    .await
                    .unwrap()
                {
                    HostReply::Released(SessionHandoffOutcome::Released { receipt }) => {
                        break receipt;
                    }
                    HostReply::Released(SessionHandoffOutcome::Pending { .. }) => {
                        tokio::task::yield_now().await
                    }
                    _ => panic!("unexpected source release"),
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(receipt.flows.len(), 3);
        assert!(receipt.flows.iter().all(|flow| flow.scheduling
            == Some(atman_runtime::flow_runtime::host::HandoffScheduling::Running)));
        let installed = destination_host
            .acquire_handoff(root.path(), &receipt)
            .await
            .unwrap();
        // Reproduce an Acquire interrupted after its first actual scheduler installation.
        rpc(
            &destination,
            atman_proto::methods::FLOW_BIND,
            serde_json::json!({
                "flow":{"session_id":session,"instance_id":flows[0].to_string()},
            }),
        )
        .await;
        let first = installed
            .scheduling_status(flows[0].clone())
            .await
            .unwrap()
            .unwrap();
        assert!(
            installed
                .scheduling_status(flows[1].clone())
                .await
                .unwrap()
                .is_none()
        );
        let lost = peer
            .client
            .post(format!(
                "http://{}{CALL_PATH}",
                destination_server.endpoint()
            ))
            .bearer_auth(&peer.credential.0)
            .json(&Envelope {
                revision: REVISION,
                request: Uuid::now_v7(),
                caller: peer.identity.clone(),
                expected: binding.destination.clone(),
                operation: HostCall::Acquire {
                    binding: binding.clone(),
                    receipt: receipt.clone(),
                },
            })
            .send()
            .await
            .unwrap();
        assert!(lost.status().is_success());
        drop(lost);
        let second = installed
            .scheduling_status(flows[1].clone())
            .await
            .unwrap()
            .unwrap();
        let mut third = installed
            .scheduling_status(flows[2].clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*second.borrow(), SchedulingStatus::Running);
        assert_eq!(*third.borrow(), SchedulingStatus::Running);
        // Lose the successful application reply, then change real destination state before retry.
        let closed = installed.open_flow(flows[1].clone()).await.unwrap();
        closed
            .control(
                ControlRequest {
                    id: Uuid::now_v7(),
                    sender: SenderIdentity::User(Uuid::now_v7()),
                    target: closed.address().clone(),
                    expected_revision: closed.state().await.unwrap().revision(),
                    pause_key: Uuid::now_v7().into(),
                    action: ControlAction::Close,
                },
                |_| Ok(()),
            )
            .await
            .unwrap();
        installed.stop_scheduling(flows[2].clone()).await.unwrap();
        let failed = installed.open_flow(flows[2].clone()).await.unwrap();
        let pending = failed
            .accept_input(
                DeliveryRequest {
                    expected_head: None,
                    request_id: Uuid::now_v7(),
                    sender: SenderIdentity::User(Uuid::now_v7()),
                    recipient: failed.address().clone(),
                    delivery: DeliveryMode::NextTurn,
                    cause: None,
                    correlation: None,
                    scope: None,
                    payload: InboxPayload::Invocation {
                        arguments: Default::default(),
                        invocation: Default::default(),
                        user: None,
                    },
                },
                |_| Ok(()),
            )
            .await
            .unwrap();
        let original_executor = installed.executor_for(None).await.unwrap();
        third = installed
            .schedule_flow(flows[2].clone(), original_executor, |_| {
                Err(io::Error::other("destination preparation failed"))
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if *second.borrow() == SchedulingStatus::Stopped
                    && matches!(*third.borrow(), SchedulingStatus::Failed(_))
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let failed_state = third.borrow().clone();
        assert!(
            matches!(peer.call_host(binding.destination.clone(), HostCall::Acquire {
            binding: binding.clone(), receipt: receipt.clone(),
        }).await.unwrap(), HostReply::Installed(ref original) if original == &receipt)
        );
        let current = installed
            .scheduling_status(flows[0].clone())
            .await
            .unwrap()
            .unwrap();
        assert!(first.same_channel(&current));
        assert_eq!(*current.borrow(), SchedulingStatus::Running);
        let current = installed
            .scheduling_status(flows[1].clone())
            .await
            .unwrap()
            .unwrap();
        assert!(second.same_channel(&current));
        assert_eq!(*current.borrow(), SchedulingStatus::Stopped);
        let current = installed
            .scheduling_status(flows[2].clone())
            .await
            .unwrap()
            .unwrap();
        assert!(third.same_channel(&current));
        assert_eq!(*current.borrow(), failed_state);
        assert_eq!(
            failed.snapshot().await.unwrap().pending[0].id,
            pending.message_id
        );
        assert_eq!(
            destination_host
                .installed_handoff(session, binding.request, binding.source.runtime_generation)
                .unwrap(),
            Some(receipt.clone())
        );
        assert!(
            destination
                .peer_transport
                .get()
                .unwrap()
                .dispatch_host(
                    peer.identity.clone(),
                    HostCall::Complete {
                        binding: binding.clone(),
                        receipt: receipt.clone()
                    }
                )
                .await
                .is_err()
        );
        assert!(
            destination.handoff_pending(),
            "an unacknowledged release cannot be retired by an installation observer"
        );
        assert!(
            matches!(peer.call_host(binding.destination.clone(), HostCall::Confirm(binding.clone())).await.unwrap(), HostReply::Confirmation(Some(ref installed)) if installed == &receipt)
        );
        // Stop at the real source acknowledgement boundary before Complete reaches the destination.
        peer.persist(&binding, Phase::Completed, Some(receipt.clone()), None)
            .await
            .unwrap();
        assert!(destination.handoff_pending());
        assert_eq!(
            peer.transfer_binding(&source, binding.clone(), CancellationToken::new())
                .await
                .unwrap(),
            receipt
        );
        assert!(!destination.handoff_pending());
        let lost = peer
            .client
            .post(format!(
                "http://{}{CALL_PATH}",
                destination_server.endpoint()
            ))
            .bearer_auth(&peer.credential.0)
            .json(&Envelope {
                revision: REVISION,
                request: Uuid::now_v7(),
                caller: peer.identity.clone(),
                expected: binding.destination.clone(),
                operation: HostCall::Complete {
                    binding: binding.clone(),
                    receipt: receipt.clone(),
                },
            })
            .send()
            .await
            .unwrap();
        assert!(lost.status().is_success());
        drop(lost);
        assert_eq!(
            peer.transfer_binding(&source, binding.clone(), CancellationToken::new())
                .await
                .unwrap(),
            receipt
        );
        assert!(matches!(
            peer.call_host(
                peer.identity.clone(),
                HostCall::Acknowledge { binding, receipt }
            )
            .await
            .unwrap(),
            HostReply::Acknowledged
        ));
        assert!(!destination.handoff_pending());
        destination_server.shutdown().await.unwrap();
        let cleanup = destination_host.shutdown().await.unwrap_err();
        assert!(
            cleanup
                .to_string()
                .contains("destination preparation failed"),
            "{cleanup}"
        );
        source_server.shutdown().await.unwrap();
        source.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn two_processes_transfer_one_session_and_share_the_original_profile_owner() {
        let root = tempfile::tempdir().unwrap();
        let session = Uuid::now_v7();
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "peer_transport::tests::child_host",
                "--nocapture",
            ])
            .env("ATMAN_PEER_HOST_FIXTURE", root.path())
            .env("ATMAN_PEER_HOST_SESSION", session.to_string())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        tokio::time::timeout(Duration::from_secs(15), async {
            while !lines
                .next_line()
                .await
                .unwrap()
                .expect("peer child exited before readiness")
                .contains("PEER_HOST_READY")
            {}
        })
        .await
        .unwrap();

        let host = RuntimeHost::default();
        let state = Arc::new(DaemonState::with_shared_host(
            root.path().to_owned(),
            Uuid::now_v7().to_string(),
            SharedHostServices {
                host: host.clone(),
                permissions: Arc::new(atman_runtime::permission::PermissionClients::default()),
                operator: Uuid::from_bytes(*b"atman-local-user"),
                prompts: None,
            },
        ));
        let (server, _registration) = serve(&state).await;
        let profile = host.profile_control_port().unwrap();
        assert!(!profile.is_local_owner());
        assert!(host.profile_handle().is_none());
        let original = profile.snapshot().await.unwrap();
        assert!(
            ProfileStore::open(
                &atman_runtime::flow_store::RuntimeStore::open(root.path()).unwrap()
            )
            .is_err()
        );
        let view = profile.bootstrap(Default::default()).await.unwrap();
        assert_eq!(view.profile.profile_id, original.profile.profile_id);
        assert!(view.profile.management.is_some());

        let receipt = tokio::time::timeout(
            Duration::from_secs(15),
            state.claim_session(session, CancellationToken::new()),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        assert_eq!(receipt.session_id, session);
        assert_ne!(receipt.source_generation, host.generation());
        assert_eq!(
            state
                .peer_transport
                .get()
                .unwrap()
                .recovery
                .lock()
                .unwrap()
                .get(&receipt.request_id)
                .unwrap()
                .phase,
            Phase::Completed,
            "the coordinator retained Installed after the source acknowledged completion"
        );
        assert_eq!(
            host.installed_handoff(session, receipt.request_id, receipt.source_generation)
                .unwrap(),
            Some(receipt)
        );
        assert!(!state.handoff_pending());
        let instance = atman_rt::FlowInstanceId::now();
        let target = atman_runtime::flow_store::FlowRef {
            session_id: session.into(),
            instance_id: instance.clone(),
        };
        let worker = host
            .session(session)
            .unwrap()
            .create_flow(atman_runtime::flow_store::FlowDescriptor {
                address: target.clone(),
                creation_request: Uuid::now_v7(),
                created_by: None,
                content: atman_runtime::flow_store::EntrySnapshot {
                    name: "main".into(),
                    path: "main.at".into(),
                    bundle: atman_runtime::source_program::RevisionBundle {
                        entry: "entry:main.at".into(),
                        edges: Default::default(),
                        sources: [(
                            "entry:main.at".into(),
                            "flow main(input: string) -> string { return input }".into(),
                        )]
                        .into(),
                    },
                }
                .into(),
            })
            .await
            .unwrap();
        worker
            .transition(0, atman_runtime::flow_store::FlowTransition::Activated)
            .await
            .unwrap();
        let sender = atman_runtime::flow_store::inbox::SenderIdentity::User(Uuid::now_v7());
        let pause = Uuid::now_v7();
        let request = atman_runtime::flow_store::profile::ControlBatchRequest {
            id: Uuid::now_v7(),
            sender: sender.clone(),
            commands: vec![atman_runtime::flow_store::control::ControlRequest {
                id: Uuid::now_v7(),
                sender: sender.clone(),
                target,
                expected_revision: worker.snapshot().await.unwrap().state.revision(),
                pause_key: pause.into(),
                action: atman_runtime::flow_store::control::ControlAction::Pause,
            }],
        };
        let accepted = profile
            .submit_controls(request.clone(), |_| Ok(()))
            .await
            .unwrap();
        let duplicate = profile.submit_controls(request, |_| Ok(())).await.unwrap();
        assert_eq!(accepted.submitted, duplicate.submitted);
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let record = profile
                    .control_batch(sender.clone(), accepted.id)
                    .await
                    .unwrap()
                    .unwrap();
                if record.items.iter().all(|item| {
                    matches!(
                        item.history.last().map(|history| &history.progress),
                        Some(atman_runtime::flow_store::profile::ControlProgress::Applied { .. })
                    )
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            worker
                .snapshot()
                .await
                .unwrap()
                .state
                .pauses()
                .contains(&pause.into())
        );
        tokio::time::timeout(
            Duration::from_secs(15),
            state.return_sessions_to_daemon(CancellationToken::new()),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(host.sessions().is_empty());
        assert!(!state.handoff_pending());

        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"stop\n")
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(15), child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
        server.shutdown().await.unwrap();
        host.shutdown().await.unwrap();
    }

    #[test]
    fn child_host() {
        let Some(root) = std::env::var_os("ATMAN_PEER_HOST_FIXTURE") else {
            return;
        };
        let session: Uuid = std::env::var("ATMAN_PEER_HOST_SESSION")
            .unwrap()
            .parse()
            .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let tui = std::env::var("ATMAN_PEER_HOST_ROLE").is_ok_and(|role| role == "tui");
            let state = Arc::new(if tui {
                DaemonState::with_shared_host(
                    root.into(),
                    Uuid::now_v7().to_string(),
                    SharedHostServices {
                        host: RuntimeHost::default(),
                        permissions: Arc::new(
                            atman_runtime::permission::PermissionClients::default(),
                        ),
                        operator: Uuid::from_bytes(*b"atman-local-user"),
                        prompts: None,
                    },
                )
            } else {
                DaemonState::new(root.into())
            });
            state.prepare_peer_transport().await.unwrap();
            if !tui {
                state
                    .runtime_host()
                    .create_session(
                        state.data_dir(),
                        SessionDescriptor {
                            id: session,
                            title: "Peer fixture".into(),
                            project: None,
                            configuration: Default::default(),
                        },
                    )
                    .await
                    .unwrap();
            }
            let (server, _registration) = serve(&state).await;
            println!("PEER_HOST_READY");
            std::io::stdout().flush().unwrap();
            let (sender, mut input) = tokio::sync::mpsc::unbounded_channel();
            let input_reader = std::thread::spawn(move || {
                for line in std::io::BufRead::lines(std::io::stdin().lock()) {
                    let stop = matches!(line.as_deref(), Ok("stop"));
                    if sender.send(line).is_err() || stop {
                        break;
                    }
                }
            });
            loop {
                match input.recv().await.map(Result::unwrap).as_deref() {
                    Some("advance") => {
                        advance_fixture(&state, session).await;
                        println!("PEER_HOST_ADVANCED");
                        std::io::stdout().flush().unwrap();
                    }
                    Some("stop") => break,
                    _ => panic!("unexpected fixture command"),
                }
            }
            input_reader.join().unwrap();
            assert!(state.runtime_host().session(session).is_ok());
            assert!(!state.handoff_pending());
            server.shutdown().await.unwrap();
            state.shutdown().await.unwrap();
            if tui {
                state.runtime_host().shutdown().await.unwrap();
            }
        });
    }

    #[tokio::test]
    async fn lost_install_reply_and_dead_destination_rebind_to_a_new_generation_without_cold_open()
    {
        let root = tempfile::tempdir().unwrap();
        let session = Uuid::now_v7();
        let source = Arc::new(DaemonState::new(root.path().to_owned()));
        let (source_server, _source_registration) = serve(&source).await;
        source
            .runtime_host()
            .create_session(
                root.path(),
                SessionDescriptor {
                    id: session,
                    title: "Crash fixture".into(),
                    project: None,
                    configuration: Default::default(),
                },
            )
            .await
            .unwrap();
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "peer_transport::tests::child_host",
                "--nocapture",
            ])
            .env("ATMAN_PEER_HOST_FIXTURE", root.path())
            .env("ATMAN_PEER_HOST_SESSION", session.to_string())
            .env("ATMAN_PEER_HOST_ROLE", "tui")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        tokio::time::timeout(Duration::from_secs(15), async {
            while !lines
                .next_line()
                .await
                .unwrap()
                .expect("destination exited before readiness")
                .contains("PEER_HOST_READY")
            {}
        })
        .await
        .unwrap();
        let peer = source.peer_transport.get().unwrap();
        let destination = peer
            .descriptions()
            .await
            .unwrap()
            .into_iter()
            .find(|(_, description)| description.role == HostRole::Tui)
            .unwrap()
            .1
            .identity;
        let binding = HandoffBinding {
            request: Uuid::now_v7(),
            session,
            source: peer.identity.clone(),
            destination: destination.clone(),
        };
        peer.dispatch_host(peer.identity.clone(), HostCall::Prepare(binding.clone()))
            .await
            .unwrap();
        let released = match peer
            .dispatch_host(peer.identity.clone(), HostCall::Release(binding.clone()))
            .await
            .unwrap()
        {
            HostReply::Released(SessionHandoffOutcome::Released { receipt }) => receipt,
            _ => panic!("inactive source did not release"),
        };
        let installed = peer
            .call_host(
                destination.clone(),
                HostCall::Acquire {
                    binding: binding.clone(),
                    receipt: released.clone(),
                },
            )
            .await
            .unwrap();
        assert!(matches!(installed, HostReply::Installed(_)));
        // Deliberately lose the application reply: no Confirm or source acknowledgement occurs.
        assert!(source.handoff_pending());
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"advance\n")
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            while !lines
                .next_line()
                .await
                .unwrap()
                .expect("destination exited before its commit")
                .contains("PEER_HOST_ADVANCED")
            {}
        })
        .await
        .unwrap();
        child.kill().await.unwrap();
        child.wait().await.unwrap();
        let replacement_host = RuntimeHost::default();
        let replacement = Arc::new(DaemonState::with_shared_host(
            root.path().to_owned(),
            Uuid::now_v7().to_string(),
            SharedHostServices {
                host: replacement_host.clone(),
                permissions: Arc::new(atman_runtime::permission::PermissionClients::default()),
                operator: Uuid::from_bytes(*b"atman-local-user"),
                prompts: None,
            },
        ));
        let (replacement_server, _replacement_registration) = serve(&replacement).await;
        let receipt = tokio::time::timeout(
            Duration::from_secs(20),
            replacement.claim_session(session, CancellationToken::new()),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        assert_ne!(receipt.request_id, released.request_id);
        assert_eq!(receipt.source_generation, released.source_generation);
        assert!(receipt.catalog.sequence > released.catalog.sequence);
        assert_eq!(receipt.flows.len(), 1);
        assert!(receipt.flows.iter().all(|flow| !matches!(
            flow.scheduling,
            Some(atman_runtime::flow_runtime::host::HandoffScheduling::Running)
        )));
        assert_eq!(
            replacement_host
                .installed_handoff(session, receipt.request_id, receipt.source_generation)
                .unwrap(),
            Some(receipt)
        );
        assert!(!replacement.handoff_pending());
        assert!(!source.handoff_pending());
        let records = peer_handoff_store::load(root.path().canonicalize().unwrap())
            .await
            .unwrap();
        assert!(
            records
                .iter()
                .any(|record| record.binding.request == binding.request
                    && record.phase == Phase::Completed
                    && record.replacement.is_some())
        );
        assert!(
            records
                .iter()
                .any(|record| record.supersedes == Some(binding.request)
                    && record.phase == Phase::Completed)
        );
        replacement
            .return_sessions_to_daemon(CancellationToken::new())
            .await
            .unwrap();
        replacement_server.shutdown().await.unwrap();
        replacement_host.shutdown().await.unwrap();
        source_server.shutdown().await.unwrap();
        source.shutdown().await.unwrap();
    }
}
