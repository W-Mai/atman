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

use atman_proto::{
    EventCursor, JsonRpcError, JsonRpcRequest, JsonRpcResponse, PROJECTION_EVENT_SCHEMA_VERSION,
    ProjectionEventEnvelope, ServerEvent, SessionId,
};

use crate::DaemonState;

pub struct HttpState {
    pub daemon: Arc<DaemonState>,
    pub auth_token: String,
}

pub fn router(state: Arc<HttpState>) -> Router {
    Router::new()
        .route("/rpc", post(rpc_handler))
        .route("/events", get(sse_handler))
        .route("/session-events", get(session_sse_handler))
        .route("/openapi.json", get(openapi_handler))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_bearer,
        ))
        .with_state(state)
}

async fn openapi_handler() -> Json<serde_json::Value> {
    use utoipa::OpenApi;
    let schema = crate::openapi::AtmanOpenApi::openapi();
    Json(serde_json::to_value(schema).unwrap_or(serde_json::json!({})))
}

async fn rpc_handler(
    State(state): State<Arc<HttpState>>,
    Extension(principal_id): Extension<String>,
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
    Json(crate::dispatch_as(state.daemon.clone(), req, &principal_id).await)
}

#[derive(Deserialize)]
pub struct SseQuery {
    pub session_id: SessionId,
    #[serde(default)]
    pub since_seq: Option<u64>,
}

#[derive(Deserialize)]
pub struct SessionSseQuery {
    pub session_id: SessionId,
    #[serde(default, alias = "since_seq")]
    pub after_cursor: Option<u64>,
}

async fn sse_handler(
    State(state): State<Arc<HttpState>>,
    Extension(principal_id): Extension<String>,
    Query(q): Query<SseQuery>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, std::io::Error>>>, (StatusCode, String)> {
    if !state.daemon.can_read_session(&q.session_id, &principal_id) {
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
    let prelude =
        futures::stream::once(async { Ok(Event::default().retry(Duration::from_millis(3000))) });
    let stream = prelude.chain(tail_events_stream(events_path, start));
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

fn last_event_id(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
}

fn tail_events_stream(
    path: std::path::PathBuf,
    start_cursor: u64,
) -> impl Stream<Item = Result<Event, std::io::Error>> {
    async_stream::stream! {
        let mut reader: Option<crate::events::EventLogReader> = None;
        loop {
            if reader.is_none() && path.exists() {
                if let Ok(opened) = crate::events::EventLogReader::open(&path).await {
                    reader = Some(opened);
                }
            }
            let Some(rd) = reader.as_mut() else {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            };
            loop {
                match rd.next().await {
                    Ok(None) => break,
                    Ok(Some(envelope)) => {
                        if envelope.cursor > EventCursor(start_cursor) {
                            let data = serde_json::to_string(&envelope).map_err(|error| {
                                std::io::Error::new(std::io::ErrorKind::InvalidData, error)
                            })?;
                            let ev = Event::default()
                                .event("event")
                                .id(envelope.cursor.0.to_string())
                                .data(data);
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

async fn session_sse_handler(
    State(state): State<Arc<HttpState>>,
    Extension(principal_id): Extension<String>,
    Query(q): Query<SessionSseQuery>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, std::io::Error>>>, (StatusCode, String)> {
    if !state.daemon.can_read_session(&q.session_id, &principal_id) {
        return Err((
            StatusCode::FORBIDDEN,
            "permission denied for session".into(),
        ));
    }
    let start = q
        .after_cursor
        .or_else(|| last_event_id(&headers))
        .unwrap_or(0);
    let subscription = state
        .daemon
        .subscribe_session_updates(&q.session_id, &principal_id)
        .await
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    let prelude =
        futures::stream::once(async { Ok(Event::default().retry(Duration::from_millis(3000))) });
    let stream = prelude.chain(session_updates_stream(
        state.daemon.clone(),
        q.session_id,
        principal_id,
        EventCursor(start),
        subscription,
    ));
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

fn session_updates_stream(
    daemon: Arc<DaemonState>,
    session_id: SessionId,
    principal_id: String,
    start_cursor: EventCursor,
    subscription: Option<(
        tokio::sync::broadcast::Receiver<ProjectionEventEnvelope>,
        Option<Arc<atman_runtime::redact::Redactor>>,
    )>,
) -> impl Stream<Item = Result<Event, std::io::Error>> {
    async_stream::try_stream! {
        let (mut receiver, redactor) = match subscription {
            Some((receiver, redactor)) => (Some(receiver), redactor),
            None => (None, None),
        };
        let mut cursor = start_cursor;
        loop {
            let page = daemon
                .session_updates(&session_id, &principal_id, cursor, None)
                .await
                .map_err(stream_io_error)?;
            if let Some(gap) = page.resync_required {
                yield projection_sse_event(&ProjectionEventEnvelope {
                    schema_version: PROJECTION_EVENT_SCHEMA_VERSION,
                    daemon_generation: page.daemon_generation,
                    session_id: session_id.clone(),
                    cursor: page.next_cursor,
                    ts: chrono::Utc::now(),
                    event: ServerEvent::ResyncRequired { gap },
                })?;
                return;
            }
            for event in page.events {
                cursor = event.cursor;
                yield projection_sse_event(&event)?;
            }
            if page.has_more {
                continue;
            }
            let Some(active_receiver) = receiver.as_mut() else {
                return;
            };
            match active_receiver.recv().await {
                Ok(event) if event.cursor <= cursor => continue,
                Ok(event) => {
                    let event = crate::projection::redacted_projection_event(
                        &event,
                        redactor.as_deref(),
                    ).map_err(stream_io_error)?;
                    let requires_resync =
                        matches!(&event.event, ServerEvent::ResyncRequired { .. });
                    cursor = event.cursor;
                    yield projection_sse_event(&event)?;
                    if requires_resync {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    }
}

fn projection_sse_event(envelope: &ProjectionEventEnvelope) -> Result<Event, std::io::Error> {
    let data = serde_json::to_string(envelope).map_err(stream_io_error)?;
    Ok(Event::default()
        .event("session_event")
        .id(envelope.cursor.0.to_string())
        .data(data))
}

fn stream_io_error(error: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(error.to_string())
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
            let principal_id = "authenticated-daemon-client".to_string();
            let mut req = req;
            req.extensions_mut().insert(principal_id);
            return next.run(req).await;
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
                    let principal_id = "authenticated-daemon-client".to_string();
                    let mut req = req;
                    req.extensions_mut().insert(principal_id);
                    return next.run(req).await;
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

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}
