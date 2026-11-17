//! Typed same-machine peer operations retaining original invocation and journal authority.

use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    sync::{Arc, Mutex, Weak},
};

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{access::FlowClient, host::RuntimeHost};
use crate::{
    event::{AtmanUuid, TurnId},
    flow_authority::EffectiveAuthority,
    flow_store::{
        FlowInstanceId, FlowRef,
        access::{AccessTarget, FlowAction},
        completion::CommittedCompletion,
        control::{CommittedControl, ControlRequest},
        goal::{CommittedGoalUpdate, GoalRequest},
        inbox::{Acceptance, DeliveryRequest},
        name::{CommittedName, NameRequest, NameSnapshot},
        planning::PlanningSnapshot,
        position::FlowPosition,
        publication::PublishedMessage,
        subscription::{
            FlowSubscription, PeerFlowEventPage, PeerObservationAnchor, SubscriptionRecord,
        },
        task_scope::{ScopeCancellation, TaskScopeRef},
    },
    message::Message,
    permission::{PermissionAction, PermissionRequest, PermissionRequestId, ResolveOutcome},
};

pub(super) mod capture;
mod client;
mod dispatch;
mod durable;
mod observation;
pub(in crate::flow_runtime) mod pending_launch;
mod permission;
mod profile;
#[cfg(test)]
mod tests;

/// A verified internal endpoint, separate from user RPC authentication.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PeerIdentity {
    pub profile_id: Uuid,
    pub canonical_data_root: PathBuf,
    pub runtime_generation: Uuid,
    pub transport_generation: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "identity", rename_all = "snake_case")]
pub enum PeerTarget {
    Session(Uuid),
    Profile,
    Runtime(Uuid),
}

/// Transport discovery never grants runtime effect authority.
pub trait PeerPort: Send + Sync {
    fn identity(&self) -> PeerIdentity;
    fn locate(&self, target: PeerTarget) -> BoxFuture<'static, io::Result<PeerIdentity>>;
    fn call(
        &self,
        owner: PeerIdentity,
        call: PeerCall,
    ) -> BoxFuture<'static, io::Result<PeerReply>>;
}

impl RuntimeHost {
    pub fn configure_peer(
        &self,
        identity: PeerIdentity,
        port: Arc<dyn PeerPort>,
    ) -> io::Result<()> {
        if identity.runtime_generation != self.generation() || port.identity() != identity {
            return Err(denied(
                "peer endpoint does not belong to the original runtime",
            ));
        }
        if let Some(installed) = self.peer_port() {
            return if installed.identity() == identity {
                Ok(())
            } else {
                Err(denied("peer endpoint identity is already installed"))
            };
        }
        self.bind_peer_authority(crate::peer_authority::PeerAuthority::new(
            identity.canonical_data_root,
            identity.profile_id,
        )?)?;
        self.set_peer_port(port)
    }

    pub fn peer_identity(&self) -> io::Result<PeerIdentity> {
        self.peer_port().map(|port| port.identity()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "internal peer endpoint is unavailable",
            )
        })
    }
}

/// Minted from a live original FlowClient. Its nonce never appears in model arguments.
#[derive(Clone, Deserialize, Serialize)]
pub struct InvocationPrincipal {
    pub(super) owner: Uuid,
    pub(super) nonce: Uuid,
    pub(super) source: FlowRef,
}

impl std::fmt::Debug for InvocationPrincipal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InvocationPrincipal")
            .field("owner", &self.owner)
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DurableOrigin {
    Outbox {
        source: FlowRef,
        request: Uuid,
        intent: FlowPosition,
    },
    Subscription {
        recipient: FlowRef,
        id: Uuid,
        registration: FlowPosition,
    },
}

/// An owner-issued reference to an already committed operation, never a live-turn substitute.
#[derive(Clone, Deserialize, Serialize)]
pub struct DurablePrincipal {
    pub(super) owner: Uuid,
    pub(super) nonce: Uuid,
    pub(super) origin: DurableOrigin,
}

impl std::fmt::Debug for DurablePrincipal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurablePrincipal")
            .field("owner", &self.owner)
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", content = "principal", rename_all = "snake_case")]
pub enum PeerPrincipal {
    Invocation(InvocationPrincipal),
    Durable(DurablePrincipal),
    Observation(ObservationPrincipal),
    PendingLaunch(PendingLaunchPrincipal),
}

/// A finite original prepared launch admission; no invocation lease is synthesized.
#[derive(Clone, Deserialize, Serialize)]
pub struct PendingLaunchPrincipal {
    pub(super) owner: Uuid,
    pub(super) nonce: Uuid,
    pub(super) source: FlowRef,
}

impl std::fmt::Debug for PendingLaunchPrincipal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PendingLaunchPrincipal")
            .field("owner", &self.owner)
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

/// A finite native observation admission, bound to its original owner callback.
#[derive(Clone, Deserialize, Serialize)]
pub struct ObservationPrincipal {
    pub(super) owner: Uuid,
    pub(super) nonce: Uuid,
    pub(super) recipient: FlowRef,
}

impl std::fmt::Debug for ObservationPrincipal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ObservationPrincipal")
            .field("owner", &self.owner)
            .field("recipient", &self.recipient)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EffectBinding {
    pub transaction: Uuid,
    pub target_owner: Uuid,
    pub operation_digest: String,
    pub target: AccessTarget,
    pub actions: Vec<FlowAction>,
    pub finishing_context: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct InvocationWitness {
    pub source: FlowRef,
    pub lease: Option<atman_rt::TurnLease<AtmanUuid>>,
    pub binding: EffectBinding,
    pub access_cursor: crate::event_log::journal::CommitReceipt,
    pub source_grants: Vec<FlowAction>,
    pub registered_authority: Option<EffectiveAuthority>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ScopeEvidence {
    pub reference: TaskScopeRef,
    pub open: bool,
    pub cancellation: Option<ScopeCancellation>,
    pub ancestors: Vec<TaskScopeRef>,
    pub cursor: FlowPosition,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", content = "operation", rename_all = "snake_case")]
pub enum PeerCall {
    Execute {
        principal: PeerPrincipal,
        binding: EffectBinding,
        operation: Box<PeerOperation>,
    },
    Validate {
        principal: PeerPrincipal,
        binding: EffectBinding,
    },
    Scopes {
        references: Vec<TaskScopeRef>,
    },
    PrepareScopes {
        references: Vec<TaskScopeRef>,
    },
    Access {
        source: FlowRef,
        target: AccessTarget,
        action: FlowAction,
    },
    AccessBatch {
        source: FlowRef,
        targets: Vec<(AccessTarget, FlowAction)>,
    },
    SessionSummary {
        session: Uuid,
    },
    Profile(super::peer_profile::ProfileOperation),
    ProfileDelivery(super::peer_profile::ControlDelivery),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PeerOperation {
    RegisterLaunchResult {
        request: pending_launch::PendingLaunchRequest,
    },
    Sessions {
        before: Option<Uuid>,
        limit: usize,
    },
    Capture {
        request: capture::CaptureRequest,
    },
    CaptureChunk {
        target: FlowRef,
        token: Uuid,
        offset: usize,
        read: capture::ReadKind,
    },
    ReleaseCapture {
        target: FlowRef,
        token: Uuid,
        read: capture::ReadKind,
    },
    InputStatus {
        target: FlowRef,
        request_id: Uuid,
    },
    ControlBatch {
        request: crate::flow_store::profile::ControlBatchRequest,
    },
    ControlStatus {
        source: FlowRef,
        id: Uuid,
    },
    Name {
        target: FlowRef,
    },
    Rename {
        target: FlowRef,
        request: NameRequest,
    },
    Inspect {
        target: FlowRef,
    },
    Observe {
        target: FlowRef,
    },
    Instances {
        session: Uuid,
        before: Option<FlowInstanceId>,
        limit: usize,
    },
    Output {
        target: FlowRef,
    },
    Result {
        target: FlowRef,
        turn: Option<TurnId>,
    },
    Context {
        target: FlowRef,
    },
    Planning {
        target: FlowRef,
    },
    Goal {
        target: FlowRef,
    },
    EditGoal {
        target: FlowRef,
        request: GoalRequest,
    },
    Deliver {
        request: DeliveryRequest,
    },
    Control {
        request: ControlRequest,
    },
    Anchor {
        recipient: FlowRef,
        source: FlowRef,
    },
    Events {
        recipient: FlowRef,
        source: FlowRef,
        after: u64,
        limit: usize,
    },
    Permissions {
        target: FlowRef,
    },
    PermissionReview {
        target: FlowRef,
        request: PermissionRequestId,
    },
    ResolvePermission {
        target: FlowRef,
        request: PermissionRequestId,
        revision: u64,
        action: PermissionAction,
        reason: Option<String>,
    },
    OfferPermission {
        reviewer: FlowRef,
        request: PermissionRequestId,
        revision: u64,
    },
    PermissionReviewProgress {
        reviewer: FlowRef,
        request: PermissionRequestId,
        revision: u64,
        accepted: Acceptance,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum PeerReply {
    LaunchResult(crate::flow_store::subscription::PeerResultSubscriptionProof),
    #[serde(skip)]
    NativeSnapshot(Box<capture::NativeSnapshot>),
    Sessions(super::catalog::SessionPage),
    SessionSummary(super::catalog::SessionSummary),
    AccessBatch {
        cursor: crate::event_log::journal::CommitReceipt,
        allowed: Vec<bool>,
    },
    Capture(capture::Transfer),
    CaptureChunk {
        offset: usize,
        bytes: Vec<u8>,
    },
    CaptureReleased,
    InputStatus(Option<crate::flow_store::input_observation::InputObservation>),
    ControlBatch(crate::flow_store::profile::ControlBatchRecord),
    ControlStatus(Option<crate::flow_store::profile::ControlBatchRecord>),
    Witness(InvocationWitness),
    Access {
        cursor: crate::event_log::journal::CommitReceipt,
        allowed: bool,
    },
    Scopes(Vec<ScopeEvidence>),
    Name {
        title: String,
        snapshot: NameSnapshot,
    },
    Renamed(CommittedName),
    State {
        #[serde(with = "crate::flow_store::execution::instance_wire")]
        state: atman_rt::FlowInstance<AtmanUuid>,
    },
    ProfileDelivery(super::peer_profile::ControlDeliveryReply),
    Observation(super::access::InstanceObservation),
    Instances(super::access::InstancePage),
    Output(Vec<PublishedMessage>),
    Result(Option<CommittedCompletion>),
    Context(Vec<Message>),
    Planning(PlanningSnapshot),
    Goal(super::goal::GoalView),
    GoalEdited(CommittedGoalUpdate),
    Accepted(Acceptance),
    Controlled(CommittedControl),
    Anchor(PeerObservationAnchor),
    Events(PeerFlowEventPage),
    Permissions(Vec<PermissionRequest>),
    PermissionReview(serde_json::Value),
    PermissionResolved(ResolveOutcome),
    PermissionReviewProgress {
        waiting: bool,
    },
    Profile(super::peer_profile::ProfileReply),
}

#[derive(Default)]
pub(super) struct InvocationRegistry {
    pub(in crate::flow_runtime) captures: Mutex<HashMap<Uuid, Arc<capture::Captured>>>,
    clients: Mutex<HashMap<Uuid, Weak<InvocationIdentity>>>,
    durable: Mutex<HashMap<Uuid, Weak<durable::DurableIdentity>>>,
    observations: Mutex<HashMap<Uuid, Weak<observation::ObservationIdentity>>>,
    pending_launches: Mutex<HashMap<Uuid, Weak<pending_launch::PendingLaunchIdentity>>>,
}

pub(super) struct InvocationIdentity {
    pub principal: InvocationPrincipal,
    pub lease: atman_rt::TurnLease<AtmanUuid>,
    pub state: tokio::sync::watch::Receiver<atman_rt::FlowInstance<AtmanUuid>>,
    pub access: Weak<std::sync::RwLock<crate::flow_store::access::SessionAccess>>,
    effects: Mutex<HashMap<Uuid, Weak<RegisteredEffect>>>,
}

struct RegisteredEffect {
    binding: EffectBinding,
    operation: PeerOperation,
}

impl InvocationRegistry {
    pub(super) fn bind(
        &self,
        host: &RuntimeHost,
        lease: atman_rt::TurnLease<AtmanUuid>,
        state: tokio::sync::watch::Receiver<atman_rt::FlowInstance<AtmanUuid>>,
        access: &Arc<std::sync::RwLock<crate::flow_store::access::SessionAccess>>,
    ) -> Arc<InvocationIdentity> {
        let nonce = Uuid::new_v4();
        let identity = Arc::new(InvocationIdentity {
            principal: InvocationPrincipal {
                owner: host.generation(),
                nonce,
                source: lease.flow.clone(),
            },
            lease,
            state,
            access: Arc::downgrade(access),
            effects: Default::default(),
        });
        let mut clients = self.clients.lock().unwrap();
        clients.retain(|_, client| client.strong_count() != 0);
        clients.insert(nonce, Arc::downgrade(&identity));
        identity
    }

    fn client(
        &self,
        host: &RuntimeHost,
        principal: &InvocationPrincipal,
    ) -> io::Result<Arc<FlowClient>> {
        if principal.owner != host.generation() {
            return Err(denied("invocation owner generation changed"));
        }
        let client = self
            .clients
            .lock()
            .unwrap()
            .get(&principal.nonce)
            .and_then(Weak::upgrade)
            .ok_or_else(|| denied("original invocation principal is unavailable"))?;
        if client.lease.flow != principal.source {
            return Err(denied("invocation source identity differs"));
        }
        Ok(Arc::new(FlowClient::from_peer_identity(
            host.clone(),
            client,
        )?))
    }
}

fn denied(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

pub(super) fn protocol(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PeerFailure {
    pub kind: PeerFailureKind,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum PeerFailureKind {
    Recipient(crate::flow_store::outbox::DeliveryFailure),
    Denied,
    NotFound,
    InvalidInput,
    InvalidData,
    Unavailable,
    TimedOut,
    Interrupted,
    Busy,
    Conflict,
    Other,
}

pub fn encode_error(error: io::Error) -> PeerFailure {
    let kind = if let Some(rejection) = error
        .get_ref()
        .and_then(|cause| cause.downcast_ref::<crate::flow_store::outbox::RecipientRejected>())
    {
        PeerFailureKind::Recipient(rejection.0.clone())
    } else {
        match error.kind() {
            io::ErrorKind::PermissionDenied => PeerFailureKind::Denied,
            io::ErrorKind::NotFound => PeerFailureKind::NotFound,
            io::ErrorKind::InvalidInput => PeerFailureKind::InvalidInput,
            io::ErrorKind::InvalidData => PeerFailureKind::InvalidData,
            io::ErrorKind::NotConnected
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::BrokenPipe => PeerFailureKind::Unavailable,
            io::ErrorKind::TimedOut => PeerFailureKind::TimedOut,
            io::ErrorKind::Interrupted => PeerFailureKind::Interrupted,
            io::ErrorKind::WouldBlock => PeerFailureKind::Busy,
            io::ErrorKind::AlreadyExists => PeerFailureKind::Conflict,
            _ => PeerFailureKind::Other,
        }
    };
    PeerFailure {
        kind,
        message: error.to_string(),
    }
}

pub fn decode_error(error: PeerFailure) -> io::Error {
    let kind = match error.kind {
        PeerFailureKind::Recipient(failure) => {
            return io::Error::other(crate::flow_store::outbox::RecipientRejected(failure));
        }
        PeerFailureKind::Denied => io::ErrorKind::PermissionDenied,
        PeerFailureKind::NotFound => io::ErrorKind::NotFound,
        PeerFailureKind::InvalidInput => io::ErrorKind::InvalidInput,
        PeerFailureKind::InvalidData => io::ErrorKind::InvalidData,
        PeerFailureKind::Unavailable => io::ErrorKind::NotConnected,
        PeerFailureKind::TimedOut => io::ErrorKind::TimedOut,
        PeerFailureKind::Interrupted => io::ErrorKind::Interrupted,
        PeerFailureKind::Busy => io::ErrorKind::WouldBlock,
        PeerFailureKind::Conflict => io::ErrorKind::AlreadyExists,
        PeerFailureKind::Other => io::ErrorKind::Other,
    };
    io::Error::new(kind, error.message)
}

pub(crate) use durable::deliver_outbox;
pub(crate) use observation::scan_subscription;
pub(in crate::flow_runtime) use observation::{replace_native, watch_native};

pub use dispatch::handle_peer_call;
