use std::{path::PathBuf, sync::Arc};

use atman_proto::{
    JSONRPC_VERSION, JsonRpcError, JsonRpcRequest, JsonRpcResponse,
    flows::{FlowAddress, InputStatusRequest, SubmitFlowRequest},
    methods::Method,
};
use atman_runtime::flow_store::local_operator::LocalOperator;
use axum::{Json, extract::State, http::HeaderMap};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Semaphore;

use crate::{DaemonState, flow_service, host_gateway, http::HttpState};

use super::devices::InputAuthority;

static SUBMISSION_SLOTS: Semaphore = Semaphore::const_new(16);

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ForwardedDeviceInput {
    authority: InputAuthority,
    pub(crate) request: JsonRpcRequest,
}

enum InputRequest {
    Submit(SubmitFlowRequest),
    Status(InputStatusRequest),
}

impl InputRequest {
    fn parse(request: &JsonRpcRequest) -> Result<Self, JsonRpcError> {
        if request.jsonrpc != JSONRPC_VERSION {
            return Err(JsonRpcError::invalid_params("expected JSON-RPC 2.0"));
        }
        let params = request.params.as_ref().cloned().unwrap_or(Value::Null);
        let invalid = |_| JsonRpcError::invalid_params("invalid device input request");
        match Method::from_name(&request.method) {
            Some(Method::FlowSubmit) => {
                let submit: SubmitFlowRequest = serde_json::from_value(params).map_err(invalid)?;
                if submit.request_id.is_nil()
                    || submit.expected_head.is_none()
                    || !submit.invocation.is_empty()
                    || !submit.user.images.is_empty()
                    || submit.user.text.trim_start().starts_with('/')
                {
                    return Err(JsonRpcError::invalid_params(
                        "device input requires an observed head and supports text and quotes",
                    ));
                }
                Ok(Self::Submit(submit))
            }
            Some(Method::FlowInputStatus) => {
                let status: InputStatusRequest = serde_json::from_value(params).map_err(invalid)?;
                if status.request_id.is_nil() {
                    return Err(JsonRpcError::invalid_params(
                        "input request identity must not be nil",
                    ));
                }
                Ok(Self::Status(status))
            }
            _ => Err(JsonRpcError::invalid_params(
                "expected a device input method",
            )),
        }
    }

    fn target(&self) -> &FlowAddress {
        match self {
            Self::Submit(request) => &request.flow,
            Self::Status(request) => &request.flow,
        }
    }
}

pub(super) async fn dispatch(
    state: Arc<DaemonState>,
    authority: InputAuthority,
    request: JsonRpcRequest,
) -> JsonRpcResponse {
    respond(state, ForwardedDeviceInput { authority, request }, false).await
}

pub(crate) async fn forwarded(
    State(state): State<Arc<HttpState>>,
    headers: HeaderMap,
    Json(input): Json<ForwardedDeviceInput>,
) -> Json<JsonRpcResponse> {
    if !headers.contains_key(host_gateway::FORWARDED_HEADER) {
        return Json(JsonRpcResponse::err(
            input.request.id,
            JsonRpcError::application("device input requires authenticated host forwarding"),
        ));
    }
    Json(respond(state.daemon.clone(), input, true).await)
}

async fn respond(
    state: Arc<DaemonState>,
    input: ForwardedDeviceInput,
    forwarded: bool,
) -> JsonRpcResponse {
    let id = input.request.id.clone();
    match route(state, input, forwarded).await {
        Ok(response) => response,
        Err(error) => JsonRpcResponse::err(id, error),
    }
}

async fn route(
    state: Arc<DaemonState>,
    input: ForwardedDeviceInput,
    forwarded: bool,
) -> Result<JsonRpcResponse, JsonRpcError> {
    let command = InputRequest::parse(&input.request)?;
    let destination = command.target();
    let session = destination.session_id.0;
    if session != input.authority.session_id || session.is_nil() || destination.instance_id.is_nil()
    {
        return Err(JsonRpcError::application(
            "input target is outside the device scope",
        ));
    }
    let data = state.data_dir().to_path_buf();
    let authority = input.authority;
    check(authority, data.clone(), session).await?;
    if forwarded && !state.runtime_host().sessions().contains(&session) {
        return Err(JsonRpcError::application(
            "execution owner changed; refresh host discovery",
        ));
    }
    if !forwarded
        && let Some(response) = host_gateway::forward_device_input(&state, session, &input).await?
    {
        return Ok(response);
    }
    let operator = LocalOperator::new(authority.device_id);
    let result = match command {
        InputRequest::Submit(request) => {
            let slot = SUBMISSION_SLOTS
                .try_acquire()
                .map_err(|_| JsonRpcError::application("device submissions are busy"))?;
            let authorize = Arc::new(move |target: &atman_runtime::flow_store::FlowRef| {
                authority.check(&data, target.session_id.0, Utc::now())
            });
            tokio::spawn(async move {
                let _slot = slot;
                flow_service::submit_device_input(state, request, operator, authorize, forwarded)
                    .await
            })
            .await
            .map_err(|_| JsonRpcError::application("device input outcome is unknown"))?
        }
        InputRequest::Status(request) => {
            let result = flow_service::catalog::input_status_for(&state, request, operator).await;
            check(authority, data, session).await?;
            result
        }
    };
    result
        .map(|value| JsonRpcResponse::ok(input.request.id, value))
        .map_err(|error| JsonRpcError::application(error.to_string()))
}

async fn check(
    authority: InputAuthority,
    data: PathBuf,
    session: uuid::Uuid,
) -> Result<(), JsonRpcError> {
    tokio::task::spawn_blocking(move || authority.check(&data, session, Utc::now()))
        .await
        .map_err(|_| JsonRpcError::application("device authorization is unavailable"))?
        .map_err(|_| JsonRpcError::application("device input authorization is no longer valid"))
}
