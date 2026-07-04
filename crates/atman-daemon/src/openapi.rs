use atman_proto::{
    CancelRunRequest, GetEventsRequest, JsonRpcError, JsonRpcRequest, JsonRpcResponse,
    ResolvePromptRequest, RunFlowRequest, RunFlowResponse, SessionSummary,
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
        (status = 200, description = "SSE stream (text/event-stream). Payload is a JSON-serialized atman_runtime::event::Event per line."),
        (status = 401, description = "Missing or invalid bearer token"),
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
        description = "JSON-RPC 2.0 daemon for the atman flow runtime. \
Methods dispatched at POST /rpc: ping, list_sessions, run_flow, cancel_run, resolve_prompt. \
SSE event stream at GET /events. Every endpoint requires a bearer token."
    ),
    paths(rpc_endpoint, sse_endpoint, openapi_endpoint),
    components(schemas(
        JsonRpcRequest,
        JsonRpcResponse,
        JsonRpcError,
        RunFlowRequest,
        RunFlowResponse,
        CancelRunRequest,
        ResolvePromptRequest,
        GetEventsRequest,
        SessionSummary,
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
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "bearer_token",
                SecurityScheme::Http(HttpBuilder::new().scheme(HttpAuthScheme::Bearer).build()),
            );
        }
    }
}
