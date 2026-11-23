//! Target-bound routing between authenticated local execution hosts.

use std::{io, sync::Arc, time::Duration};

use atman_proto::{
    JsonRpcError, JsonRpcRequest, JsonRpcResponse, hosts::HostDescription, methods::Method,
};
use axum::{
    body::Body,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures::StreamExt;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{DaemonState, host_directory::HostRecord};

pub const GENERATION_HEADER: &str = "x-atman-host-generation";
pub(crate) const FORWARDED_HEADER: &str = "x-atman-rpc-forwarded";

pub(crate) struct HostRouting {
    token: String,
    rpc: reqwest::Client,
    stream: reqwest::Client,
}

impl HostRouting {
    pub(crate) fn new(token: String) -> io::Result<Self> {
        if token.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "host routing credential is empty",
            ));
        }
        let client = || {
            reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(3))
        };
        Ok(Self {
            token,
            rpc: client()
                .timeout(Duration::from_secs(15))
                .build()
                .map_err(io::Error::other)?,
            stream: client().build().map_err(io::Error::other)?,
        })
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    pub(crate) fn check_token(&self, token: &str) -> io::Result<()> {
        if self.token == token {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "host routing credential is already configured",
            ))
        }
    }

    fn request(
        &self,
        record: &HostRecord,
        client: &reqwest::Client,
        path: &str,
    ) -> reqwest::RequestBuilder {
        client
            .post(format!("http://{}{path}", record.endpoint))
            .bearer_auth(&self.token)
            .header(GENERATION_HEADER, record.generation.to_string())
            .header(FORWARDED_HEADER, "1")
    }

    async fn describe(&self, record: &HostRecord) -> Result<HostDescription, JsonRpcError> {
        let request = JsonRpcRequest::new(1, atman_proto::methods::HOST_DESCRIBE, json!({}));
        let response = self
            .request(record, &self.rpc, "/rpc")
            .json(&request)
            .send()
            .await
            .map_err(|_| unavailable("host identity observation failed"))?;
        let response = decode(response, record, &request).await?;
        if let Some(error) = response.error {
            return Err(error);
        }
        let description: HostDescription = serde_json::from_value(
            response
                .result
                .ok_or_else(|| unavailable("host identity response is absent"))?,
        )
        .map_err(|_| unavailable("host identity response is invalid"))?;
        if description.generation != record.generation.to_string()
            || description.role != record.role
        {
            return Err(unavailable("host identity changed; refresh discovery"));
        }
        Ok(description)
    }
}

#[derive(Clone, Copy)]
enum Target {
    Session(Uuid),
    Profile,
}

fn parsed<T: DeserializeOwned>(
    params: &Value,
    session: impl FnOnce(T) -> Option<Uuid>,
) -> Option<Target> {
    let value = serde_json::from_value(params.clone()).ok()?;
    session(value)
        .filter(|id| !id.is_nil())
        .map(Target::Session)
}

fn target(method: Method, params: &Value) -> Option<Target> {
    use atman_proto::{flows::*, history::*};
    match method {
        Method::SessionCreate => {
            parsed::<CreateSessionRequest>(params, |value| Some(value.session_id.0))
        }
        Method::SessionOpen => parsed::<SessionRequest>(params, |value| Some(value.session_id.0)),
        Method::SessionBootstrap => {
            parsed::<BootstrapSessionRequest>(params, |value| Some(value.session_id.0))
        }
        Method::SessionFlows => {
            parsed::<ListFlowsRequest>(params, |value| Some(value.session_id.0))
        }
        Method::SessionSelectFlow | Method::FlowGet | Method::FlowBind | Method::FlowOverview => {
            parsed::<FlowRequest>(params, |value| Some(value.flow.session_id.0))
        }
        Method::FlowExecution => {
            parsed::<atman_proto::execution::FlowExecutionRequest>(params, |value| {
                Some(value.flow.session_id.0)
            })
        }
        Method::FlowComposer => {
            parsed::<atman_proto::composer::FlowComposerRequest>(params, |value| {
                Some(value.flow.session_id.0)
            })
        }
        Method::FlowComposerPrepare => {
            parsed::<FlowRequest>(params, |value| Some(value.flow.session_id.0))
        }
        Method::FlowComposerPreferenceSet => {
            parsed::<atman_proto::composer::SetComposerPreferenceRequest>(params, |value| {
                Some(value.flow.session_id.0)
            })
        }
        Method::FlowComposerPreferenceStatus => {
            parsed::<atman_proto::composer::ComposerPreferenceStatusRequest>(params, |value| {
                Some(value.flow.session_id.0)
            })
        }
        Method::FlowControl => {
            parsed::<ControlFlowRequest>(params, |value| Some(value.flow.session_id.0))
        }
        Method::FlowLaunch => {
            parsed::<LaunchFlowRequest>(params, |value| Some(value.flow.session_id.0))
        }
        Method::FlowInvoke => {
            parsed::<InvokeFlowRequest>(params, |value| Some(value.flow.session_id.0))
        }
        Method::FlowSubmit => {
            parsed::<SubmitFlowRequest>(params, |value| Some(value.flow.session_id.0))
        }
        Method::QuestionStart => {
            parsed::<atman_proto::questions::StartQuestionRequest>(params, |value| {
                Some(value.identity.source.session_id.0)
            })
        }
        Method::QuestionStatus => {
            parsed::<atman_proto::questions::QuestionStatusRequest>(params, |value| {
                Some(value.identity.source.session_id.0)
            })
        }
        Method::QuestionTranscript => {
            parsed::<atman_proto::questions::ReadQuestionTranscriptRequest>(params, |value| {
                Some(value.identity.source.session_id.0)
            })
        }
        Method::QuestionFollowup => {
            parsed::<atman_proto::questions::QuestionFollowupRequest>(params, |value| {
                Some(value.question.identity.source.session_id.0)
            })
        }
        Method::QuestionFollowupStatus => {
            parsed::<atman_proto::questions::QuestionFollowupStatusRequest>(params, |value| {
                Some(value.question.identity.source.session_id.0)
            })
        }
        Method::QuestionExecution => {
            parsed::<atman_proto::questions::QuestionExecutionRequest>(params, |value| {
                Some(value.question.identity.source.session_id.0)
            })
        }
        Method::QuestionStop => {
            parsed::<atman_proto::questions::QuestionStopRequest>(params, |value| {
                Some(value.question.identity.source.session_id.0)
            })
        }
        Method::QuestionDiscard => {
            parsed::<atman_proto::questions::QuestionDiscardRequest>(params, |value| {
                Some(value.question.identity.source.session_id.0)
            })
        }
        Method::QuestionDiscardStatus => {
            parsed::<atman_proto::questions::QuestionDiscardStatusRequest>(params, |value| {
                Some(value.question.identity.source.session_id.0)
            })
        }
        Method::QuestionRefresh => {
            parsed::<atman_proto::questions::QuestionRefreshRequest>(params, |value| {
                Some(value.question.identity.source.session_id.0)
            })
        }
        Method::QuestionRefreshStatus => {
            parsed::<atman_proto::questions::QuestionRefreshStatusRequest>(params, |value| {
                Some(value.question.identity.source.session_id.0)
            })
        }
        Method::QuestionKeep => {
            parsed::<atman_proto::questions::QuestionKeepRequest>(params, |value| {
                Some(value.question.identity.source.session_id.0)
            })
        }
        Method::QuestionKeepStatus => {
            parsed::<atman_proto::questions::QuestionKeepStatusRequest>(params, |value| {
                Some(value.question.identity.source.session_id.0)
            })
        }
        Method::QuestionKeepInstalled => {
            parsed::<atman_proto::questions::QuestionKeepInstalledRequest>(params, |value| {
                Some(value.question.identity.source.session_id.0)
            })
        }
        Method::QuestionModelCheck => {
            parsed::<atman_proto::questions::QuestionModelCheckRequest>(params, |value| {
                Some(value.question.identity.source.session_id.0)
            })
        }
        Method::QuestionModelOptions => {
            parsed::<atman_proto::questions::QuestionModelOptionsRequest>(params, |value| {
                Some(value.flow.session_id.0)
            })
        }
        Method::FlowInbox => parsed::<atman_proto::inbox::ReadFlowInboxRequest>(params, |value| {
            Some(value.flow.session_id.0)
        }),
        Method::FlowInboxEdit => {
            parsed::<atman_proto::inbox::EditFlowInboxRequest>(params, |value| {
                Some(value.flow.session_id.0)
            })
        }
        Method::FlowInputStatus => {
            parsed::<InputStatusRequest>(params, |value| Some(value.flow.session_id.0))
        }
        Method::FlowConversation => {
            parsed::<atman_proto::conversation::ReadConversationRequest>(params, |value| {
                Some(value.flow.session_id.0)
            })
        }
        Method::FlowApprovalQueue => {
            parsed::<atman_proto::approvals::ReadFlowApprovalQueueRequest>(params, |value| {
                Some(value.flow.session_id.0)
            })
        }
        Method::FlowDrafts => parsed::<atman_proto::live::ReadFlowDraftsRequest>(params, |value| {
            Some(value.flow.session_id.0)
        }),
        Method::FlowHistoryRecoverInput => {
            parsed::<HistoryRecoverInputRequest>(params, |value| Some(value.flow.session_id.0))
        }
        Method::FlowHistoryBranches => {
            parsed::<HistoryBranchesRequest>(params, |value| Some(value.flow.session_id.0))
        }
        Method::FlowHistoryReview => {
            parsed::<HistoryReviewRequest>(params, |value| Some(value.flow.session_id.0))
        }
        Method::FlowHistoryBegin => {
            parsed::<HistoryOperationRequest>(params, |value| Some(value.flow.session_id.0))
        }
        Method::FlowHistoryStop
        | Method::FlowHistoryApply
        | Method::FlowHistoryAbort
        | Method::FlowHistoryStatus => {
            parsed::<HistoryOperationQuery>(params, |value| Some(value.flow.session_id.0))
        }
        Method::FlowOutput => {
            parsed::<ReadFlowOutputRequest>(params, |value| Some(value.flow.session_id.0))
        }
        Method::FlowOutputDetail => {
            parsed::<ReadFlowOutputDetailRequest>(params, |value| Some(value.flow.session_id.0))
        }
        Method::FlowEvents => {
            parsed::<ReadFlowEventsRequest>(params, |value| Some(value.flow.session_id.0))
        }
        Method::GoalGet
        | Method::GoalSet
        | Method::GoalPause
        | Method::GoalResume
        | Method::GoalClear => {
            parsed::<atman_proto::GoalRequest>(params, |value| Some(value.session_id.0))
        }
        Method::ListPermissionRequests => {
            parsed::<atman_proto::ListPermissionRequestsRequest>(params, |value| {
                Some(value.target.session_id.0)
            })
        }
        Method::CreatePermissionGroup => {
            parsed::<atman_proto::CreatePermissionGroupRequest>(params, |value| {
                Some(value.target.session_id.0)
            })
        }
        Method::ResolvePermissionRequests => {
            parsed::<atman_proto::ResolvePermissionRequestsRequest>(params, |value| {
                Some(value.target.session_id.0)
            })
        }
        Method::ResolvePrompt => parsed::<atman_proto::ResolvePromptRequest>(params, |value| {
            value.session_id.map(|id| id.0)
        }),
        Method::ManagementGet | Method::ManagementOpen => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Empty {}
            serde_json::from_value::<Empty>(params.clone())
                .ok()
                .map(|_| Target::Profile)
        }
        Method::ProjectSetPinned => crate::project_directory::parse_pin(params.clone())
            .ok()
            .map(|_| Target::Profile),
        Method::ManagementSelect => {
            serde_json::from_value::<SelectManagementRequest>(params.clone())
                .ok()
                .map(|_| Target::Profile)
        }
        Method::FlowControlBatch => {
            serde_json::from_value::<ControlFlowBatchRequest>(params.clone())
                .ok()
                .map(|_| Target::Profile)
        }
        Method::FlowControlStatus => {
            serde_json::from_value::<ControlFlowStatusRequest>(params.clone())
                .ok()
                .map(|_| Target::Profile)
        }
        _ => None,
    }
}

fn local(state: &DaemonState, target: Target) -> bool {
    if state.session_service.get().is_none() {
        return false;
    }
    let host = state.runtime_host();
    match target {
        Target::Session(id) => host.sessions().contains(&id),
        Target::Profile => host.profile_handle().is_some(),
    }
}

fn unavailable(message: &str) -> JsonRpcError {
    JsonRpcError::application(message.to_owned())
}

async fn destination(
    state: &DaemonState,
    target: Target,
    prefer_creator: bool,
) -> Result<Option<HostRecord>, JsonRpcError> {
    if local(state, target) {
        return Ok(None);
    }
    let Some(routing) = state.host_routing.get() else {
        return Ok(None);
    };
    let data = state.data_dir().to_owned();
    let records = tokio::task::spawn_blocking(move || crate::host_directory::discover(&data))
        .await
        .map_err(|_| unavailable("host discovery worker failed"))?
        .map_err(|_| unavailable("host discovery failed"))?;
    if records.len() > 64 {
        return Err(unavailable("host discovery exceeds its connection budget"));
    }
    let mut selected = None;
    let mut failed = None;
    let mut creators = Vec::new();
    for record in records {
        if record.generation.to_string() == state.daemon_generation() {
            continue;
        }
        if let Target::Session(id) = target
            && !prefer_creator
            && !record.sessions.contains(&id)
        {
            continue;
        }
        let description = match routing.describe(&record).await {
            Ok(description) => description,
            Err(error) => {
                if !prefer_creator
                    || record.role == atman_proto::hosts::HostRole::Tui
                    || matches!(target, Target::Session(id) if record.sessions.contains(&id))
                {
                    failed = Some(error);
                }
                continue;
            }
        };
        let owns = match target {
            Target::Session(id) => description.sessions.iter().any(|session| session.0 == id),
            Target::Profile => description.profile_owner,
        };
        if !owns {
            if matches!(target, Target::Session(id) if record.sessions.contains(&id)) {
                failed = Some(unavailable("session owner changed; refresh host discovery"));
            }
            if prefer_creator
                && description.role == atman_proto::hosts::HostRole::Tui
                && description.presentation == atman_proto::hosts::HostPresentation::Attached
            {
                creators.push(record);
            }
            continue;
        }
        if selected.is_some() {
            return Err(unavailable(
                "multiple hosts claim the same execution authority",
            ));
        }
        selected = Some(record);
    }
    if selected.is_none()
        && let Some(error) = failed
    {
        return Err(error);
    }
    if selected.is_none() && prefer_creator {
        selected = creators.into_iter().max_by_key(|record| record.generation);
    }
    Ok(selected)
}

/// Route only authenticated, typed targets. A failed write is never sent a second time.
pub(crate) async fn rpc(
    state: &Arc<DaemonState>,
    request: &JsonRpcRequest,
    method: Method,
    principal: &str,
    forwarded: bool,
) -> Option<JsonRpcResponse> {
    if principal != crate::state::LOCAL_OPERATOR {
        return None;
    }
    let target = target(method, request.params.as_ref().unwrap_or(&json!({})))?;
    if local(state, target) {
        return None;
    }
    if forwarded
        && method == Method::SessionCreate
        && state.host_role() == atman_proto::hosts::HostRole::Tui
    {
        return None;
    }
    let routed = if forwarded {
        Err(unavailable(
            "execution owner changed; refresh host discovery",
        ))
    } else {
        destination(
            state,
            target,
            method == Method::SessionCreate
                && state.host_role() == atman_proto::hosts::HostRole::Daemon,
        )
        .await
    };
    let record = match routed {
        Ok(Some(record)) => record,
        Ok(None) => return None,
        Err(error) => return Some(JsonRpcResponse::err(request.id.clone(), error)),
    };
    let routing = state.host_routing.get().expect("routing was configured");
    let response = match routing
        .request(&record, &routing.rpc, "/rpc")
        .json(request)
        .send()
        .await
    {
        Ok(response) => decode(response, &record, request).await,
        Err(_) => Err(unavailable(
            "forwarded request outcome is unknown; query or retry its original identity",
        )),
    };
    Some(response.unwrap_or_else(|error| JsonRpcResponse::err(request.id.clone(), error)))
}

async fn decode(
    response: reqwest::Response,
    record: &HostRecord,
    request: &JsonRpcRequest,
) -> Result<JsonRpcResponse, JsonRpcError> {
    if !response.status().is_success() {
        return Err(unavailable(
            "host RPC failed; the original request outcome may be unknown",
        ));
    }
    if response
        .headers()
        .get(GENERATION_HEADER)
        .and_then(|value| value.to_str().ok())
        != Some(record.generation.to_string().as_str())
    {
        return Err(unavailable(
            "host response identity changed; refresh discovery",
        ));
    }
    let value: Value = response.json().await.map_err(|_| {
        unavailable("host RPC response is invalid; the original outcome may be unknown")
    })?;
    if value.get("jsonrpc") != Some(&json!(atman_proto::JSONRPC_VERSION))
        || value.get("id").unwrap_or(&Value::Null) != request.id.as_ref().unwrap_or(&Value::Null)
        || value.get("result").is_some() == value.get("error").is_some()
    {
        return Err(unavailable(
            "host RPC response identity or envelope is invalid",
        ));
    }
    let error = value
        .get("error")
        .map(|error| serde_json::from_value(error.clone()))
        .transpose()
        .map_err(|_| unavailable("host RPC error is invalid"))?;
    Ok(JsonRpcResponse {
        jsonrpc: atman_proto::JSONRPC_VERSION.into(),
        id: request.id.clone(),
        result: value.get("result").cloned(),
        error,
    })
}

pub(crate) async fn events(
    state: &Arc<DaemonState>,
    query: &crate::http::SseQuery,
    headers: &HeaderMap,
    shutdown: tokio_util::sync::CancellationToken,
) -> Result<Option<Response>, (StatusCode, String)> {
    let target = Target::Session(query.session_id.0);
    if local(state, target) {
        return Ok(None);
    }
    if headers.contains_key(FORWARDED_HEADER) {
        return Err((
            StatusCode::CONFLICT,
            "execution owner changed; refresh host discovery".into(),
        ));
    }
    let record = destination(state, target, false)
        .await
        .map_err(|error| (StatusCode::BAD_GATEWAY, error.message))?;
    let Some(record) = record else {
        return Ok(None);
    };
    let routing = state.host_routing.get().expect("routing was configured");
    let mut request = routing
        .stream
        .get(format!("http://{}/events", record.endpoint))
        .bearer_auth(&routing.token)
        .header(GENERATION_HEADER, record.generation.to_string())
        .header(FORWARDED_HEADER, "1")
        .query(query);
    if let Some(last) = headers.get("last-event-id") {
        request = request.header("Last-Event-ID", last);
    }
    let response = tokio::time::timeout(Duration::from_secs(15), request.send())
        .await
        .map_err(|_| {
            (
                StatusCode::GATEWAY_TIMEOUT,
                "event source connection timed out".into(),
            )
        })?
        .map_err(|_| {
            (
                StatusCode::BAD_GATEWAY,
                "event source connection failed".into(),
            )
        })?;
    if !response.status().is_success() {
        return Err((
            response.status(),
            "event source rejected the original subscription".into(),
        ));
    }
    if response
        .headers()
        .get(GENERATION_HEADER)
        .and_then(|value| value.to_str().ok())
        != Some(record.generation.to_string().as_str())
    {
        return Err((
            StatusCode::CONFLICT,
            "event source generation changed".into(),
        ));
    }
    if !response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"))
    {
        return Err((
            StatusCode::BAD_GATEWAY,
            "event source response is invalid".into(),
        ));
    }
    let body = Body::from_stream(
        response
            .bytes_stream()
            .take_until(shutdown.cancelled_owned()),
    );
    let mut response = body.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/event-stream"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-cache"),
    );
    Ok(Some(response))
}
