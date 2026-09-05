use atman_proto::{JsonRpcRequest, JsonRpcResponse};
use utoipa::OpenApi;

use crate::http::{EventTicketRequest, EventTicketResponse};

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
    post,
    path = "/event-ticket",
    request_body = EventTicketRequest,
    responses(
        (status = 200, body = EventTicketResponse),
        (status = 401, description = "Missing or invalid bearer token"),
        (status = 403, description = "Authenticated principal is not authorized for the session"),
    ),
    security(("bearer_token" = [])),
    tag = "events",
)]
#[allow(dead_code)]
fn event_ticket_endpoint() {}

#[utoipa::path(
    get,
    path = "/events",
    params(
        ("session_id" = String, Query, description = "Session UUID"),
        ("since_seq" = Option<u64>, Query, description = "Resume from this seq (exclusive)"),
        ("ticket" = Option<String>, Query, description = "Short-lived session-scoped event ticket"),
    ),
    responses(
        (status = 200, description = "SSE stream (text/event-stream). Each data frame is a ServerEventEnvelope and each SSE id is its cursor."),
        (status = 401, description = "Missing or invalid bearer token"),
        (status = 403, description = "Authenticated principal is not authorized for the session"),
    ),
    security(("bearer_token" = []), ("event_ticket" = [])),
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
        ("ticket" = Option<String>, Query, description = "Short-lived session-scoped event ticket"),
    ),
    responses(
        (status = 200, description = "SSE projection stream. Each data frame is a ProjectionEventEnvelope and each SSE id is its cursor."),
        (status = 401, description = "Missing or invalid bearer token"),
        (status = 403, description = "Authenticated principal is not authorized for the session"),
    ),
    security(("bearer_token" = []), ("event_ticket" = [])),
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
Methods dispatched at POST /rpc: daemon.capabilities, ping, project.list, config.initialize, config.provider.mutate, config.provider.probe, config.model.upsert, config.model.switch_default, mcp.list, mcp.mutate, mcp.probe, mcp.tools, mcp.resources, mcp.prompts, session.create, session.close, session.delete, session.sanitize_attachments, session.import_messages, session.send_message, session.interject, session.set_goal, session.update_todos, session.update_trust, session.reload_mcp, session.auto_name, session.suggest_flow, session.install_suggested_flow, session.move, session.compact, list_sessions, rename_session, run.start, run_flow, cancel_run, get_events, session.get_snapshot, session.get_updates, resolve_prompt, form.submit, compact_review.resolve, list_permission_requests, create_permission_group, resolve_permission_requests, resource.list, resource.inspect, resource.terminate, resource.resize_terminal, resource.retain, resource.release. \
Raw event-log SSE is available at GET /events. Convergent session projection SSE is available at GET /session-events. RPC and ticket issuance require a bearer token; event streams also accept a short-lived event ticket."
    ),
    paths(
        rpc_endpoint,
        event_ticket_endpoint,
        sse_endpoint,
        session_sse_endpoint,
        openapi_endpoint
    ),
    modifiers(&ProtocolSchemas, &BearerSecurity),
)]
pub struct AtmanOpenApi;

struct ProtocolSchemas;

impl utoipa::Modify for ProtocolSchemas {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let generated = atman_proto::protocol_openapi_components()
            .expect("typed protocol schemas must form a valid OpenAPI component registry");
        let components = openapi.components.get_or_insert_default();
        components.schemas.extend(generated.schemas);
    }
}

struct BearerSecurity;

impl utoipa::Modify for BearerSecurity {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        use utoipa::openapi::security::{
            ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityScheme,
        };
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "bearer_token",
                SecurityScheme::Http(HttpBuilder::new().scheme(HttpAuthScheme::Bearer).build()),
            );
            components.add_security_scheme(
                "event_ticket",
                SecurityScheme::ApiKey(ApiKey::Query(ApiKeyValue::new("ticket"))),
            );
        }
    }
}
