use atman_rt::Value as CoreValue;
use atman_runtime::ValueJson;
use std::sync::Arc;

type Value = CoreValue<atman_runtime::AtmanPayload, atman_runtime::RuntimeError>;

use atman_proto::{
    CancelRunRequest, CreatePermissionGroupRequest, JsonRpcError, JsonRpcRequest, JsonRpcResponse,
    ListPermissionRequestsRequest, PermissionRpcAction, PermissionRpcScope, PermissionRpcSelector,
    ResolvePermissionRequestsRequest, ResolvePromptRequest, RunFlowRequest, RunFlowResponse,
    methods::{self, Method},
};
use serde_json::json;
use std::collections::{BTreeSet, HashMap};

fn permission_action(action: PermissionRpcAction) -> atman_runtime::permission::PermissionAction {
    match action {
        PermissionRpcAction::Approve => atman_runtime::permission::PermissionAction::Approve,
        PermissionRpcAction::Deny => atman_runtime::permission::PermissionAction::Deny,
        PermissionRpcAction::Defer => atman_runtime::permission::PermissionAction::Defer,
    }
}

fn permission_scope(
    scope: Option<PermissionRpcScope>,
) -> Option<atman_runtime::permission::GrantScope> {
    scope.map(|scope| match scope {
        PermissionRpcScope::CurrentCall => atman_runtime::permission::GrantScope::CurrentCall,
        PermissionRpcScope::ChildRunSameTool { run_id, tool_name } => {
            atman_runtime::permission::GrantScope::ChildRunSameTool {
                run_id: run_id.into_core(),
                tool_name,
            }
        }
        PermissionRpcScope::ChildRunSamePathRule {
            run_id,
            tool_name,
            workspace_relative_path,
        } => atman_runtime::permission::GrantScope::ChildRunSamePathRule {
            run_id: run_id.into_core(),
            tool_name,
            workspace_relative_path,
        },
    })
}

fn approval_decision_view(
    outcome: &atman_runtime::permission::BatchRequestOutcome,
) -> Option<atman_proto::approvals::ApprovalDecisionView> {
    use atman_proto::approvals::{ApprovalDecisionAction, ApprovalDecisionView};
    use atman_runtime::permission::BatchRequestOutcome;

    let (record, action) = match outcome {
        BatchRequestOutcome::Approved(record) => (record, ApprovalDecisionAction::Approved),
        BatchRequestOutcome::Denied(record) => (record, ApprovalDecisionAction::Denied),
        BatchRequestOutcome::Deferred(record) => (record, ApprovalDecisionAction::Cancelled),
        _ => return None,
    };
    Some(ApprovalDecisionView {
        decision_id: record.decision_id.0,
        action,
    })
}

fn permission_error(error: impl std::fmt::Display) -> JsonRpcError {
    JsonRpcError::application(error.to_string())
}

async fn authorized_permission_session(
    state: &DaemonState,
    target: &atman_proto::PermissionTarget,
    principal_id: &str,
) -> Result<Arc<atman_runtime::Session>, JsonRpcError> {
    if let Some(flow) =
        flow_service::resolve_target(state, &target.session_id, target.instance_id, principal_id)
            .await?
    {
        return flow_service::context(state, flow, principal_id).await;
    }
    state
        .authorized_live_session(&target.session_id, principal_id)
        .ok_or_else(|| JsonRpcError::application("permission denied for session"))
}

pub mod bootstrap;
pub mod config;
pub mod embedded;
pub mod host_directory;
pub mod host_gateway;
pub mod host_maintenance;
mod host_service;
pub mod http;
pub mod openapi;
mod peer_handoff_store;
pub mod peer_transport;
pub mod pidfile;
pub mod preview_server;
pub mod prompt_bridge;
pub mod run;
pub mod state;
#[cfg(unix)]
pub mod unix;
mod web;

pub use embedded::{EmbeddedRpcServer, SharedHostServices, SharedPrompts};
pub use state::{DaemonState, LiveSession};

mod capabilities;
mod flow_service;
mod project_directory;
mod workspace_directory;

pub async fn dispatch(state: Arc<DaemonState>, req: JsonRpcRequest) -> JsonRpcResponse {
    dispatch_as(state, req, crate::state::LOCAL_OPERATOR).await
}

pub async fn dispatch_as(
    state: Arc<DaemonState>,
    req: JsonRpcRequest,
    principal_id: &str,
) -> JsonRpcResponse {
    dispatch_with_routing(state, req, principal_id, false).await
}

pub(crate) async fn dispatch_forwarded_as(
    state: Arc<DaemonState>,
    req: JsonRpcRequest,
    principal_id: &str,
) -> JsonRpcResponse {
    dispatch_with_routing(state, req, principal_id, true).await
}

async fn dispatch_with_routing(
    state: Arc<DaemonState>,
    req: JsonRpcRequest,
    principal_id: &str,
    forwarded: bool,
) -> JsonRpcResponse {
    if state.is_stopping() {
        return JsonRpcResponse::err(req.id, JsonRpcError::application("daemon is stopping"));
    }
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
    let Some(method) = Method::from_name(&req.method) else {
        return JsonRpcResponse::err(id, JsonRpcError::method_not_found(&req.method));
    };
    if let Some(response) = host_gateway::rpc(&state, &req, method, principal_id, forwarded).await {
        return response;
    }
    match method {
        Method::HostDescribe | Method::HostHandoff => {
            match host_service::dispatch(
                &state,
                method,
                req.params.unwrap_or(json!({})),
                principal_id,
            )
            .await
            {
                Ok(result) => JsonRpcResponse::ok(id, result),
                Err(error) => JsonRpcResponse::err(id, error),
            }
        }
        Method::ManagementGet
        | Method::ManagementOpen
        | Method::ManagementSelect
        | Method::SessionCreate
        | Method::SessionOpen
        | Method::SessionBootstrap
        | Method::SessionFlows
        | Method::SessionSelectFlow
        | Method::FlowGet
        | Method::FlowExecution
        | Method::FlowOverview
        | Method::FlowComposer
        | Method::FlowComposerPrepare
        | Method::FlowComposerPreferenceSet
        | Method::FlowComposerPreferenceStatus
        | Method::FlowControl
        | Method::FlowControlBatch
        | Method::FlowControlStatus
        | Method::FlowLaunch
        | Method::FlowBind
        | Method::FlowInvoke
        | Method::FlowSubmit
        | Method::QuestionStart
        | Method::QuestionStatus
        | Method::QuestionTranscript
        | Method::QuestionObserve
        | Method::QuestionFollowup
        | Method::QuestionFollowupStatus
        | Method::QuestionExecution
        | Method::QuestionStop
        | Method::QuestionDiscard
        | Method::QuestionDiscardStatus
        | Method::QuestionRefresh
        | Method::QuestionRefreshStatus
        | Method::QuestionKeep
        | Method::QuestionKeepStatus
        | Method::QuestionKeepInstalled
        | Method::QuestionModelCheck
        | Method::QuestionModelOptions
        | Method::FlowInbox
        | Method::FlowInboxEdit
        | Method::FlowInputStatus
        | Method::FlowConversation
        | Method::FlowConversationArtifact
        | Method::FlowPreview
        | Method::FlowDrafts
        | Method::FlowApprovalQueue
        | Method::FlowHistoryBranches
        | Method::FlowHistoryReview
        | Method::FlowHistoryBegin
        | Method::FlowHistoryStop
        | Method::FlowHistoryApply
        | Method::FlowHistoryAbort
        | Method::FlowHistoryStatus
        | Method::FlowHistoryRecoverInput
        | Method::FlowOutput
        | Method::FlowOutputDetail
        | Method::FlowEvents => {
            match flow_service::dispatch(
                state,
                method,
                req.params.unwrap_or(json!({})),
                principal_id,
            )
            .await
            {
                Ok(result) => JsonRpcResponse::ok(id, result),
                Err(error) => JsonRpcResponse::err(id, error),
            }
        }
        Method::Capabilities => {
            match capabilities::query(&state, req.params.unwrap_or(json!({})), principal_id) {
                Ok(result) => typed_response(id, result),
                Err(error) => JsonRpcResponse::err(id, error),
            }
        }
        Method::Ping => typed_response(
            id,
            atman_proto::responses::PingResponse {
                pong: true,
                version: env!("CARGO_PKG_VERSION").into(),
            },
        ),
        Method::DaemonShutdown => {
            if principal_id != crate::state::LOCAL_OPERATOR {
                JsonRpcResponse::err(
                    id,
                    JsonRpcError::application("permission denied for daemon shutdown"),
                )
            } else {
                state.request_shutdown();
                JsonRpcResponse::ok(id, json!({ "stopping": true }))
            }
        }
        Method::ProjectList => {
            match project_directory::list(&state, req.params.unwrap_or(json!({})), principal_id)
                .await
            {
                Ok(page) => typed_response(id, page),
                Err(error) => JsonRpcResponse::err(id, error),
            }
        }
        Method::ProjectDirectory => {
            match project_directory::read_directory(
                &state,
                req.params.unwrap_or(json!({})),
                principal_id,
            )
            .await
            {
                Ok(page) => typed_response(id, page),
                Err(error) => JsonRpcResponse::err(id, error),
            }
        }
        Method::ProjectSetPinned => {
            match project_directory::set_pinned(
                &state,
                req.params.unwrap_or(json!({})),
                principal_id,
            )
            .await
            {
                Ok(receipt) => typed_response(id, receipt),
                Err(error) => JsonRpcResponse::err(id, error),
            }
        }
        Method::ProjectWorkspaceRoots => {
            match workspace_directory::roots(&state, req.params.unwrap_or(json!({})), principal_id)
                .await
            {
                Ok(roots) => typed_response(id, roots),
                Err(error) => JsonRpcResponse::err(id, error),
            }
        }
        Method::ProjectWorkspaceFolders => {
            match workspace_directory::folders(
                &state,
                req.params.unwrap_or(json!({})),
                principal_id,
            )
            .await
            {
                Ok(page) => typed_response(id, page),
                Err(error) => JsonRpcResponse::err(id, error),
            }
        }
        Method::ProjectOpenWorkspace => {
            match workspace_directory::open(state, req.params.unwrap_or(json!({})), principal_id)
                .await
            {
                Ok(receipt) => typed_response(id, receipt),
                Err(error) => JsonRpcResponse::err(id, error),
            }
        }
        Method::ProjectWorkspaceOpenStatus => {
            match workspace_directory::status(&state, req.params.unwrap_or(json!({})), principal_id)
                .await
            {
                Ok(receipt) => typed_response(id, receipt),
                Err(error) => JsonRpcResponse::err(id, error),
            }
        }
        Method::SettingsGitCoAuthor | Method::SettingsGitCoAuthorSet => {
            match git_co_author_setting(
                &state,
                method,
                req.params.unwrap_or(json!({})),
                principal_id,
            )
            .await
            {
                Ok(setting) => typed_response(id, setting),
                Err(error) => JsonRpcResponse::err(id, error),
            }
        }
        Method::ListSessions => {
            let params = req.params.unwrap_or(json!({}));
            let project_root = params.get("project_root").and_then(|value| value.as_str());
            let search = params.get("search").and_then(|value| value.as_str());
            let limit = params
                .get("limit")
                .and_then(|value| value.as_u64())
                .map(|value| value as usize);
            if principal_id != crate::state::LOCAL_OPERATOR {
                return JsonRpcResponse::err(
                    id,
                    JsonRpcError::application("permission denied for session discovery"),
                );
            }
            match state.list_sessions_query(project_root, search, limit).await {
                Ok(summaries) => match serde_json::to_value(
                    atman_proto::responses::ListSessionsResponse(summaries),
                ) {
                    Ok(v) => JsonRpcResponse::ok(id, v),
                    Err(e) => JsonRpcResponse::err(id, JsonRpcError::internal(e.to_string())),
                },
                Err(e) => JsonRpcResponse::err(id, JsonRpcError::internal(e.to_string())),
            }
        }
        Method::RenameSession => {
            let params = req.params.unwrap_or(json!({}));
            let sid = params
                .get("session_id")
                .and_then(|v| v.as_str())
                .and_then(|s| uuid::Uuid::parse_str(s).ok());
            let title = params
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            match sid {
                Some(uuid) if !title.trim().is_empty() => match state
                    .rename_session(&atman_proto::SessionId(uuid), title)
                {
                    Ok(summary) => {
                        JsonRpcResponse::ok(id, serde_json::to_value(summary).unwrap_or(json!({})))
                    }
                    Err(e) => JsonRpcResponse::err(id, JsonRpcError::application(e.to_string())),
                },
                _ => JsonRpcResponse::err(
                    id,
                    JsonRpcError::invalid_params("session_id and non-empty title are required"),
                ),
            }
        }
        Method::CancelRun => {
            let params = req.params.unwrap_or(json!({}));
            let parsed: Result<CancelRunRequest, _> = serde_json::from_value(params);
            match parsed {
                Ok(p) => match state.cancel_run_as(&p.run_id, principal_id).await {
                    Ok(cancelled) => {
                        typed_response(id, atman_proto::responses::CancelRunResponse { cancelled })
                    }
                    Err(error) => {
                        JsonRpcResponse::err(id, JsonRpcError::application(error.to_string()))
                    }
                },
                Err(e) => JsonRpcResponse::err(id, JsonRpcError::invalid_params(e.to_string())),
            }
        }
        Method::ResolvePrompt => {
            if principal_id != crate::state::LOCAL_OPERATOR {
                return JsonRpcResponse::err(
                    id,
                    JsonRpcError::application("permission denied for prompt resolution"),
                );
            }
            let params = req.params.unwrap_or(json!({}));
            let parsed: Result<ResolvePromptRequest, _> = serde_json::from_value(params);
            match parsed {
                Ok(p) => {
                    let resolved = state
                        .resolve_prompt_for_session(&p.prompt_id, p.answer, p.session_id.as_ref())
                        .await;
                    match resolved {
                        Ok(resolved) => typed_response(
                            id,
                            atman_proto::responses::ResolvePromptResponse { resolved },
                        ),
                        Err(error) => {
                            JsonRpcResponse::err(id, JsonRpcError::internal(error.to_string()))
                        }
                    }
                }
                Err(e) => JsonRpcResponse::err(id, JsonRpcError::invalid_params(e.to_string())),
            }
        }
        Method::ListPermissionRequests => {
            let parsed: Result<ListPermissionRequestsRequest, _> =
                serde_json::from_value(req.params.unwrap_or(json!({})));
            match parsed {
                Ok(p) => match authorized_permission_session(&state, &p.target, principal_id).await
                {
                    Ok(session) => {
                        let (requests, groups) = session
                            .permission_broker()
                            .user_list(&session.id().to_string());
                        let response = atman_proto::ListPermissionRequestsResponse {
                            requests: requests
                                .into_iter()
                                .map(|request| atman_proto::PermissionRequestView {
                                    request_id: request.request_id.0,
                                    session_id: request.session_id,
                                    instance_id: session
                                        .flow_instance()
                                        .map(|address| address.instance_id.0.0),
                                    requesting_run_id: request.requesting_run_id.into(),
                                    tool: request.intent.tool_name,
                                    tier: format!("{:?}", request.intent.tier),
                                    state: format!("{:?}", request.state),
                                    target: request
                                        .escalation_path
                                        .last()
                                        .map(|hop| format!("{:?}", hop.target))
                                        .unwrap_or_default(),
                                    revision: request.revision,
                                })
                                .collect(),
                            groups: groups
                                .into_iter()
                                .map(|group| atman_proto::PermissionGroupView {
                                    group_id: group.group_id.0,
                                    label: group.label,
                                    request_ids: group
                                        .request_ids
                                        .into_iter()
                                        .map(|id| id.0)
                                        .collect(),
                                    revision: group.revision,
                                })
                                .collect(),
                        };
                        JsonRpcResponse::ok(id, serde_json::to_value(response).unwrap_or(json!({})))
                    }
                    Err(error) => JsonRpcResponse::err(id, error),
                },
                Err(e) => JsonRpcResponse::err(id, JsonRpcError::invalid_params(e.to_string())),
            }
        }
        Method::CreatePermissionGroup => {
            let parsed: Result<CreatePermissionGroupRequest, _> =
                serde_json::from_value(req.params.unwrap_or(json!({})));
            match parsed {
                Ok(p) => match authorized_permission_session(&state, &p.target, principal_id).await
                {
                    Ok(session) => {
                        let ids: BTreeSet<_> = p
                            .request_ids
                            .iter()
                            .copied()
                            .map(atman_runtime::permission::PermissionRequestId)
                            .collect();
                        let revisions: HashMap<_, _> = p
                            .expected_request_revisions
                            .into_iter()
                            .map(|(id, revision)| {
                                (atman_runtime::permission::PermissionRequestId(id), revision)
                            })
                            .collect();
                        match session.permission_broker().user_create_group(
                            &session.id().to_string(),
                            ids,
                            p.label,
                            &revisions,
                        ) {
                            Ok(group) => typed_response(
                                id,
                                atman_proto::PermissionGroupView {
                                    group_id: group.group_id.0,
                                    request_ids: group
                                        .request_ids
                                        .into_iter()
                                        .map(|id| id.0)
                                        .collect(),
                                    revision: group.revision,
                                    label: group.label,
                                },
                            ),
                            Err(e) => JsonRpcResponse::err(id, permission_error(e)),
                        }
                    }
                    Err(error) => JsonRpcResponse::err(id, error),
                },
                Err(e) => JsonRpcResponse::err(id, JsonRpcError::invalid_params(e.to_string())),
            }
        }
        Method::ResolvePermissionRequests => {
            let parsed: Result<ResolvePermissionRequestsRequest, _> =
                serde_json::from_value(req.params.unwrap_or(json!({})));
            match parsed {
                Ok(p) => match authorized_permission_session(&state, &p.target, principal_id).await
                {
                    Ok(session) => {
                        let (request_ids, revisions, group) = match p.selector {
                            PermissionRpcSelector::Requests {
                                request_ids,
                                expected_request_revisions,
                            } => (
                                request_ids
                                    .into_iter()
                                    .map(atman_runtime::permission::PermissionRequestId)
                                    .collect(),
                                expected_request_revisions
                                    .into_iter()
                                    .map(|(id, revision)| {
                                        (
                                            atman_runtime::permission::PermissionRequestId(id),
                                            revision,
                                        )
                                    })
                                    .collect(),
                                None,
                            ),
                            PermissionRpcSelector::Group {
                                group_id,
                                expected_group_revision,
                            } => (
                                Vec::new(),
                                HashMap::new(),
                                Some((
                                    atman_runtime::permission::PermissionGroupId(group_id),
                                    expected_group_revision,
                                )),
                            ),
                        };
                        let retained = session.clone();
                        let principal = principal_id.to_owned();
                        let decision = session
                            .apply_instance_effect(move || {
                                retained.permission_broker().user_resolve(
                                    &retained.id().to_string(),
                                    Some(principal),
                                    request_ids,
                                    &revisions,
                                    group,
                                    permission_action(p.action),
                                    permission_scope(p.scope),
                                    p.reason,
                                )
                            })
                            .await;
                        match decision {
                            Ok(Ok(results)) => {
                                let response = atman_proto::ResolvePermissionRequestsResponse {
                                    resolutions: results
                                        .into_iter()
                                        .map(|result| atman_proto::PermissionResolutionView {
                                            request_id: result.request_id.0,
                                            decision: approval_decision_view(&result.outcome),
                                            outcome: format!("{:?}", result.outcome),
                                        })
                                        .collect(),
                                };
                                JsonRpcResponse::ok(
                                    id,
                                    serde_json::to_value(response).unwrap_or(json!({})),
                                )
                            }
                            Ok(Err(e)) => JsonRpcResponse::err(id, permission_error(e)),
                            Err(error) => {
                                JsonRpcResponse::err(id, JsonRpcError::internal(error.to_string()))
                            }
                        }
                    }
                    Err(error) => JsonRpcResponse::err(id, error),
                },
                Err(e) => JsonRpcResponse::err(id, JsonRpcError::invalid_params(e.to_string())),
            }
        }
        Method::GoalGet
        | Method::GoalSet
        | Method::GoalPause
        | Method::GoalResume
        | Method::GoalClear => {
            let parsed =
                serde_json::from_value::<atman_proto::GoalRequest>(req.params.unwrap_or(json!({})));
            let p = match parsed {
                Ok(p) => p,
                Err(e) => {
                    return JsonRpcResponse::err(id, JsonRpcError::invalid_params(e.to_string()));
                }
            };
            if flow_service::uses_persistent(&state, &p.session_id, p.instance_id) {
                return match flow_service::goal::dispatch(state, req.method, p, principal_id).await
                {
                    Ok(value) => JsonRpcResponse::ok(id, value),
                    Err(error) => JsonRpcResponse::err(id, error),
                };
            }
            let Some(session) = state.authorized_live_session(&p.session_id, principal_id) else {
                return JsonRpcResponse::err(
                    id,
                    JsonRpcError::application("session is not live or is owned by another client"),
                );
            };
            let control = match p.target_handle.as_deref().filter(|h| *h != "root") {
                None => session.goal_control.clone(),
                Some(handle) => match session
                    .flow_registry
                    .lookup(handle)
                    .ok()
                    .and_then(|e| e.goal_control.get().cloned())
                {
                    Some(control) if session.goal_control.is_ancestor_of(&control) => control,
                    _ => {
                        return JsonRpcResponse::err(
                            id,
                            JsonRpcError::invalid_params("unknown goal target"),
                        );
                    }
                },
            };
            let result = if req.method == methods::GOAL_GET {
                Ok(control.snapshot())
            } else {
                if control.record().is_some()
                    && (p.goal_id.is_none() || p.expected_revision.is_none())
                {
                    return JsonRpcResponse::err(
                        id,
                        JsonRpcError::invalid_params("goal_id and expected_revision are required"),
                    );
                }
                use atman_runtime::goal::GoalAction;
                let action = match req.method.as_str() {
                    methods::GOAL_PAUSE => GoalAction::Pause,
                    methods::GOAL_RESUME => GoalAction::Resume,
                    methods::GOAL_CLEAR => GoalAction::Clear,
                    _ => GoalAction::Set {
                        text: p.text.unwrap_or_default(),
                    },
                };
                session.control_goal(
                    p.target_handle.as_deref(),
                    action,
                    p.goal_id.as_deref().zip(p.expected_revision),
                )
            };
            match result {
                Ok(snapshot) => {
                    JsonRpcResponse::ok(id, serde_json::to_value(snapshot).expect("goal snapshot"))
                }
                Err(error) => {
                    JsonRpcResponse::err(id, JsonRpcError::application(error.to_string()))
                }
            }
        }
        Method::RunFlow | Method::GoalRun => {
            let Some(launcher) = state.launcher() else {
                return JsonRpcResponse::err(
                    id,
                    JsonRpcError::application("daemon started without a run launcher"),
                );
            };
            let params = req.params.unwrap_or(json!({}));
            let parsed: Result<RunFlowRequest, _> = serde_json::from_value(params);
            match parsed {
                Ok(p) => {
                    let args: Vec<(String, Value)> = p
                        .args
                        .into_iter()
                        .map(|(k, v)| (k, Value::from_json(v)))
                        .collect();
                    match launcher
                        .spawn_as_with_options(
                            state.clone(),
                            &p.flow_path,
                            args,
                            principal_id,
                            crate::run::RunOptions {
                                reasoning: p.reasoning,
                                images: p.images,
                                goal: req.method == methods::GOAL_RUN,
                            },
                        )
                        .await
                    {
                        Ok(spawned) => {
                            let resp = RunFlowResponse {
                                session_id: spawned.session_id,
                                instance_id: spawned.instance_id,
                                run_id: spawned.run_id,
                            };
                            match serde_json::to_value(&resp) {
                                Ok(mut v) => {
                                    if let Some(goal_id) = spawned.goal_id {
                                        v["goal_id"] = json!(goal_id);
                                    }
                                    JsonRpcResponse::ok(id, v)
                                }
                                Err(e) => {
                                    JsonRpcResponse::err(id, JsonRpcError::internal(e.to_string()))
                                }
                            }
                        }
                        Err(e) => {
                            JsonRpcResponse::err(id, JsonRpcError::application(e.to_string()))
                        }
                    }
                }
                Err(e) => JsonRpcResponse::err(id, JsonRpcError::invalid_params(e.to_string())),
            }
        }
    }
}

async fn git_co_author_setting(
    state: &DaemonState,
    method: Method,
    params: serde_json::Value,
    principal: &str,
) -> Result<atman_proto::settings::GitCoAuthorSetting, JsonRpcError> {
    use atman_proto::settings::{GitCoAuthorSetting, ReadGitCoAuthorRequest};
    use atman_runtime::config_hub::ConfigHub;

    flow_service::authorize_operator(principal)?;
    let update = if method == Method::SettingsGitCoAuthorSet {
        let setting: GitCoAuthorSetting = serde_json::from_value(params)
            .map_err(|_| JsonRpcError::invalid_params("expected an enabled boolean"))?;
        let operation = state
            .runtime_host()
            .retain_host_operation()
            .map_err(|_| JsonRpcError::application("Settings host is stopping"))?;
        Some((setting, operation))
    } else {
        serde_json::from_value::<ReadGitCoAuthorRequest>(params)
            .map_err(|_| JsonRpcError::invalid_params("expected empty settings parameters"))?;
        None
    };
    tokio::task::spawn_blocking(move || {
        let config = ConfigHub::global()?;
        if let Some((setting, _operation)) = update {
            config.set_git_co_author_enabled(setting.enabled)?;
            Ok(setting)
        } else {
            config
                .git_co_author_enabled()
                .map(|enabled| GitCoAuthorSetting { enabled })
        }
    })
    .await
    .map_err(|_| JsonRpcError::application("Settings worker is unavailable"))?
    .map_err(|_| JsonRpcError::application("Unable to access the global Commit Co-author setting"))
}

fn typed_response(id: Option<serde_json::Value>, result: impl serde::Serialize) -> JsonRpcResponse {
    match serde_json::to_value(result) {
        Ok(value) => JsonRpcResponse::ok(id, value),
        Err(error) => JsonRpcResponse::err(id, JsonRpcError::internal(error.to_string())),
    }
}
