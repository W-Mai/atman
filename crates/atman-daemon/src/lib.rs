use std::sync::Arc;

use atman_proto::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, methods};
use serde_json::json;

pub mod http;
pub mod state;
pub mod unix;

pub use state::DaemonState;

pub async fn dispatch(state: Arc<DaemonState>, req: JsonRpcRequest) -> JsonRpcResponse {
    if req.jsonrpc != atman_proto::JSONRPC_VERSION {
        return JsonRpcResponse::err(
            req.id,
            JsonRpcError::invalid_params(format!(
                "expected jsonrpc {}",
                atman_proto::JSONRPC_VERSION
            )),
        );
    }

    let id = req.id.clone();
    match req.method.as_str() {
        methods::PING => JsonRpcResponse::ok(
            id,
            json!({"pong": true, "version": env!("CARGO_PKG_VERSION")}),
        ),
        methods::LIST_SESSIONS => match state.list_sessions() {
            Ok(summaries) => match serde_json::to_value(&summaries) {
                Ok(v) => JsonRpcResponse::ok(id, v),
                Err(e) => JsonRpcResponse::err(id, JsonRpcError::internal(e.to_string())),
            },
            Err(e) => JsonRpcResponse::err(id, JsonRpcError::internal(e.to_string())),
        },
        other => JsonRpcResponse::err(id, JsonRpcError::method_not_found(other)),
    }
}
