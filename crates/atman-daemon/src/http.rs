use std::sync::Arc;
use std::time::Duration;

use axum::{
    Router,
    extract::{Extension, Query, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{
        IntoResponse, Json, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{get, post},
};
use futures::{Stream, StreamExt};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};

use atman_proto::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, SessionId};

use crate::DaemonState;

mod flow_events;
mod transport;

pub use transport::serve;

type EventStream = futures::stream::BoxStream<'static, Result<Event, std::io::Error>>;

#[derive(Clone)]
pub(crate) struct ListenerShutdown(pub tokio_util::sync::CancellationToken);

pub struct HttpState {
    pub daemon: Arc<DaemonState>,
    pub auth_token: String,
}

pub fn router(state: Arc<HttpState>) -> Router {
    Router::new()
        .route("/rpc", post(rpc_handler))
        .route("/events", get(sse_handler))
        .route("/openapi.json", get(openapi_handler))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_bearer,
        ))
        .with_state(state.clone())
        .merge(crate::peer_transport::router(state.daemon.clone()))
        .merge(crate::web::router())
}

async fn openapi_handler() -> Json<serde_json::Value> {
    use utoipa::OpenApi;
    let schema = crate::openapi::AtmanOpenApi::openapi();
    Json(serde_json::to_value(schema).unwrap_or(serde_json::json!({})))
}

async fn rpc_handler(
    State(state): State<Arc<HttpState>>,
    Extension(principal_id): Extension<String>,
    headers: HeaderMap,
    body: String,
) -> Json<JsonRpcResponse> {
    let req: JsonRpcRequest = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => {
            return Json(JsonRpcResponse::err(
                None,
                JsonRpcError::parse_error(e.to_string()),
            ));
        }
    };
    Json(
        if headers.contains_key(crate::host_gateway::FORWARDED_HEADER) {
            crate::dispatch_forwarded_as(state.daemon.clone(), req, &principal_id).await
        } else {
            crate::dispatch_as(state.daemon.clone(), req, &principal_id).await
        },
    )
}

#[derive(Deserialize, serde::Serialize)]
pub struct SseQuery {
    pub session_id: SessionId,
    #[serde(default)]
    pub instance_id: Option<uuid::Uuid>,
    #[serde(default)]
    pub after: Option<String>,
    #[serde(default)]
    pub since_seq: Option<u64>,
}

async fn sse_handler(
    State(state): State<Arc<HttpState>>,
    Extension(principal_id): Extension<String>,
    Query(q): Query<SseQuery>,
    headers: HeaderMap,
    listener: Option<Extension<ListenerShutdown>>,
) -> Result<Response, (StatusCode, String)> {
    let shutdown = listener.map_or_else(
        || state.daemon.shutdown_signal(),
        |Extension(listener)| listener.0,
    );
    if let Some(response) =
        crate::host_gateway::events(&state.daemon, &q, &headers, shutdown.clone()).await?
    {
        return Ok(response);
    }
    let target = crate::flow_service::resolve_target(
        &state.daemon,
        &q.session_id,
        q.instance_id,
        &principal_id,
    )
    .await
    .map_err(|error| (StatusCode::BAD_REQUEST, error.message))?;
    if let Some(flow) = target {
        let stream =
            flow_events::subscribe(&state.daemon, &principal_id, q, flow.instance_id, &headers)
                .await?;
        return Ok(
            event_stream(stream.take_until(shutdown.cancelled_owned()).boxed()).into_response(),
        );
    }
    if q.after.is_some() {
        return Err((
            StatusCode::BAD_REQUEST,
            "after requires an instance_id".into(),
        ));
    }
    if !state.daemon.owns_live_session(&q.session_id, &principal_id) {
        return Err((
            StatusCode::FORBIDDEN,
            "permission denied for session".into(),
        ));
    }
    let events_path = state
        .daemon
        .sessions_root()
        .join(q.session_id.0.to_string())
        .join("events.jsonl");
    let start = q.since_seq.or_else(|| last_event_id(&headers)).unwrap_or(0);
    Ok(event_stream(
        tail_events_stream(events_path, start)
            .take_until(shutdown.cancelled_owned())
            .boxed(),
    )
    .into_response())
}

fn event_stream(stream: EventStream) -> Sse<impl Stream<Item = Result<Event, std::io::Error>>> {
    let prelude =
        futures::stream::once(async { Ok(Event::default().retry(Duration::from_millis(3000))) });
    Sse::new(prelude.chain(stream).boxed())
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

fn last_event_id(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
}

fn tail_events_stream(
    path: std::path::PathBuf,
    start_line: u64,
) -> impl Stream<Item = Result<Event, std::io::Error>> {
    async_stream::stream! {
        let mut sent: u64 = 0;
        let mut reader: Option<BufReader<tokio::fs::File>> = None;
        loop {
            if reader.is_none() && path.exists() {
                if let Ok(f) = tokio::fs::File::open(&path).await {
                    reader = Some(BufReader::new(f));
                }
            }
            let Some(rd) = reader.as_mut() else {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            };
            let mut line = String::new();
            loop {
                line.clear();
                match rd.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        if !line.ends_with('\n') {
                            break;
                        }
                        let trimmed = line.trim_end();
                        sent += 1;
                        if sent > start_line && !trimmed.is_empty() {
                            let ev = Event::default()
                                .event("event")
                                .id(sent.to_string())
                                .data(trimmed);
                            yield Ok(ev);
                        }
                    }
                    Err(e) => {
                        yield Err(e);
                        return;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

async fn require_bearer(
    State(state): State<Arc<HttpState>>,
    headers: HeaderMap,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    if let Some(auth) = headers.get(axum::http::header::AUTHORIZATION)
        && let Ok(auth_str) = auth.to_str()
        && let Some(token) = auth_str.strip_prefix("Bearer ")
    {
        if constant_time_eq(token.as_bytes(), state.auth_token.as_bytes()) {
            return authenticated_request(&state, req, next).await;
        }
        return (StatusCode::UNAUTHORIZED, "invalid token").into_response();
    }
    // EventSource in browsers cannot set headers, so accept ?token=<t> as a fallback on GET.
    if req.method() == axum::http::Method::GET
        && let Some(q) = req.uri().query()
    {
        for pair in q.split('&') {
            if let Some(rest) = pair.strip_prefix("token=") {
                let decoded = urlencoding::decode(rest)
                    .map(|c| c.into_owned())
                    .unwrap_or_else(|_| rest.to_string());
                if constant_time_eq(decoded.as_bytes(), state.auth_token.as_bytes()) {
                    return authenticated_request(&state, req, next).await;
                }
            }
        }
    }
    (
        StatusCode::UNAUTHORIZED,
        "missing or invalid Authorization / token",
    )
        .into_response()
}

async fn authenticated_request(
    state: &HttpState,
    mut req: axum::extract::Request,
    next: Next,
) -> Response {
    use crate::host_gateway::{FORWARDED_HEADER, GENERATION_HEADER};
    if let Some(expected) = req.headers().get(GENERATION_HEADER)
        && expected.to_str().ok() != Some(state.daemon.daemon_generation())
    {
        return (
            StatusCode::CONFLICT,
            "host generation changed; refresh discovery",
        )
            .into_response();
    }
    if req.headers().contains_key(FORWARDED_HEADER)
        && (!req.headers().contains_key(GENERATION_HEADER)
            || req
                .headers()
                .get(FORWARDED_HEADER)
                .and_then(|value| value.to_str().ok())
                != Some("1"))
    {
        return (
            StatusCode::BAD_REQUEST,
            "forwarded requests require a complete host identity",
        )
            .into_response();
    }
    req.extensions_mut()
        .insert(crate::state::LOCAL_OPERATOR.to_owned());
    let mut response = next.run(req).await;
    if let Ok(generation) = axum::http::HeaderValue::from_str(state.daemon.daemon_generation()) {
        response.headers_mut().insert(GENERATION_HEADER, generation);
    }
    response
}

pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}
