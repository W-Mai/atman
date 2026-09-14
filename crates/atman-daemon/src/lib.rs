use std::sync::Arc;

use atman_proto::{
    CancelRunRequest, CreatePermissionGroupRequest, JsonRpcError, JsonRpcRequest, JsonRpcResponse,
    ListPermissionRequestsRequest, PermissionRpcAction, PermissionRpcScope, PermissionRpcSelector,
    ResolvePermissionRequestsRequest, ResolvePromptRequest, RunFlowRequest, RunFlowResponse,
    methods,
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
                run_id: atman_runtime::event::FlowRunId(run_id.0),
                tool_name,
            }
        }
        PermissionRpcScope::ChildRunSamePathRule {
            run_id,
            tool_name,
            workspace_relative_path,
        } => atman_runtime::permission::GrantScope::ChildRunSamePathRule {
            run_id: atman_runtime::event::FlowRunId(run_id.0),
            tool_name,
            workspace_relative_path,
        },
    })
}

fn permission_error(error: impl std::fmt::Display) -> JsonRpcError {
    JsonRpcError::application(error.to_string())
}

fn authorized_permission_session(
    state: &DaemonState,
    session_id: &atman_proto::SessionId,
    principal_id: &str,
) -> Result<Arc<atman_runtime::Session>, JsonRpcError> {
    state
        .authorized_live_session(session_id, principal_id)
        .ok_or_else(|| JsonRpcError::application("permission denied for session"))
}

pub mod bootstrap;
pub mod config;
pub mod http;
pub mod openapi;
pub mod pidfile;
pub mod preview_server;
pub mod prompt_bridge;
pub mod run;
pub mod state;
pub mod unix;

pub use state::{DaemonState, LiveSession};

pub async fn dispatch(state: Arc<DaemonState>, req: JsonRpcRequest) -> JsonRpcResponse {
    dispatch_as(state, req, "local-daemon").await
}

pub async fn dispatch_as(
    state: Arc<DaemonState>,
    req: JsonRpcRequest,
    principal_id: &str,
) -> JsonRpcResponse {
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
        methods::LIST_SESSIONS => {
            let params = req.params.unwrap_or(json!({}));
            let project_root = params.get("project_root").and_then(|value| value.as_str());
            let search = params.get("search").and_then(|value| value.as_str());
            let limit = params
                .get("limit")
                .and_then(|value| value.as_u64())
                .map(|value| value as usize);
            match state.list_sessions_query(project_root, search, limit) {
                Ok(summaries) => match serde_json::to_value(&summaries) {
                    Ok(v) => JsonRpcResponse::ok(id, v),
                    Err(e) => JsonRpcResponse::err(id, JsonRpcError::internal(e.to_string())),
                },
                Err(e) => JsonRpcResponse::err(id, JsonRpcError::internal(e.to_string())),
            }
        }
        methods::RENAME_SESSION => {
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
        methods::CANCEL_RUN => {
            let params = req.params.unwrap_or(json!({}));
            let parsed: Result<CancelRunRequest, _> = serde_json::from_value(params);
            match parsed {
                Ok(p) => {
                    let cancelled = state.cancel_run(&p.run_id);
                    JsonRpcResponse::ok(id, json!({"cancelled": cancelled}))
                }
                Err(e) => JsonRpcResponse::err(id, JsonRpcError::invalid_params(e.to_string())),
            }
        }
        methods::RESOLVE_PROMPT => {
            let params = req.params.unwrap_or(json!({}));
            let parsed: Result<ResolvePromptRequest, _> = serde_json::from_value(params);
            match parsed {
                Ok(p) => {
                    let resolved = state.resolve_prompt_for_session(
                        &p.prompt_id,
                        p.answer,
                        p.session_id.as_ref(),
                    );
                    JsonRpcResponse::ok(id, json!({"resolved": resolved}))
                }
                Err(e) => JsonRpcResponse::err(id, JsonRpcError::invalid_params(e.to_string())),
            }
        }
        methods::LIST_PERMISSION_REQUESTS => {
            let parsed: Result<ListPermissionRequestsRequest, _> =
                serde_json::from_value(req.params.unwrap_or(json!({})));
            match parsed {
                Ok(p) => match authorized_permission_session(&state, &p.session_id, principal_id) {
                    Ok(session) => {
                        let (requests, groups) = session
                            .permission_broker()
                            .user_list(&p.session_id.0.to_string());
                        let response = atman_proto::ListPermissionRequestsResponse {
                            requests: requests
                                .into_iter()
                                .map(|request| atman_proto::PermissionRequestView {
                                    request_id: request.request_id.0,
                                    session_id: request.session_id,
                                    requesting_run_id: atman_proto::FlowRunId(
                                        request.requesting_run_id.0,
                                    ),
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
        methods::CREATE_PERMISSION_GROUP => {
            let parsed: Result<CreatePermissionGroupRequest, _> =
                serde_json::from_value(req.params.unwrap_or(json!({})));
            match parsed {
                Ok(p) => match authorized_permission_session(&state, &p.session_id, principal_id) {
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
                            &p.session_id.0.to_string(),
                            ids,
                            p.label,
                            &revisions,
                        ) {
                            Ok(group) => JsonRpcResponse::ok(
                                id,
                                json!({"group_id": group.group_id.0, "request_ids": group.request_ids, "revision": group.revision, "label": group.label}),
                            ),
                            Err(e) => JsonRpcResponse::err(id, permission_error(e)),
                        }
                    }
                    Err(error) => JsonRpcResponse::err(id, error),
                },
                Err(e) => JsonRpcResponse::err(id, JsonRpcError::invalid_params(e.to_string())),
            }
        }
        methods::RESOLVE_PERMISSION_REQUESTS => {
            let parsed: Result<ResolvePermissionRequestsRequest, _> =
                serde_json::from_value(req.params.unwrap_or(json!({})));
            match parsed {
                Ok(p) => match authorized_permission_session(&state, &p.session_id, principal_id) {
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
                        match session.permission_broker().user_resolve(
                            &p.session_id.0.to_string(),
                            Some(principal_id.to_owned()),
                            request_ids,
                            &revisions,
                            group,
                            permission_action(p.action),
                            permission_scope(p.scope),
                            p.reason,
                        ) {
                            Ok(results) => {
                                let response = atman_proto::ResolvePermissionRequestsResponse {
                                    resolutions: results
                                        .into_iter()
                                        .map(|result| atman_proto::PermissionResolutionView {
                                            request_id: result.request_id.0,
                                            outcome: format!("{:?}", result.outcome),
                                        })
                                        .collect(),
                                };
                                JsonRpcResponse::ok(
                                    id,
                                    serde_json::to_value(response).unwrap_or(json!({})),
                                )
                            }
                            Err(e) => JsonRpcResponse::err(id, permission_error(e)),
                        }
                    }
                    Err(error) => JsonRpcResponse::err(id, error),
                },
                Err(e) => JsonRpcResponse::err(id, JsonRpcError::invalid_params(e.to_string())),
            }
        }
        methods::RUN_FLOW => {
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
                    let args: Vec<(String, atman_runtime::Value)> = p
                        .args
                        .into_iter()
                        .map(|(k, v)| (k, atman_runtime::Value::from_json(v)))
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
                            },
                        )
                        .await
                    {
                        Ok(spawned) => {
                            let resp = RunFlowResponse {
                                session_id: spawned.session_id,
                                run_id: spawned.run_id,
                            };
                            match serde_json::to_value(&resp) {
                                Ok(v) => JsonRpcResponse::ok(id, v),
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
        other => JsonRpcResponse::err(id, JsonRpcError::method_not_found(other)),
    }
}
