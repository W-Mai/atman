use std::sync::Arc;
use std::time::Duration;

use axum::{
    Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{
        IntoResponse, Json, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{get, post},
};
use futures::Stream;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};

use atman_proto::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, SessionId};

use crate::{DaemonState, dispatch};

pub struct HttpState {
    pub daemon: Arc<DaemonState>,
    pub auth_token: String,
}

pub fn router(state: Arc<HttpState>) -> Router {
    Router::new()
        .route("/rpc", post(rpc_handler))
        .route("/events", get(sse_handler))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_bearer,
        ))
        .with_state(state)
}

async fn rpc_handler(State(state): State<Arc<HttpState>>, body: String) -> Json<JsonRpcResponse> {
    let req: JsonRpcRequest = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => {
            return Json(JsonRpcResponse::err(
                None,
                JsonRpcError::parse_error(e.to_string()),
            ));
        }
    };
    Json(dispatch(state.daemon.clone(), req).await)
}

#[derive(Deserialize)]
pub struct SseQuery {
    pub session_id: SessionId,
    #[serde(default)]
    pub since_seq: Option<u64>,
}

async fn sse_handler(
    State(state): State<Arc<HttpState>>,
    Query(q): Query<SseQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, std::io::Error>>>, (StatusCode, String)> {
    let events_path = state
        .daemon
        .sessions_dir()
        .join(q.session_id.0.to_string())
        .join("events.jsonl");
    let start = q.since_seq.unwrap_or(0);
    let stream = tail_events_stream(events_path, start);
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
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
    let Some(auth) = headers.get(axum::http::header::AUTHORIZATION) else {
        return (StatusCode::UNAUTHORIZED, "missing Authorization header").into_response();
    };
    let Ok(auth_str) = auth.to_str() else {
        return (StatusCode::UNAUTHORIZED, "invalid Authorization header").into_response();
    };
    let Some(token) = auth_str.strip_prefix("Bearer ") else {
        return (StatusCode::UNAUTHORIZED, "expected Bearer scheme").into_response();
    };
    if !constant_time_eq(token.as_bytes(), state.auth_token.as_bytes()) {
        return (StatusCode::UNAUTHORIZED, "invalid token").into_response();
    }
    next.run(req).await
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
