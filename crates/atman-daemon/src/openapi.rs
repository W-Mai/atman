use atman_proto::{
    CancelRunRequest, CancelRunResponse, CapabilitiesRequest, CapabilitiesResponse,
    CreatePermissionGroupRequest, CreatePermissionGroupResponse, CreateSessionRequest,
    GetEventsRequest, GetEventsResponse, InspectResourceRequest, InspectResourceResponse,
    InterjectSessionRequest, InterjectSessionResponse, JsonRpcError, JsonRpcRequest,
    JsonRpcResponse, ListPermissionRequestsRequest, ListResourcesRequest, ListResourcesResponse,
    ListSessionsRequest, MethodCapability, ProtocolLimits, RenameSessionRequest,
    RenameSessionResponse, ResolveCompactReviewRequest, ResolveCompactReviewResponse,
    ResolvePermissionRequestsRequest, ResolvePromptRequest, ResolvePromptResponse, RunFlowRequest,
    RunFlowResponse, SendMessageRequest, SendMessageResponse, ServerEventEnvelope, SessionSummary,
    StartRunRequest, StartRunResponse, SubmitFormRequest, SubmitFormResponse,
    TerminateResourceRequest, TerminateResourceResponse,
};
use utoipa::OpenApi;

#[utoipa::path(
    post,
    path = "/rpc",
    request_body = JsonRpcRequest,
    responses(
        (status = 200, body = JsonRpcResponse),
        (status = 401, description = "Missing or invalid bearer token"),
    ),
    security(("bearer_token" = [])),
    tag = "rpc",
)]
#[allow(dead_code)]
fn rpc_endpoint() {}

#[utoipa::path(
    get,
    path = "/events",
    params(
        ("session_id" = String, Query, description = "Session UUID"),
        ("since_seq" = Option<u64>, Query, description = "Resume from this seq (exclusive)"),
        ("token" = Option<String>, Query, description = "Bearer token fallback for EventSource (query only, GET only)"),
    ),
    responses(
        (status = 200, description = "SSE stream (text/event-stream). Each data frame is a ServerEventEnvelope and each SSE id is its cursor."),
        (status = 401, description = "Missing or invalid bearer token"),
        (status = 403, description = "Authenticated principal is not authorized for the session"),
    ),
    security(("bearer_token" = [])),
    tag = "events",
)]
#[allow(dead_code)]
fn sse_endpoint() {}

#[utoipa::path(
    get,
    path = "/session-events",
    params(
        ("session_id" = String, Query, description = "Session UUID"),
        ("after_cursor" = Option<u64>, Query, description = "Resume from this projection cursor (exclusive)"),
        ("token" = Option<String>, Query, description = "Bearer token fallback for EventSource (query only, GET only)"),
    ),
    responses(
        (status = 200, description = "SSE projection stream. Each data frame is a ProjectionEventEnvelope and each SSE id is its cursor."),
        (status = 401, description = "Missing or invalid bearer token"),
        (status = 403, description = "Authenticated principal is not authorized for the session"),
    ),
    security(("bearer_token" = [])),
    tag = "events",
)]
#[allow(dead_code)]
fn session_sse_endpoint() {}

#[utoipa::path(
    get,
    path = "/openapi.json",
    responses((status = 200, description = "OpenAPI 3.1 schema as JSON")),
    security(("bearer_token" = [])),
    tag = "meta",
)]
#[allow(dead_code)]
fn openapi_endpoint() {}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "atman daemon",
        version = env!("CARGO_PKG_VERSION"),
        description = "JSON-RPC 2.0 daemon for the atman flow runtime. \
Methods dispatched at POST /rpc: daemon.capabilities, ping, session.create, session.send_message, session.interject, list_sessions, rename_session, run.start, run_flow, cancel_run, get_events, session.get_snapshot, session.get_updates, resolve_prompt, form.submit, compact_review.resolve, list_permission_requests, create_permission_group, resolve_permission_requests, resource.list, resource.inspect, resource.terminate. \
Raw event-log SSE is available at GET /events. Convergent session projection SSE is available at GET /session-events. Every endpoint requires a bearer token."
    ),
    paths(rpc_endpoint, sse_endpoint, session_sse_endpoint, openapi_endpoint),
    components(schemas(
        JsonRpcRequest,
        JsonRpcResponse,
        JsonRpcError,
        CapabilitiesRequest,
        CapabilitiesResponse,
        MethodCapability,
        ProtocolLimits,
        CreateSessionRequest,
        SendMessageRequest,
        SendMessageResponse,
        InterjectSessionRequest,
        InterjectSessionResponse,
        atman_proto::InterjectionLevel,
        atman_proto::InterjectionState,
        RunFlowRequest,
        RunFlowResponse,
        StartRunRequest,
        StartRunResponse,
        CancelRunRequest,
        CancelRunResponse,
        atman_proto::RunCancellationStatus,
        ResolvePromptRequest,
        ResolvePromptResponse,
        atman_proto::PromptResolutionStatus,
        SubmitFormRequest,
        SubmitFormResponse,
        atman_proto::FormSubmission,
        atman_proto::FormAnswer,
        atman_proto::FormResolutionStatus,
        ResolveCompactReviewRequest,
        ResolveCompactReviewResponse,
        atman_proto::CompactReviewDecision,
        atman_proto::CompactReviewResolutionStatus,
        GetEventsRequest,
        GetEventsResponse,
        ServerEventEnvelope,
        atman_proto::ProjectionEventEnvelope,
        atman_proto::ServerEvent,
        atman_proto::ProjectionDelta,
        atman_proto::ProjectionChange,
        atman_proto::SessionSignal,
        atman_proto::ResyncRequired,
        ListSessionsRequest,
        RenameSessionRequest,
        RenameSessionResponse,
        ListPermissionRequestsRequest,
        CreatePermissionGroupRequest,
        CreatePermissionGroupResponse,
        ResolvePermissionRequestsRequest,
        atman_proto::ListPermissionRequestsResponse,
        atman_proto::PermissionRequestView,
        atman_proto::PermissionGroupView,
        atman_proto::ResolvePermissionRequestsResponse,
        atman_proto::PermissionResolutionView,
        ListResourcesRequest,
        ListResourcesResponse,
        InspectResourceRequest,
        InspectResourceResponse,
        TerminateResourceRequest,
        TerminateResourceResponse,
        atman_proto::ResourceTerminationStatus,
        atman_proto::ResourceProjection,
        atman_proto::ResourceKind,
        atman_proto::ResourceState,
        atman_proto::ResourceId,
        SessionSummary,
        atman_proto::SessionStatus,
        atman_proto::SessionId,
        atman_proto::FlowRunId,
        atman_proto::PromptId,
        atman_proto::ClientId,
        atman_proto::RequestId,
        atman_proto::ProjectId,
        atman_proto::DaemonGeneration,
        atman_proto::EventCursor,
        atman_proto::Revision,
        atman_proto::RpcKind,
    )),
    modifiers(&BearerSecurity),
)]
pub struct AtmanOpenApi;

struct BearerSecurity;

impl utoipa::Modify for BearerSecurity {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "bearer_token",
                SecurityScheme::Http(HttpBuilder::new().scheme(HttpAuthScheme::Bearer).build()),
            );
        }
    }
}
