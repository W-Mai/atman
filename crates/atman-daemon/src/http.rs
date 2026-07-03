use std::sync::Arc;

use atman_proto::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use axum::{Router, extract::State, response::Json, routing::post};

use crate::{DaemonState, dispatch};

pub fn router(state: Arc<DaemonState>) -> Router {
    Router::new()
        .route("/rpc", post(rpc_handler))
        .with_state(state)
}

async fn rpc_handler(State(state): State<Arc<DaemonState>>, body: String) -> Json<JsonRpcResponse> {
    let req: JsonRpcRequest = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => {
            return Json(JsonRpcResponse::err(
                None,
                JsonRpcError::parse_error(e.to_string()),
            ));
        }
    };
    Json(dispatch(state, req).await)
}
