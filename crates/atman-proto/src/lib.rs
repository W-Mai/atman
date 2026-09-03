use serde::{Deserialize, Serialize, de::DeserializeOwned};
use utoipa::ToSchema;
use uuid::Uuid;

mod projection;

pub use projection::*;

pub const JSONRPC_VERSION: &str = "2.0";
pub const PROTOCOL_VERSION: u32 = 1;
pub const EVENT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, ToSchema)]
#[serde(transparent)]
#[schema(value_type = String, format = Uuid)]
pub struct SessionId(pub Uuid);

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, ToSchema)]
#[serde(transparent)]
#[schema(value_type = String, format = Uuid)]
pub struct FlowRunId(pub Uuid);

impl std::fmt::Display for FlowRunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, ToSchema)]
#[serde(transparent)]
#[schema(value_type = String, format = Uuid)]
pub struct PromptId(pub Uuid);

impl std::fmt::Display for PromptId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, ToSchema)]
#[serde(transparent)]
#[schema(value_type = String, format = Uuid)]
pub struct ClientId(pub Uuid);

impl ClientId {
    pub fn now() -> Self {
        Self(Uuid::now_v7())
    }
}

impl std::fmt::Display for ClientId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, ToSchema)]
#[serde(transparent)]
#[schema(value_type = String, format = Uuid)]
pub struct RequestId(pub Uuid);

impl RequestId {
    pub fn now() -> Self {
        Self(Uuid::now_v7())
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, ToSchema)]
#[serde(transparent)]
pub struct ProjectId(pub String);

impl std::fmt::Display for ProjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, ToSchema)]
#[serde(transparent)]
pub struct DaemonGeneration(pub String);

#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    ToSchema,
)]
#[serde(transparent)]
pub struct EventCursor(pub u64);

#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    ToSchema,
)]
#[serde(transparent)]
pub struct Revision(pub u64);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RpcKind {
    Command,
    Query,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RpcMethodDescriptor {
    pub name: &'static str,
    pub kind: RpcKind,
    pub revision: u32,
}

pub trait RpcMethod {
    const NAME: &'static str;
    const KIND: RpcKind;
    const REVISION: u32 = 1;
    type Params: Serialize + DeserializeOwned;
    type Output: Serialize + DeserializeOwned;
}

pub const fn method_descriptor<M: RpcMethod>() -> RpcMethodDescriptor {
    RpcMethodDescriptor {
        name: M::NAME,
        kind: M::KIND,
        revision: M::REVISION,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    #[schema(value_type = Option<Object>)]
    pub id: Option<serde_json::Value>,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcRequest {
    pub fn new(
        id: impl Into<serde_json::Value>,
        method: impl Into<String>,
        params: serde_json::Value,
    ) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id: Some(id.into()),
            method: method.into(),
            params: Some(params),
        }
    }

    pub fn for_method<M: RpcMethod>(
        id: impl Into<serde_json::Value>,
        params: &M::Params,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self::new(id, M::NAME, serde_json::to_value(params)?))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[schema(value_type = Option<Object>)]
    pub id: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    pub fn ok(id: Option<serde_json::Value>, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Option<serde_json::Value>, error: JsonRpcError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            result: None,
            error: Some(error),
        }
    }

    pub fn into_method_output<M: RpcMethod>(self) -> Result<M::Output, JsonRpcError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let result = self
            .result
            .ok_or_else(|| JsonRpcError::internal("JSON-RPC response has no result"))?;
        serde_json::from_value(result)
            .map_err(|error| JsonRpcError::internal(format!("invalid {} result: {error}", M::NAME)))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error, ToSchema)]
#[error("json-rpc error {code}: {message}")]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub data: Option<serde_json::Value>,
}

impl JsonRpcError {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;
    pub const APPLICATION_ERROR: i32 = -32000;

    pub fn parse_error(msg: impl Into<String>) -> Self {
        Self {
            code: Self::PARSE_ERROR,
            message: msg.into(),
            data: None,
        }
    }

    pub fn method_not_found(method: &str) -> Self {
        Self {
            code: Self::METHOD_NOT_FOUND,
            message: format!("method not found: {method}"),
            data: None,
        }
    }

    pub fn invalid_params(msg: impl Into<String>) -> Self {
        Self {
            code: Self::INVALID_PARAMS,
            message: msg.into(),
            data: None,
        }
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            code: Self::INTERNAL_ERROR,
            message: msg.into(),
            data: None,
        }
    }

    pub fn application(msg: impl Into<String>) -> Self {
        Self {
            code: Self::APPLICATION_ERROR,
            message: msg.into(),
            data: None,
        }
    }
}

pub mod methods {
    pub const DAEMON_CAPABILITIES: &str = "daemon.capabilities";
    pub const START_RUN: &str = "run.start";
    pub const RUN_FLOW: &str = "run_flow";
    pub const CANCEL_RUN: &str = "cancel_run";
    pub const CREATE_SESSION: &str = "session.create";
    pub const SEND_MESSAGE: &str = "session.send_message";
    pub const INTERJECT_SESSION: &str = "session.interject";
    pub const LIST_PROJECTS: &str = "project.list";
    pub const LIST_SESSIONS: &str = "list_sessions";
    pub const RENAME_SESSION: &str = "rename_session";
    pub const GET_EVENTS: &str = "get_events";
    pub const GET_SESSION_SNAPSHOT: &str = "session.get_snapshot";
    pub const GET_SESSION_UPDATES: &str = "session.get_updates";
    pub const RESOLVE_PROMPT: &str = "resolve_prompt";
    pub const SUBMIT_FORM: &str = "form.submit";
    pub const RESOLVE_COMPACT_REVIEW: &str = "compact_review.resolve";
    pub const LIST_PERMISSION_REQUESTS: &str = "list_permission_requests";
    pub const CREATE_PERMISSION_GROUP: &str = "create_permission_group";
    pub const RESOLVE_PERMISSION_REQUESTS: &str = "resolve_permission_requests";
    pub const LIST_RESOURCES: &str = "resource.list";
    pub const INSPECT_RESOURCE: &str = "resource.inspect";
    pub const TERMINATE_RESOURCE: &str = "resource.terminate";
    pub const RETAIN_RESOURCE: &str = "resource.retain";
    pub const RELEASE_RESOURCE: &str = "resource.release";
    pub const PING: &str = "ping";

    pub const ALL: &[super::RpcMethodDescriptor] = &[
        super::method_descriptor::<super::rpc::DaemonCapabilities>(),
        super::method_descriptor::<super::rpc::Ping>(),
        super::method_descriptor::<super::rpc::CreateSession>(),
        super::method_descriptor::<super::rpc::SendMessage>(),
        super::method_descriptor::<super::rpc::InterjectSession>(),
        super::method_descriptor::<super::rpc::ListProjects>(),
        super::method_descriptor::<super::rpc::ListSessions>(),
        super::method_descriptor::<super::rpc::RenameSession>(),
        super::method_descriptor::<super::rpc::StartRun>(),
        super::method_descriptor::<super::rpc::RunFlow>(),
        super::method_descriptor::<super::rpc::CancelRun>(),
        super::method_descriptor::<super::rpc::GetEvents>(),
        super::method_descriptor::<super::rpc::GetSessionSnapshot>(),
        super::method_descriptor::<super::rpc::GetSessionUpdates>(),
        super::method_descriptor::<super::rpc::ResolvePrompt>(),
        super::method_descriptor::<super::rpc::SubmitForm>(),
        super::method_descriptor::<super::rpc::ResolveCompactReview>(),
        super::method_descriptor::<super::rpc::ListPermissionRequests>(),
        super::method_descriptor::<super::rpc::CreatePermissionGroup>(),
        super::method_descriptor::<super::rpc::ResolvePermissionRequests>(),
        super::method_descriptor::<super::rpc::ListResources>(),
        super::method_descriptor::<super::rpc::InspectResource>(),
        super::method_descriptor::<super::rpc::TerminateResource>(),
        super::method_descriptor::<super::rpc::RetainResource>(),
        super::method_descriptor::<super::rpc::ReleaseResource>(),
    ];
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct EmptyParams {}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PingResponse {
    pub pong: bool,
    pub version: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct CapabilitiesRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<ClientId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct MethodCapability {
    pub name: String,
    pub kind: RpcKind,
    pub revision: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct ProtocolLimits {
    pub max_event_page_size: usize,
    pub subscriber_buffer: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct CapabilitiesResponse {
    pub protocol_version: u32,
    pub daemon_version: String,
    pub daemon_generation: DaemonGeneration,
    pub event_schema_version: u32,
    pub methods: Vec<MethodCapability>,
    pub limits: ProtocolLimits,
}

impl CapabilitiesResponse {
    pub fn supports<M: RpcMethod>(&self) -> bool {
        self.methods
            .iter()
            .any(|method| method.name == M::NAME && method.revision >= M::REVISION)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct ListProjectsRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct ProjectSummary {
    pub id: ProjectId,
    pub name: String,
    pub root: String,
    pub session_count: usize,
    pub active_session_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_session_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct ListProjectsResponse {
    pub projects: Vec<ProjectSummary>,
    pub total: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct ListSessionsRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct CreateSessionRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RenameSessionRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
    pub session_id: SessionId,
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RenameSessionResponse {
    pub session: SessionSummary,
    pub revision: Revision,
    pub cursor: EventCursor,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SendMessageRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
    pub session_id: SessionId,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<InlineImage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SendMessageResponse {
    pub session_id: SessionId,
    pub run_id: FlowRunId,
    pub revision: Revision,
    pub cursor: EventCursor,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum InterjectionLevel {
    Nudge,
    CourseCorrect,
    Redirect,
    HardStop,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum InterjectionState {
    Pending,
    Injected,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InterjectSessionRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
    pub session_id: SessionId,
    pub run_id: FlowRunId,
    pub text: String,
    pub level: InterjectionLevel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_target: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct InterjectSessionResponse {
    pub session_id: SessionId,
    pub run_id: FlowRunId,
    pub injection_id: Uuid,
    pub state: InterjectionState,
    pub revision: Revision,
    pub cursor: EventCursor,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CancelRunResponse {
    pub cancelled: bool,
    pub status: RunCancellationStatus,
    pub session_id: SessionId,
    pub run_id: FlowRunId,
    pub revision: Revision,
    pub cursor: EventCursor,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunCancellationStatus {
    Accepted,
    AlreadyRequested,
    NotFound,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResolvePromptResponse {
    pub resolved: bool,
    pub status: PromptResolutionStatus,
    pub session_id: SessionId,
    pub prompt_id: PromptId,
    pub revision: Revision,
    pub cursor: EventCursor,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PromptResolutionStatus {
    Resolved,
    AlreadyResolved,
    Abandoned,
    NotFound,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FormSubmission {
    Submitted { answers: Vec<FormAnswer> },
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FormAnswer {
    Confirmed {
        value: bool,
    },
    Selected {
        index: usize,
        label: String,
    },
    MultiSelected {
        indices: Vec<usize>,
        labels: Vec<String>,
    },
    TextEntered {
        text: String,
    },
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SubmitFormRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
    pub session_id: SessionId,
    pub form_id: String,
    pub submission: FormSubmission,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FormResolutionStatus {
    Resolved,
    AlreadyResolved,
    Abandoned,
    NotFound,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct SubmitFormResponse {
    pub resolved: bool,
    pub status: FormResolutionStatus,
    pub session_id: SessionId,
    pub form_id: String,
    pub revision: Revision,
    pub cursor: EventCursor,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum CompactReviewDecision {
    AcceptAsIs,
    AcceptEdited { summary: String },
    Reject,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResolveCompactReviewRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
    pub session_id: SessionId,
    pub review_id: String,
    pub decision: CompactReviewDecision,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CompactReviewResolutionStatus {
    Resolved,
    AlreadyResolved,
    Abandoned,
    NotFound,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct ResolveCompactReviewResponse {
    pub resolved: bool,
    pub status: CompactReviewResolutionStatus,
    pub session_id: SessionId,
    pub review_id: String,
    pub revision: Revision,
    pub cursor: EventCursor,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RunFlowRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
    pub flow_path: String,
    #[serde(default)]
    #[schema(value_type = Object)]
    pub args: serde_json::Map<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<InlineImage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InlineImage {
    pub data_base64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RunFlowResponse {
    pub session_id: SessionId,
    pub run_id: FlowRunId,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct StartRunRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
    pub session_id: SessionId,
    pub flow_path: String,
    #[serde(default)]
    #[schema(value_type = Object)]
    pub args: serde_json::Map<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<InlineImage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct StartRunResponse {
    pub session_id: SessionId,
    pub run_id: FlowRunId,
    pub revision: Revision,
    pub cursor: EventCursor,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CancelRunRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
    pub session_id: SessionId,
    pub run_id: FlowRunId,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SessionSummary {
    pub id: SessionId,
    pub event_count: usize,
    pub first_ts: Option<chrono::DateTime<chrono::Utc>>,
    pub status: SessionStatus,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub goal: Option<String>,
    #[serde(default)]
    pub project_root: Option<String>,
    #[serde(default)]
    pub name_source: NameSource,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum NameSource {
    #[default]
    Auto,
    User,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Running,
    Finished,
    Pending,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GetEventsRequest {
    pub session_id: SessionId,
    #[serde(default)]
    pub since_seq: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResolvePromptRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
    pub session_id: SessionId,
    pub prompt_id: PromptId,
    #[schema(value_type = Object)]
    pub answer: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ListPermissionRequestsRequest {
    pub session_id: SessionId,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreatePermissionGroupRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
    pub session_id: SessionId,
    pub request_ids: Vec<Uuid>,
    pub expected_request_revisions: std::collections::BTreeMap<Uuid, u64>,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionRpcAction {
    Approve,
    Deny,
    Defer,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionRpcScope {
    CurrentCall,
    ChildRunSameTool {
        run_id: FlowRunId,
        tool_name: String,
    },
    ChildRunSamePathRule {
        run_id: FlowRunId,
        tool_name: String,
        workspace_relative_path: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum PermissionRpcSelector {
    Requests {
        request_ids: Vec<Uuid>,
        expected_request_revisions: std::collections::BTreeMap<Uuid, u64>,
    },
    Group {
        group_id: Uuid,
        expected_group_revision: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResolvePermissionRequestsRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
    pub session_id: SessionId,
    pub selector: PermissionRpcSelector,
    pub action: PermissionRpcAction,
    #[serde(default)]
    pub scope: Option<PermissionRpcScope>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct PermissionRequestView {
    pub request_id: Uuid,
    pub session_id: String,
    pub requesting_run_id: FlowRunId,
    pub tool: String,
    pub tier: String,
    pub state: String,
    pub target: String,
    pub revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct PermissionGroupView {
    pub group_id: Uuid,
    pub label: String,
    pub request_ids: Vec<Uuid>,
    pub revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ListPermissionRequestsResponse {
    pub session_id: SessionId,
    pub requests: Vec<PermissionRequestView>,
    pub groups: Vec<PermissionGroupView>,
    pub revision: Revision,
    pub cursor: EventCursor,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PermissionResolutionView {
    pub request_id: Uuid,
    pub outcome: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResolvePermissionRequestsResponse {
    pub session_id: SessionId,
    pub resolutions: Vec<PermissionResolutionView>,
    pub revision: Revision,
    pub cursor: EventCursor,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ServerEventEnvelope {
    pub schema_version: u32,
    pub cursor: EventCursor,
    #[schema(value_type = Object)]
    pub event: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GetEventsResponse {
    pub events: Vec<ServerEventEnvelope>,
    pub next_cursor: EventCursor,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreatePermissionGroupResponse {
    pub session_id: SessionId,
    pub group_id: Uuid,
    pub request_ids: Vec<Uuid>,
    pub revision: u64,
    pub label: String,
    pub session_revision: Revision,
    pub cursor: EventCursor,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ListResourcesRequest {
    pub session_id: SessionId,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ListResourcesResponse {
    pub session_id: SessionId,
    pub resources: Vec<ResourceProjection>,
    pub revision: Revision,
    pub cursor: EventCursor,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InspectResourceRequest {
    pub session_id: SessionId,
    pub resource_id: ResourceId,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InspectResourceResponse {
    pub session_id: SessionId,
    pub resource: ResourceProjection,
    pub revision: Revision,
    pub cursor: EventCursor,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TerminateResourceRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
    pub session_id: SessionId,
    pub resource_id: ResourceId,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResourceTerminationStatus {
    Terminating,
    AlreadyTerminal,
    Unavailable,
    Unsupported,
    NotFound,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TerminateResourceResponse {
    pub session_id: SessionId,
    pub resource_id: ResourceId,
    pub status: ResourceTerminationStatus,
    pub revision: Revision,
    pub cursor: EventCursor,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RetainResourceRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
    pub session_id: SessionId,
    pub resource_id: ResourceId,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RetainResourceResponse {
    pub session_id: SessionId,
    pub resource: ResourceProjection,
    pub revision: Revision,
    pub cursor: EventCursor,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ReleaseResourceRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
    pub session_id: SessionId,
    pub resource_id: ResourceId,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ReleaseResourceResponse {
    pub session_id: SessionId,
    pub resource: ResourceProjection,
    pub revision: Revision,
    pub cursor: EventCursor,
}

pub mod rpc {
    use super::*;

    macro_rules! method {
        ($marker:ident, $name:expr, $kind:ident, $params:ty, $output:ty) => {
            pub struct $marker;

            impl RpcMethod for $marker {
                const NAME: &'static str = $name;
                const KIND: RpcKind = RpcKind::$kind;
                type Params = $params;
                type Output = $output;
            }
        };
        ($marker:ident, $name:expr, $kind:ident, $params:ty, $output:ty, $revision:expr) => {
            pub struct $marker;

            impl RpcMethod for $marker {
                const NAME: &'static str = $name;
                const KIND: RpcKind = RpcKind::$kind;
                const REVISION: u32 = $revision;
                type Params = $params;
                type Output = $output;
            }
        };
    }

    method!(
        DaemonCapabilities,
        methods::DAEMON_CAPABILITIES,
        Query,
        CapabilitiesRequest,
        CapabilitiesResponse
    );
    method!(Ping, methods::PING, Query, EmptyParams, PingResponse);
    method!(
        ListProjects,
        methods::LIST_PROJECTS,
        Query,
        ListProjectsRequest,
        ListProjectsResponse
    );
    method!(
        CreateSession,
        methods::CREATE_SESSION,
        Command,
        CreateSessionRequest,
        SessionSnapshot
    );
    method!(
        SendMessage,
        methods::SEND_MESSAGE,
        Command,
        SendMessageRequest,
        SendMessageResponse
    );
    method!(
        InterjectSession,
        methods::INTERJECT_SESSION,
        Command,
        InterjectSessionRequest,
        InterjectSessionResponse
    );
    method!(
        ListSessions,
        methods::LIST_SESSIONS,
        Query,
        ListSessionsRequest,
        Vec<SessionSummary>
    );
    method!(
        RenameSession,
        methods::RENAME_SESSION,
        Command,
        RenameSessionRequest,
        RenameSessionResponse,
        2
    );
    method!(
        RunFlow,
        methods::RUN_FLOW,
        Command,
        RunFlowRequest,
        RunFlowResponse
    );
    method!(
        StartRun,
        methods::START_RUN,
        Command,
        StartRunRequest,
        StartRunResponse
    );
    method!(
        CancelRun,
        methods::CANCEL_RUN,
        Command,
        CancelRunRequest,
        CancelRunResponse,
        2
    );
    method!(
        GetEvents,
        methods::GET_EVENTS,
        Query,
        GetEventsRequest,
        GetEventsResponse
    );
    method!(
        GetSessionSnapshot,
        methods::GET_SESSION_SNAPSHOT,
        Query,
        GetSessionSnapshotRequest,
        SessionSnapshot
    );
    method!(
        GetSessionUpdates,
        methods::GET_SESSION_UPDATES,
        Query,
        GetSessionUpdatesRequest,
        GetSessionUpdatesResponse
    );
    method!(
        ResolvePrompt,
        methods::RESOLVE_PROMPT,
        Command,
        ResolvePromptRequest,
        ResolvePromptResponse,
        2
    );
    method!(
        SubmitForm,
        methods::SUBMIT_FORM,
        Command,
        SubmitFormRequest,
        SubmitFormResponse
    );
    method!(
        ResolveCompactReview,
        methods::RESOLVE_COMPACT_REVIEW,
        Command,
        ResolveCompactReviewRequest,
        ResolveCompactReviewResponse
    );
    method!(
        ListPermissionRequests,
        methods::LIST_PERMISSION_REQUESTS,
        Query,
        ListPermissionRequestsRequest,
        ListPermissionRequestsResponse,
        2
    );
    method!(
        CreatePermissionGroup,
        methods::CREATE_PERMISSION_GROUP,
        Command,
        CreatePermissionGroupRequest,
        CreatePermissionGroupResponse,
        2
    );
    method!(
        ResolvePermissionRequests,
        methods::RESOLVE_PERMISSION_REQUESTS,
        Command,
        ResolvePermissionRequestsRequest,
        ResolvePermissionRequestsResponse,
        2
    );
    method!(
        ListResources,
        methods::LIST_RESOURCES,
        Query,
        ListResourcesRequest,
        ListResourcesResponse
    );
    method!(
        InspectResource,
        methods::INSPECT_RESOURCE,
        Query,
        InspectResourceRequest,
        InspectResourceResponse
    );
    method!(
        TerminateResource,
        methods::TERMINATE_RESOURCE,
        Command,
        TerminateResourceRequest,
        TerminateResourceResponse
    );
    method!(
        RetainResource,
        methods::RETAIN_RESOURCE,
        Command,
        RetainResourceRequest,
        RetainResourceResponse
    );
    method!(
        ReleaseResource,
        methods::RELEASE_RESOURCE,
        Command,
        ReleaseResourceRequest,
        ReleaseResourceResponse
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_rpc_request_round_trip() {
        let req = JsonRpcRequest::new(1, "run_flow", serde_json::json!({"flow_path": "x.at"}));
        let s = serde_json::to_string(&req).unwrap();
        let back: JsonRpcRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(back.method, "run_flow");
        assert_eq!(back.jsonrpc, "2.0");
    }

    #[test]
    fn json_rpc_error_response_shape() {
        let resp = JsonRpcResponse::err(
            Some(serde_json::json!(7)),
            JsonRpcError::method_not_found("foo"),
        );
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains("\"code\":-32601"));
        assert!(s.contains("\"id\":7"));
        assert!(!s.contains("\"result\""));
    }

    #[test]
    fn run_flow_request_deserialize_without_args() {
        let s = r#"{"flow_path":"examples/hello.at"}"#;
        let req: RunFlowRequest = serde_json::from_str(s).unwrap();
        assert_eq!(req.flow_path, "examples/hello.at");
        assert!(req.request_id.is_none());
        assert!(req.args.is_empty());
        assert!(req.reasoning.is_none());
        assert!(req.images.is_empty());
    }

    #[test]
    fn session_status_snake_case() {
        assert_eq!(
            serde_json::to_string(&SessionStatus::Running).unwrap(),
            "\"running\""
        );
    }

    #[test]
    fn method_registry_is_unique_and_complete() {
        let names: std::collections::BTreeSet<_> = methods::ALL
            .iter()
            .map(|descriptor| descriptor.name)
            .collect();
        assert_eq!(names.len(), methods::ALL.len());
        assert!(names.contains(methods::DAEMON_CAPABILITIES));
        assert!(names.contains(methods::GET_EVENTS));
        assert!(methods::ALL.iter().all(|method| method.revision > 0));
    }

    #[test]
    fn typed_request_and_response_round_trip() {
        let request = JsonRpcRequest::for_method::<rpc::ListSessions>(
            7,
            &ListSessionsRequest {
                project_root: Some("/workspace".into()),
                search: None,
                limit: Some(10),
            },
        )
        .unwrap();
        assert_eq!(request.method, methods::LIST_SESSIONS);

        let response = JsonRpcResponse::ok(
            request.id,
            serde_json::to_value(Vec::<SessionSummary>::new()).unwrap(),
        );
        assert!(
            response
                .into_method_output::<rpc::ListSessions>()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn capabilities_match_method_revision() {
        let capabilities = CapabilitiesResponse {
            protocol_version: PROTOCOL_VERSION,
            daemon_version: "test".into(),
            daemon_generation: DaemonGeneration("generation".into()),
            event_schema_version: EVENT_SCHEMA_VERSION,
            methods: methods::ALL
                .iter()
                .map(|method| MethodCapability {
                    name: method.name.into(),
                    kind: method.kind,
                    revision: method.revision,
                })
                .collect(),
            limits: ProtocolLimits {
                max_event_page_size: 100,
                subscriber_buffer: 64,
            },
        };
        assert!(capabilities.supports::<rpc::DaemonCapabilities>());
        assert!(capabilities.supports::<rpc::ResolvePermissionRequests>());
        assert!(capabilities.supports::<rpc::RetainResource>());
        assert!(capabilities.supports::<rpc::ReleaseResource>());
    }

    #[test]
    fn workspace_resource_actions_do_not_expose_force_deletion() {
        let request = ReleaseResourceRequest {
            request_id: Some(RequestId::now()),
            session_id: SessionId(uuid::Uuid::now_v7()),
            resource_id: ResourceId("workspace:flow-run".into()),
        };
        let encoded = serde_json::to_value(request).unwrap();

        assert!(encoded.get("request_id").is_some());
        assert!(encoded.get("force").is_none());
        assert_eq!(rpc::ReleaseResource::NAME, methods::RELEASE_RESOURCE);
    }
}
