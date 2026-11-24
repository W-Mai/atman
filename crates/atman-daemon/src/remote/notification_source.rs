//! Read notification candidates from the current Session owner over the host HTTP transport.

use std::{io, num::NonZeroUsize, sync::Arc};

use atman_proto::{
    JSONRPC_VERSION, JsonRpcError, JsonRpcRequest, JsonRpcResponse, flows::ReadFlowEventsRequest,
};
use axum::{Json, extract::State, http::HeaderMap};

use crate::{DaemonState, flow_service, host_gateway, http::HttpState};

pub(crate) const PATH: &str = "/rpc/notification-events";
const METHOD: &str = "flow.notification_events";

pub(crate) async fn read(
    State(state): State<Arc<HttpState>>,
    headers: HeaderMap,
    Json(request): Json<JsonRpcRequest>,
) -> Json<JsonRpcResponse> {
    let result = route(
        &state.daemon,
        &request,
        headers.contains_key(host_gateway::FORWARDED_HEADER),
    )
    .await;
    Json(match result {
        Ok(response) => response,
        Err(error) => JsonRpcResponse::err(request.id, error),
    })
}

async fn route(
    state: &DaemonState,
    request: &JsonRpcRequest,
    forwarded: bool,
) -> Result<JsonRpcResponse, JsonRpcError> {
    if state.is_stopping() {
        return Err(JsonRpcError::application("notification source is stopping"));
    }
    let invalid = || JsonRpcError::invalid_params("invalid notification events request");
    if request.jsonrpc != JSONRPC_VERSION || request.method != METHOD {
        return Err(invalid());
    }
    let target =
        serde_json::from_value::<ReadFlowEventsRequest>(request.params.clone().unwrap_or_default())
            .map_err(|_| invalid())?;
    let limit = NonZeroUsize::new(target.limit)
        .filter(|limit| limit.get() <= 32)
        .ok_or_else(invalid)?;
    let flow = flow_service::core_address(target.flow);
    if flow.session_id.0.is_nil()
        || flow.instance_id.0.0.is_nil()
        || target
            .after
            .as_ref()
            .is_some_and(|cursor| cursor.journal_id != flow.instance_id.0.0)
    {
        return Err(invalid());
    }
    if !forwarded
        && let Some(response) =
            host_gateway::notification_events_page(state, flow.session_id.0, request).await?
    {
        return Ok(response);
    }

    let session = match state.runtime_host().session(flow.session_id.0) {
        Ok(session) => Some(session),
        Err(error) if !forwarded && error.kind() == io::ErrorKind::NotFound => None,
        Err(_) => {
            return Err(JsonRpcError::application(
                "notification source owner changed",
            ));
        }
    };
    let value =
        if let Some(worker) = session.and_then(|session| session.loaded_flow(&flow.instance_id)) {
            let after = target
                .after
                .map(|position| flow_service::events::receipt(position).into());
            let page = worker
                .read_notification_events(after, limit)
                .await
                .map_err(|error| {
                    if error.kind() == io::ErrorKind::InvalidInput {
                        JsonRpcError::invalid_params("receipt is outside the notification source")
                    } else {
                        JsonRpcError::application("notification events are unavailable")
                    }
                })?;
            serde_json::to_value(page)
                .map_err(|_| JsonRpcError::internal("encode notification events"))?
        } else {
            // A source without a live worker has no writer acknowledgement to share.
            serde_json::Value::Null
        };
    Ok(JsonRpcResponse::ok(request.id.clone(), value))
}
