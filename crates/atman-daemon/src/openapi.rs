use atman_proto::{
    CancelRunRequest, CreatePermissionGroupRequest, GetEventsRequest, GoalRequest, JsonRpcError,
    JsonRpcRequest, JsonRpcResponse, ListPermissionRequestsRequest,
    ResolvePermissionRequestsRequest, ResolvePromptRequest, RunFlowRequest, RunFlowResponse,
    SessionSummary,
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
        ("instance_id" = Option<String>, Query, description = "Persistent flow instance UUID; emits committed flow_update batches"),
        ("after" = Option<String>, Query, description = "Initial instance cursor journal UUID:sequence:digest, exclusive"),
        ("Last-Event-ID" = Option<String>, Header, description = "Reconnect cursor from the last applied event; instance streams require the complete cursor and prefer it over the initial after query"),
        ("since_seq" = Option<u64>, Query, description = "Legacy session event sequence, exclusive; not accepted with instance_id"),
        ("token" = Option<String>, Query, description = "Bearer token fallback for EventSource (query only, GET only)"),
    ),
    responses(
        (status = 200, description = "SSE stream. Instance streams emit committed batches of typed FlowUpdate with a receipt; legacy session streams emit event::Event."),
        (status = 400, description = "Invalid address, cursor, or cursor namespace"),
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
        description = "JSON-RPC 2.0 daemon for the atman flow runtime."
    ),
    paths(rpc_endpoint, sse_endpoint, openapi_endpoint),
    components(schemas(
        JsonRpcRequest,
        JsonRpcResponse,
        JsonRpcError,
        RunFlowRequest,
        RunFlowResponse,
        GoalRequest,
        CancelRunRequest,
        ResolvePromptRequest,
        GetEventsRequest,
        ListPermissionRequestsRequest,
        atman_proto::PermissionTarget,
        CreatePermissionGroupRequest,
        ResolvePermissionRequestsRequest,
        atman_proto::ListPermissionRequestsResponse,
        atman_proto::PermissionRequestView,
        atman_proto::PermissionGroupView,
        atman_proto::ResolvePermissionRequestsResponse,
        atman_proto::PermissionResolutionView,
        SessionSummary,
        atman_proto::flows::FlowAddress,
        atman_proto::flows::SelectManagementRequest,
        atman_proto::flows::ManagementGrant,
        atman_proto::flows::ManagementAction,
        atman_proto::flows::ManagementTarget,
        atman_proto::flows::CreateSessionRequest,
        atman_proto::flows::SessionRequest,
        atman_proto::flows::ListFlowsRequest,
        atman_proto::flows::FlowRequest,
        atman_proto::flows::ControlFlowRequest,
        atman_proto::flows::ControlFlowBatchRequest,
        atman_proto::flows::ControlFlowStatusRequest,
        atman_proto::flows::FlowControl,
        atman_proto::flows::LaunchFlowRequest,
        atman_proto::flows::FlowUserContent,
        atman_proto::InlineImage,
        atman_proto::flows::InvokeFlowRequest,
        atman_proto::flows::InputStatusRequest,
        atman_proto::flows::ReadFlowOutputRequest,
        atman_proto::flows::ReadFlowEventsRequest,
        atman_proto::flows::FlowCursor,
        atman_proto::history::HistoryHead,
        atman_proto::history::HistoryAdmissionError,
        atman_proto::history::HistorySource,
        atman_proto::history::ConversationCutoff,
        atman_proto::history::HistoryTarget,
        atman_proto::history::HistoryReviewRequest,
        atman_proto::history::HistoryBranchesRequest,
        atman_proto::history::HistoryBranch,
        atman_proto::history::HistoryBranchesResponse,
        atman_proto::history::HistoryGoalVersion,
        atman_proto::history::HistoryQueueItem,
        atman_proto::history::HistoryOperationRequest,
        atman_proto::history::HistorySelection,
        atman_proto::history::HistoryReviewResponse,
        atman_proto::history::HistoryOperationQuery,
        atman_proto::history::HistoryOperationStatus,
        atman_proto::history::HistoryCancellation,
        atman_proto::history::HistoryOperationRecord,
        atman_proto::history::HistoryStatusResponse,
        atman_proto::methods::Method,
        atman_proto::capabilities::CapabilitiesRequest,
        atman_proto::capabilities::CapabilitiesResponse,
        atman_proto::capabilities::MethodCapability,
        atman_proto::capabilities::CapabilityMismatch,
        atman_proto::capabilities::CapabilityMismatchKind,
        atman_proto::responses::PingResponse,
        atman_proto::responses::CancelRunResponse,
        atman_proto::responses::ResolvePromptResponse,
        atman_proto::responses::ListSessionsRequest,
        atman_proto::responses::ListSessionsResponse,
        atman_proto::responses::SessionView,
        atman_proto::responses::SchedulingView,
        atman_proto::responses::BindFlowResponse,
        atman_proto::responses::ControlFlowResponse,
        atman_proto::responses::InputDisposition,
        atman_proto::responses::FlowAcceptance,
        atman_proto::responses::InvokeFlowResponse,
        atman_proto::responses::InputStatusResponse,
        atman_proto::responses::FlowInputObservation,
        atman_proto::responses::InputConsumption,
        atman_proto::responses::InputTurn,
        atman_proto::responses::LaunchFlowResponse,
        atman_proto::responses::FlowOutputPage,
        atman_proto::SessionStatus,
        atman_proto::SessionId,
        atman_proto::FlowRunId,
        atman_proto::PromptId,
    )),
    modifiers(&BearerSecurity),
)]
pub struct AtmanOpenApi;

struct BearerSecurity;

impl utoipa::Modify for BearerSecurity {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
        let methods = atman_proto::methods::Method::ALL
            .iter()
            .map(|method| method.name())
            .collect::<Vec<_>>()
            .join(", ");
        openapi.info.description = Some(format!(
            "JSON-RPC 2.0 daemon for the atman flow runtime. Methods dispatched at POST /rpc: {methods}. SSE event stream at GET /events. Every endpoint requires a bearer token. Capability and schema revisions are reported by capabilities."
        ));
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "bearer_token",
                SecurityScheme::Http(HttpBuilder::new().scheme(HttpAuthScheme::Bearer).build()),
            );
        }
    }
}
