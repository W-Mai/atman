//! Typed access to an existing runtime owner over native handles or authenticated HTTP.
//!
//! Native view and live readers retain their original runtime handles. Portable operations preserve the caller's request identity and do not retry writes after transport failures.

use std::num::NonZeroUsize;

use atman_proto::{
    capabilities::{CapabilitiesRequest, CapabilitiesResponse},
    flows::{
        ControlFlowRequest, FlowRequest, InputStatusRequest, InvokeFlowRequest, ListFlowsRequest,
        ReadFlowEventsRequest, SessionRequest,
    },
    responses::{
        ControlFlowResponse, FlowEventPage, FlowListPage, InputStatusResponse, InvokeFlowResponse,
        SessionView,
    },
};
use serde::{Deserialize, Serialize};

mod http;
mod native;
mod subscription;

#[cfg(test)]
mod tests;

pub use http::HttpClient;
pub use native::NativeClient;
pub use subscription::EventSubscription;

/// Runtime execution data already returned by `flow.get`, without an execution handle.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ExecutionView {
    pub revision: u64,
    pub state: atman_rt::InstanceState<atman_runtime::event::AtmanUuid>,
    pub pause_count: usize,
    pub pause_keys: Vec<atman_runtime::event::AtmanUuid>,
    pub can_start_turn: bool,
}

/// A typed projection of the existing `flow.get` reply.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct FlowObservation {
    pub flow: atman_proto::flows::FlowAddress,
    pub history_head: Option<atman_proto::history::HistoryHead>,
    pub execution: ExecutionView,
    pub goal: Option<atman_runtime::memory::goal::GoalRecord>,
    pub last_completion: Option<atman_runtime::flow_store::completion::CommittedCompletion>,
    pub cursor: atman_proto::flows::FlowCursor,
    pub scheduler: Option<atman_proto::responses::SchedulingView>,
    pub close_error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error(transparent)]
    Runtime(#[from] std::io::Error),
    #[error(transparent)]
    Attachment(#[from] Box<atman_runtime::RuntimeError>),
    #[error(transparent)]
    Transport(#[from] reqwest::Error),
    #[error(transparent)]
    Rpc(#[from] atman_proto::client::RpcDecodeError),
    #[error(transparent)]
    Capability(#[from] atman_proto::client::CapabilityRequirementError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    EventCursor(#[from] atman_proto::client::FlowEventCursorError),
    #[error("invalid host endpoint: {0}")]
    InvalidEndpoint(&'static str),
    #[error("invalid request: {0}")]
    InvalidRequest(&'static str),
    #[error("event stream failed: {0}")]
    EventStream(String),
    #[error("event receipt does not belong to the subscribed Flow")]
    EventSource,
    #[error("response identity does not match the original request")]
    ResponseSource,
    #[error("the native-only operation requires a native client")]
    NativeRequired,
    #[error("the request identity exists in both retained operator namespaces")]
    AmbiguousRequest,
}

pub type Result<T> = std::result::Result<T, ClientError>;

/// A shared operation surface; selecting a backend never changes a business request ID.
#[derive(Clone)]
pub enum Client {
    Native(NativeClient),
    Http(HttpClient),
}

macro_rules! delegate {
    ($name:ident, $request:ty, $response:ty) => {
        pub async fn $name(&self, request: $request) -> Result<$response> {
            match self {
                Self::Native(client) => client.$name(request).await,
                Self::Http(client) => client.$name(request).await,
            }
        }
    };
}

impl Client {
    delegate!(capabilities, CapabilitiesRequest, CapabilitiesResponse);
    delegate!(open_session, SessionRequest, SessionView);
    delegate!(list_flows, ListFlowsRequest, FlowListPage);
    delegate!(get_flow, FlowRequest, FlowObservation);
    delegate!(invoke, InvokeFlowRequest, InvokeFlowResponse);
    delegate!(input_status, InputStatusRequest, InputStatusResponse);
    delegate!(control, ControlFlowRequest, ControlFlowResponse);
    delegate!(events, ReadFlowEventsRequest, FlowEventPage);
    delegate!(subscribe_events, ReadFlowEventsRequest, EventSubscription);

    /// Access native view/live extensions without passing through the portable wire format.
    pub fn native(&self) -> Result<&NativeClient> {
        match self {
            Self::Native(client) => Ok(client),
            Self::Http(_) => Err(ClientError::NativeRequired),
        }
    }
}

pub(crate) fn limit(value: usize, maximum: usize) -> Result<NonZeroUsize> {
    NonZeroUsize::new(value)
        .filter(|value| value.get() <= maximum)
        .ok_or(ClientError::InvalidRequest(
            "page limit is outside the supported range",
        ))
}

pub(crate) fn flow_identity(flow: &atman_proto::flows::FlowAddress) -> Result<()> {
    if flow.session_id.0.is_nil() || flow.instance_id.is_nil() {
        Err(ClientError::InvalidRequest(
            "session and flow identities must not be nil",
        ))
    } else {
        Ok(())
    }
}
