use std::{path::PathBuf, sync::Arc};

use atman_proto::{
    JSONRPC_VERSION, JsonRpcError, JsonRpcRequest, JsonRpcResponse,
    approvals::{ApplyFlowApprovalRequest, FlowApprovalOperation, ReadFlowApprovalQueueRequest},
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

use super::devices::{DeviceAuthority, Permission};

static SUBMISSION_SLOTS: Semaphore = Semaphore::const_new(16);

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ForwardedDeviceRpc {
    authority: DeviceAuthority,
    pub(crate) request: JsonRpcRequest,
}

enum DeviceRequest {
    Submit(SubmitFlowRequest),
    InputStatus(InputStatusRequest),
    ApprovalQueue(ReadFlowApprovalQueueRequest),
    ApprovalApply(ApplyFlowApprovalRequest),
    ApprovalStatus(FlowApprovalOperation),
}

pub(super) fn required_permission(method: Method) -> Option<Permission> {
    match method {
        Method::FlowSubmit | Method::FlowInputStatus => Some(Permission::Send),
        Method::FlowApprovalQueue | Method::FlowApprovalApply | Method::FlowApprovalStatus => {
            Some(Permission::Approve)
        }
        _ => None,
    }
}

impl DeviceRequest {
    fn parse(request: &JsonRpcRequest, method: Method) -> Result<Self, JsonRpcError> {
        if request.jsonrpc != JSONRPC_VERSION {
            return Err(JsonRpcError::invalid_params("expected JSON-RPC 2.0"));
        }
        let params = request.params.as_ref().cloned().unwrap_or(Value::Null);
        let invalid = |_| JsonRpcError::invalid_params("invalid device request");
        match method {
            Method::FlowSubmit => {
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
            Method::FlowInputStatus => {
                let status: InputStatusRequest = serde_json::from_value(params).map_err(invalid)?;
                if status.request_id.is_nil() {
                    return Err(JsonRpcError::invalid_params(
                        "input request identity must not be nil",
                    ));
                }
                Ok(Self::InputStatus(status))
            }
            Method::FlowApprovalQueue => serde_json::from_value(params)
                .map(Self::ApprovalQueue)
                .map_err(invalid),
            Method::FlowApprovalApply => serde_json::from_value(params)
                .map(Self::ApprovalApply)
                .map_err(invalid),
            Method::FlowApprovalStatus => serde_json::from_value(params)
                .map(Self::ApprovalStatus)
                .map_err(invalid),
            _ => Err(JsonRpcError::invalid_params("expected a device method")),
        }
    }

    fn target(&self) -> &FlowAddress {
        match self {
            Self::Submit(request) => &request.flow,
            Self::InputStatus(request) => &request.flow,
            Self::ApprovalQueue(request) => &request.flow,
            Self::ApprovalApply(request) => &request.operation.flow,
            Self::ApprovalStatus(request) => &request.flow,
        }
    }
}

pub(super) async fn dispatch(
    state: Arc<DaemonState>,
    authority: DeviceAuthority,
    request: JsonRpcRequest,
) -> JsonRpcResponse {
    respond(state, ForwardedDeviceRpc { authority, request }, false).await
}

pub(crate) async fn forwarded(
    State(state): State<Arc<HttpState>>,
    headers: HeaderMap,
    Json(input): Json<ForwardedDeviceRpc>,
) -> Json<JsonRpcResponse> {
    if !headers.contains_key(host_gateway::FORWARDED_HEADER) {
        return Json(JsonRpcResponse::err(
            input.request.id,
            JsonRpcError::application("device RPC requires authenticated host forwarding"),
        ));
    }
    Json(respond(state.daemon.clone(), input, true).await)
}

async fn respond(
    state: Arc<DaemonState>,
    input: ForwardedDeviceRpc,
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
    input: ForwardedDeviceRpc,
    forwarded: bool,
) -> Result<JsonRpcResponse, JsonRpcError> {
    let method = Method::from_name(&input.request.method)
        .ok_or_else(|| JsonRpcError::method_not_found(&input.request.method))?;
    let required = required_permission(method)
        .ok_or_else(|| JsonRpcError::invalid_params("expected a device method"))?;
    let command = DeviceRequest::parse(&input.request, method)?;
    let destination = command.target();
    let session = destination.session_id.0;
    if session != input.authority.session_id || session.is_nil() || destination.instance_id.is_nil()
    {
        return Err(JsonRpcError::application(
            "target is outside the device scope",
        ));
    }
    let data = state.data_dir().to_path_buf();
    let authority = input.authority;
    check(authority, data.clone(), session, required).await?;
    if forwarded && !state.runtime_host().sessions().contains(&session) {
        return Err(JsonRpcError::application(
            "execution owner changed; refresh host discovery",
        ));
    }
    if !forwarded
        && let Some(response) = host_gateway::forward_device_rpc(&state, session, &input).await?
    {
        return Ok(response);
    }
    let result = match command {
        DeviceRequest::Submit(request) => {
            let slot = SUBMISSION_SLOTS
                .try_acquire()
                .map_err(|_| JsonRpcError::application("device submissions are busy"))?;
            let authorize = Arc::new(move |target: &atman_runtime::flow_store::FlowRef| {
                authority.check(&data, target.session_id.0, Permission::Send, Utc::now())
            });
            tokio::spawn(async move {
                let _slot = slot;
                let operator = LocalOperator::new(authority.device_id);
                flow_service::submit_device_input(state, request, operator, authorize, forwarded)
                    .await
            })
            .await
            .map_err(|_| JsonRpcError::application("device input outcome is unknown"))?
            .map_err(crate::permission_error)
        }
        DeviceRequest::InputStatus(request) => {
            let operator = LocalOperator::new(authority.device_id);
            let result = flow_service::catalog::input_status_for(&state, request, operator).await;
            check(authority, data, session, required).await?;
            result.map_err(crate::permission_error)
        }
        DeviceRequest::ApprovalQueue(request) => {
            let result = flow_service::approvals::device_snapshot(&state, request);
            check(authority, data, session, required).await?;
            result
        }
        DeviceRequest::ApprovalApply(request) => {
            flow_service::approvals::apply(state, request, authority.device_id, move || {
                authority.check(&data, session, Permission::Approve, Utc::now())
            })
            .await
        }
        DeviceRequest::ApprovalStatus(request) => {
            let result =
                flow_service::approvals::status(&state, request, authority.device_id).await;
            check(authority, data, session, required).await?;
            result
        }
    };
    result.map(|value| JsonRpcResponse::ok(input.request.id, value))
}

async fn check(
    authority: DeviceAuthority,
    data: PathBuf,
    session: uuid::Uuid,
    required: Permission,
) -> Result<(), JsonRpcError> {
    tokio::task::spawn_blocking(move || authority.check(&data, session, required, Utc::now()))
        .await
        .map_err(|_| JsonRpcError::application("device authorization is unavailable"))?
        .map_err(|_| JsonRpcError::application("device authorization is no longer valid"))
}
