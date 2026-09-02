use atman_proto::{
    CancelRunResponse, CapabilitiesRequest, CapabilitiesResponse, DaemonGeneration, EventCursor,
    GetSessionSnapshotRequest, GetSessionUpdatesRequest, JsonRpcError, JsonRpcRequest,
    JsonRpcResponse, ListSessionsRequest, MethodCapability, PermissionRpcAction,
    PermissionRpcScope, PingResponse, ProtocolLimits, ResolvePromptResponse, RpcMethod,
    RpcMethodDescriptor, RunFlowResponse, method_descriptor, methods, rpc,
};
use serde_json::json;
use std::sync::Arc;

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

pub mod bootstrap;
pub mod config;
mod events;
pub mod http;
mod idempotency;
pub mod openapi;
pub mod pidfile;
mod projection;
pub mod prompt_bridge;
pub mod run;
mod session_actor;
pub mod state;
pub mod unix;

pub use state::{DaemonState, LiveRun};

pub const SUPPORTED_METHODS: &[RpcMethodDescriptor] = &[
    method_descriptor::<rpc::DaemonCapabilities>(),
    method_descriptor::<rpc::Ping>(),
    method_descriptor::<rpc::ListSessions>(),
    method_descriptor::<rpc::RenameSession>(),
    method_descriptor::<rpc::RunFlow>(),
    method_descriptor::<rpc::CancelRun>(),
    method_descriptor::<rpc::GetEvents>(),
    method_descriptor::<rpc::GetSessionSnapshot>(),
    method_descriptor::<rpc::GetSessionUpdates>(),
    method_descriptor::<rpc::ResolvePrompt>(),
    method_descriptor::<rpc::ListPermissionRequests>(),
    method_descriptor::<rpc::CreatePermissionGroup>(),
    method_descriptor::<rpc::ResolvePermissionRequests>(),
];

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
                        methods: SUPPORTED_METHODS
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
            Ok(params) if !params.title.trim().is_empty() => {
                let request_id = params
                    .request_id
                    .clone()
                    .unwrap_or_else(atman_proto::RequestId::now);
                let fingerprint = match serde_json::to_value(&params) {
                    Ok(value) => value,
                    Err(error) => {
                        return JsonRpcResponse::err(
                            id,
                            JsonRpcError::internal(format!(
                                "could not encode rename_session command: {error}"
                            )),
                        );
                    }
                };
                let operation_state = state.clone();
                let operation_principal = principal_id.to_owned();
                let outcome = state
                    .idempotency
                    .execute(
                        principal_id,
                        request_id,
                        methods::RENAME_SESSION,
                        fingerprint,
                        async move {
                            let summary = operation_state
                                .rename_session(
                                    &params.session_id,
                                    &params.title,
                                    &operation_principal,
                                )
                                .await
                                .map_err(|error| JsonRpcError::application(error.to_string()))?;
                            serde_json::to_value(summary).map_err(|error| {
                                JsonRpcError::internal(format!(
                                    "could not encode rename_session result: {error}"
                                ))
                            })
                        },
                    )
                    .await;
                match outcome {
                    Ok(result) => JsonRpcResponse::ok(id, result),
                    Err(error) => JsonRpcResponse::err(id, error),
                }
            }
            Ok(_) => {
                JsonRpcResponse::err(id, JsonRpcError::invalid_params("title must not be empty"))
            }
            Err(error) => JsonRpcResponse::err(id, error),
        },
        methods::CANCEL_RUN => match parse_params::<rpc::CancelRun>(req.params) {
            Ok(p) => match state.cancel_run(&p.run_id, principal_id).await {
                Ok(cancelled) => {
                    method_response::<rpc::CancelRun>(id, CancelRunResponse { cancelled })
                }
                Err(error) => {
                    JsonRpcResponse::err(id, JsonRpcError::application(error.to_string()))
                }
            },
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
        methods::GET_SESSION_SNAPSHOT => {
            match parse_params::<rpc::GetSessionSnapshot>(req.params) {
                Ok(GetSessionSnapshotRequest { session_id }) => {
                    match state.session_snapshot(&session_id, principal_id).await {
                        Ok(snapshot) => method_response::<rpc::GetSessionSnapshot>(id, snapshot),
                        Err(error) => {
                            JsonRpcResponse::err(id, JsonRpcError::application(error.to_string()))
                        }
                    }
                }
                Err(error) => JsonRpcResponse::err(id, error),
            }
        }
        methods::GET_SESSION_UPDATES => match parse_params::<rpc::GetSessionUpdates>(req.params) {
            Ok(GetSessionUpdatesRequest {
                session_id,
                after_cursor,
                limit,
            }) => match state
                .session_updates(&session_id, principal_id, after_cursor, limit)
                .await
            {
                Ok(updates) => method_response::<rpc::GetSessionUpdates>(id, updates),
                Err(error) => {
                    JsonRpcResponse::err(id, JsonRpcError::application(error.to_string()))
                }
            },
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
                Ok(p) => match state
                    .list_permission_requests(&p.session_id, principal_id)
                    .await
                {
                    Ok(response) => method_response::<rpc::ListPermissionRequests>(id, response),
                    Err(error) => {
                        JsonRpcResponse::err(id, JsonRpcError::application(error.to_string()))
                    }
                },
                Err(error) => JsonRpcResponse::err(id, error),
            }
        }
        methods::CREATE_PERMISSION_GROUP => {
            match parse_params::<rpc::CreatePermissionGroup>(req.params) {
                Ok(p) => match state.create_permission_group(p, principal_id).await {
                    Ok(response) => method_response::<rpc::CreatePermissionGroup>(id, response),
                    Err(error) => {
                        JsonRpcResponse::err(id, JsonRpcError::application(error.to_string()))
                    }
                },
                Err(error) => JsonRpcResponse::err(id, error),
            }
        }
        methods::RESOLVE_PERMISSION_REQUESTS => {
            match parse_params::<rpc::ResolvePermissionRequests>(req.params) {
                Ok(p) => {
                    let action = permission_action(p.action.clone());
                    let scope = permission_scope(p.scope.clone());
                    match state
                        .resolve_permission_requests(p, principal_id, action, scope)
                        .await
                    {
                        Ok(response) => {
                            method_response::<rpc::ResolvePermissionRequests>(id, response)
                        }
                        Err(error) => {
                            JsonRpcResponse::err(id, JsonRpcError::application(error.to_string()))
                        }
                    }
                }
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
