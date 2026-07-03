use std::sync::Arc;

use atman_proto::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Json, Response},
    routing::post,
};

use crate::{DaemonState, dispatch};

pub struct HttpState {
    pub daemon: Arc<DaemonState>,
    pub auth_token: String,
}

pub fn router(state: Arc<HttpState>) -> Router {
    Router::new()
        .route("/rpc", post(rpc_handler))
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
