use std::sync::Arc;

use atman_proto::{
    CancelRunResponse, CapabilitiesRequest, CapabilitiesResponse, CreatePermissionGroupResponse,
    DaemonGeneration, EventCursor, JsonRpcError, JsonRpcRequest, JsonRpcResponse,
    ListSessionsRequest, MethodCapability, PermissionRpcAction, PermissionRpcScope,
    PermissionRpcSelector, PingResponse, ProtocolLimits, RenameSessionRequest,
    ResolvePromptResponse, RpcMethod, RunFlowResponse, methods, rpc,
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

fn parse_params<M: RpcMethod>(
    params: Option<serde_json::Value>,
) -> Result<M::Params, JsonRpcError> {
    serde_json::from_value(params.unwrap_or_else(|| json!({})))
        .map_err(|error| JsonRpcError::invalid_params(error.to_string()))
}

fn method_response<M: RpcMethod>(
    id: Option<serde_json::Value>,
    output: M::Output,
) -> JsonRpcResponse {
    match serde_json::to_value(output) {
        Ok(value) => JsonRpcResponse::ok(id, value),
        Err(error) => JsonRpcResponse::err(id, JsonRpcError::internal(error.to_string())),
    }
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
mod events;
pub mod http;
pub mod openapi;
pub mod pidfile;
pub mod prompt_bridge;
pub mod run;
pub mod state;
pub mod unix;

pub use state::{DaemonState, LiveRun};

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
        methods::DAEMON_CAPABILITIES => {
            let parsed = parse_params::<rpc::DaemonCapabilities>(req.params);
            match parsed {
                Ok(CapabilitiesRequest { .. }) => method_response::<rpc::DaemonCapabilities>(
                    id,
                    CapabilitiesResponse {
                        protocol_version: atman_proto::PROTOCOL_VERSION,
                        daemon_version: env!("CARGO_PKG_VERSION").into(),
                        daemon_generation: DaemonGeneration(state.daemon_generation().to_owned()),
                        event_schema_version: atman_proto::EVENT_SCHEMA_VERSION,
                        methods: methods::ALL
                            .iter()
                            .map(|method| MethodCapability {
                                name: method.name.into(),
                                kind: method.kind,
                                revision: method.revision,
                            })
                            .collect(),
                        limits: ProtocolLimits {
                            max_event_page_size: events::MAX_EVENT_PAGE_SIZE,
                            subscriber_buffer: 2_048,
                        },
                    },
                ),
                Err(error) => JsonRpcResponse::err(id, error),
            }
        }
        methods::PING => method_response::<rpc::Ping>(
            id,
            PingResponse {
                pong: true,
                version: env!("CARGO_PKG_VERSION").into(),
            },
        ),
        methods::LIST_SESSIONS => match parse_params::<rpc::ListSessions>(req.params) {
            Ok(ListSessionsRequest {
                project_root,
                search,
                limit,
            }) => {
                match state.list_sessions_query(project_root.as_deref(), search.as_deref(), limit) {
                    Ok(summaries) => method_response::<rpc::ListSessions>(id, summaries),
                    Err(error) => {
                        JsonRpcResponse::err(id, JsonRpcError::internal(error.to_string()))
                    }
                }
            }
            Err(error) => JsonRpcResponse::err(id, error),
        },
        methods::RENAME_SESSION => match parse_params::<rpc::RenameSession>(req.params) {
            Ok(RenameSessionRequest { session_id, title }) if !title.trim().is_empty() => {
                match state.rename_session(&session_id, &title) {
                    Ok(summary) => method_response::<rpc::RenameSession>(id, summary),
                    Err(error) => {
                        JsonRpcResponse::err(id, JsonRpcError::application(error.to_string()))
                    }
                }
            }
            Ok(_) => {
                JsonRpcResponse::err(id, JsonRpcError::invalid_params("title must not be empty"))
            }
            Err(error) => JsonRpcResponse::err(id, error),
        },
        methods::CANCEL_RUN => match parse_params::<rpc::CancelRun>(req.params) {
            Ok(p) => {
                let cancelled = state.cancel_run(&p.run_id);
                method_response::<rpc::CancelRun>(id, CancelRunResponse { cancelled })
            }
            Err(error) => JsonRpcResponse::err(id, error),
        },
        methods::GET_EVENTS => match parse_params::<rpc::GetEvents>(req.params) {
            Ok(p) if state.can_read_session(&p.session_id, principal_id) => {
                let path = state
                    .sessions_root()
                    .join(p.session_id.to_string())
                    .join("events.jsonl");
                match events::read_event_page(
                    &path,
                    EventCursor(p.since_seq.unwrap_or_default()),
                    events::MAX_EVENT_PAGE_SIZE,
                )
                .await
                {
                    Ok(page) => method_response::<rpc::GetEvents>(id, page),
                    Err(error) => {
                        JsonRpcResponse::err(id, JsonRpcError::application(error.to_string()))
                    }
                }
            }
            Ok(_) => JsonRpcResponse::err(
                id,
                JsonRpcError::application("permission denied for session"),
            ),
            Err(error) => JsonRpcResponse::err(id, error),
        },
        methods::RESOLVE_PROMPT => match parse_params::<rpc::ResolvePrompt>(req.params) {
            Ok(p) => {
                let resolved = state.resolve_prompt(&p.prompt_id, p.answer);
                method_response::<rpc::ResolvePrompt>(id, ResolvePromptResponse { resolved })
            }
            Err(error) => JsonRpcResponse::err(id, error),
        },
        methods::LIST_PERMISSION_REQUESTS => {
            match parse_params::<rpc::ListPermissionRequests>(req.params) {
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
                        method_response::<rpc::ListPermissionRequests>(id, response)
                    }
                    Err(error) => JsonRpcResponse::err(id, error),
                },
                Err(error) => JsonRpcResponse::err(id, error),
            }
        }
        methods::CREATE_PERMISSION_GROUP => {
            match parse_params::<rpc::CreatePermissionGroup>(req.params) {
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
                            Ok(group) => method_response::<rpc::CreatePermissionGroup>(
                                id,
                                CreatePermissionGroupResponse {
                                    group_id: group.group_id.0,
                                    request_ids: group
                                        .request_ids
                                        .into_iter()
                                        .map(|request_id| request_id.0)
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
                Err(error) => JsonRpcResponse::err(id, error),
            }
        }
        methods::RESOLVE_PERMISSION_REQUESTS => {
            match parse_params::<rpc::ResolvePermissionRequests>(req.params) {
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
                                method_response::<rpc::ResolvePermissionRequests>(id, response)
                            }
                            Err(e) => JsonRpcResponse::err(id, permission_error(e)),
                        }
                    }
                    Err(error) => JsonRpcResponse::err(id, error),
                },
                Err(error) => JsonRpcResponse::err(id, error),
            }
        }
        methods::RUN_FLOW => {
            let Some(launcher) = state.launcher() else {
                return JsonRpcResponse::err(
                    id,
                    JsonRpcError::application("daemon started without a run launcher"),
                );
            };
            match parse_params::<rpc::RunFlow>(req.params) {
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
                        Ok(spawned) => method_response::<rpc::RunFlow>(
                            id,
                            RunFlowResponse {
                                session_id: spawned.session_id,
                                run_id: spawned.run_id,
                            },
                        ),
                        Err(e) => {
                            JsonRpcResponse::err(id, JsonRpcError::application(e.to_string()))
                        }
                    }
                }
                Err(error) => JsonRpcResponse::err(id, error),
            }
        }
        other => JsonRpcResponse::err(id, JsonRpcError::method_not_found(other)),
    }
}
