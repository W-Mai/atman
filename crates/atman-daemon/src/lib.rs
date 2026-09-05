use atman_proto::{
    AutoNameSessionResponse, CancelRunResponse, CapabilitiesRequest, CapabilitiesResponse,
    CompactSessionResponse, DaemonGeneration, EventCursor, GetSessionSnapshotRequest,
    GetSessionUpdatesRequest, InspectResourceResponse, InterjectSessionResponse, JsonRpcError,
    JsonRpcRequest, JsonRpcResponse, ListProjectsRequest, ListResourcesResponse,
    ListSessionsRequest, MethodCapability, MoveSessionResponse, PermissionRpcAction,
    PermissionRpcScope, PingResponse, ProtocolLimits, ReleaseResourceResponse,
    ReloadSessionMcpResponse, RenameSessionResponse, RequestId, ResizeTerminalResourceResponse,
    ResolveCompactReviewResponse, ResolvePromptResponse, RetainResourceResponse, RpcMethod,
    RpcMethodDescriptor, RunFlowResponse, SendMessageResponse, StartRunResponse,
    SubmitFormResponse, TerminateResourceResponse, UpdateSessionTrustResponse, method_descriptor,
    methods, rpc,
};
use serde_json::json;
use std::future::Future;
use std::sync::Arc;

pub mod server;

// Owner-only local access and the daemon bearer authenticate the same operator.
pub(crate) const LOCAL_OPERATOR_PRINCIPAL: &str = "local-daemon";

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

fn interjection_level(
    level: atman_proto::InterjectionLevel,
) -> atman_runtime::injection::InjectionLevel {
    match level {
        atman_proto::InterjectionLevel::Nudge => atman_runtime::injection::InjectionLevel::L1Nudge,
        atman_proto::InterjectionLevel::CourseCorrect => {
            atman_runtime::injection::InjectionLevel::L2CourseCorrect
        }
        atman_proto::InterjectionLevel::Redirect => {
            atman_runtime::injection::InjectionLevel::L3Redirect
        }
        atman_proto::InterjectionLevel::HardStop => {
            atman_runtime::injection::InjectionLevel::L4HardStop
        }
    }
}

fn runtime_args(
    args: serde_json::Map<String, serde_json::Value>,
) -> Vec<(String, atman_runtime::Value)> {
    args.into_iter()
        .map(|(key, value)| (key, atman_runtime::Value::from_json(value)))
        .collect()
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

async fn execute_command<M, F>(
    state: &DaemonState,
    principal_id: &str,
    request_id: Option<RequestId>,
    params: &M::Params,
    operation: F,
) -> Result<M::Output, JsonRpcError>
where
    M: RpcMethod,
    F: Future<Output = Result<M::Output, JsonRpcError>> + Send + 'static,
{
    let accepting_commands = state.is_accepting_commands();
    let encoded = state
        .idempotency
        .execute(
            principal_id,
            request_id.unwrap_or_else(RequestId::now),
            M::NAME,
            params,
            async move {
                if !accepting_commands {
                    return Err(JsonRpcError::application("daemon is shutting down"));
                }
                serde_json::to_value(operation.await?).map_err(|error| {
                    JsonRpcError::internal(format!(
                        "could not encode {} command result: {error}",
                        M::NAME
                    ))
                })
            },
        )
        .await?;
    serde_json::from_value(encoded).map_err(|error| {
        JsonRpcError::internal(format!(
            "could not decode cached {} command result: {error}",
            M::NAME
        ))
    })
}

pub mod bootstrap;
pub mod config;
mod events;
pub mod http;
mod idempotency;
pub mod openapi;
pub mod pidfile;
pub mod project_registry;
mod projection;
mod projection_snapshot;
pub mod prompt_bridge;
pub mod run;
mod session_actor;
pub mod state;
pub mod unix;

pub use session_actor::{RenameSessionCommit, RunCancellationCommit};
pub use state::{DaemonState, LiveRun};

pub const SUPPORTED_METHODS: &[RpcMethodDescriptor] = &[
    method_descriptor::<rpc::DaemonCapabilities>(),
    method_descriptor::<rpc::Ping>(),
    method_descriptor::<rpc::CreateSession>(),
    method_descriptor::<rpc::CloseSession>(),
    method_descriptor::<rpc::DeleteSession>(),
    method_descriptor::<rpc::SendMessage>(),
    method_descriptor::<rpc::InterjectSession>(),
    method_descriptor::<rpc::UpdateSessionTrust>(),
    method_descriptor::<rpc::ReloadSessionMcp>(),
    method_descriptor::<rpc::AutoNameSession>(),
    method_descriptor::<rpc::MoveSession>(),
    method_descriptor::<rpc::ListProjects>(),
    method_descriptor::<rpc::ListSessions>(),
    method_descriptor::<rpc::RenameSession>(),
    method_descriptor::<rpc::StartRun>(),
    method_descriptor::<rpc::RunFlow>(),
    method_descriptor::<rpc::CancelRun>(),
    method_descriptor::<rpc::GetEvents>(),
    method_descriptor::<rpc::GetSessionSnapshot>(),
    method_descriptor::<rpc::GetSessionUpdates>(),
    method_descriptor::<rpc::ResolvePrompt>(),
    method_descriptor::<rpc::SubmitForm>(),
    method_descriptor::<rpc::CompactSession>(),
    method_descriptor::<rpc::ResolveCompactReview>(),
    method_descriptor::<rpc::ListPermissionRequests>(),
    method_descriptor::<rpc::CreatePermissionGroup>(),
    method_descriptor::<rpc::ResolvePermissionRequests>(),
    method_descriptor::<rpc::ListResources>(),
    method_descriptor::<rpc::InspectResource>(),
    method_descriptor::<rpc::TerminateResource>(),
    method_descriptor::<rpc::ResizeTerminalResource>(),
    method_descriptor::<rpc::RetainResource>(),
    method_descriptor::<rpc::ReleaseResource>(),
];

pub async fn dispatch(state: Arc<DaemonState>, req: JsonRpcRequest) -> JsonRpcResponse {
    dispatch_as(state, req, LOCAL_OPERATOR_PRINCIPAL).await
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
                        snapshot_schema_version: atman_proto::SNAPSHOT_SCHEMA_VERSION,
                        event_schema_version: atman_proto::PROJECTION_EVENT_SCHEMA_VERSION,
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
        methods::LIST_PROJECTS => match parse_params::<rpc::ListProjects>(req.params) {
            Ok(ListProjectsRequest { search, limit }) => {
                match state.list_projects_query(search.as_deref(), limit) {
                    Ok(projects) => method_response::<rpc::ListProjects>(id, projects),
                    Err(error) => {
                        JsonRpcResponse::err(id, JsonRpcError::internal(error.to_string()))
                    }
                }
            }
            Err(error) => JsonRpcResponse::err(id, error),
        },
        methods::CREATE_SESSION => {
            let Some(launcher) = state.launcher() else {
                return JsonRpcResponse::err(
                    id,
                    JsonRpcError::application("daemon started without a session launcher"),
                );
            };
            match parse_params::<rpc::CreateSession>(req.params) {
                Ok(params) => {
                    let operation_state = state.clone();
                    let operation_principal = principal_id.to_owned();
                    let operation_params = params.clone();
                    match execute_command::<rpc::CreateSession, _>(
                        &state,
                        principal_id,
                        params.request_id.clone(),
                        &params,
                        async move {
                            let session_id = launcher
                                .create_session(
                                    operation_state.clone(),
                                    operation_params.project_root.as_deref(),
                                    operation_params.title.as_deref(),
                                    &operation_principal,
                                )
                                .await
                                .map_err(|error| JsonRpcError::application(error.to_string()))?;
                            operation_state
                                .session_snapshot(&session_id, &operation_principal)
                                .await
                                .map_err(|error| JsonRpcError::application(error.to_string()))
                        },
                    )
                    .await
                    {
                        Ok(snapshot) => method_response::<rpc::CreateSession>(id, snapshot),
                        Err(error) => JsonRpcResponse::err(id, error),
                    }
                }
                Err(error) => JsonRpcResponse::err(id, error),
            }
        }
        methods::CLOSE_SESSION => match parse_params::<rpc::CloseSession>(req.params) {
            Ok(params) => {
                let operation_state = state.clone();
                let operation_principal = principal_id.to_owned();
                let operation_params = params.clone();
                match execute_command::<rpc::CloseSession, _>(
                    &state,
                    principal_id,
                    params.request_id.clone(),
                    &params,
                    async move {
                        operation_state
                            .close_session(&operation_params.session_id, &operation_principal)
                            .await
                            .map_err(|error| JsonRpcError::application(error.to_string()))
                    },
                )
                .await
                {
                    Ok(response) => method_response::<rpc::CloseSession>(id, response),
                    Err(error) => JsonRpcResponse::err(id, error),
                }
            }
            Err(error) => JsonRpcResponse::err(id, error),
        },
        methods::DELETE_SESSION => match parse_params::<rpc::DeleteSession>(req.params) {
            Ok(params) => {
                let operation_state = state.clone();
                let operation_principal = principal_id.to_owned();
                let operation_params = params.clone();
                match execute_command::<rpc::DeleteSession, _>(
                    &state,
                    principal_id,
                    params.request_id.clone(),
                    &params,
                    async move {
                        operation_state
                            .delete_session(&operation_params.session_id, &operation_principal)
                            .await
                            .map_err(|error| JsonRpcError::application(error.to_string()))
                    },
                )
                .await
                {
                    Ok(response) => method_response::<rpc::DeleteSession>(id, response),
                    Err(error) => JsonRpcResponse::err(id, error),
                }
            }
            Err(error) => JsonRpcResponse::err(id, error),
        },
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
        methods::SEND_MESSAGE => {
            let Some(launcher) = state.launcher() else {
                return JsonRpcResponse::err(
                    id,
                    JsonRpcError::application("daemon started without a run launcher"),
                );
            };
            match parse_params::<rpc::SendMessage>(req.params) {
                Ok(params) if !params.text.trim().is_empty() => {
                    let operation_state = state.clone();
                    let operation_principal = principal_id.to_owned();
                    let operation_params = params.clone();
                    let outcome = execute_command::<rpc::SendMessage, _>(
                        &state,
                        principal_id,
                        params.request_id.clone(),
                        &params,
                        async move {
                            let spawned = launcher
                                .send_message_as_with_options(
                                    operation_state.clone(),
                                    &operation_params.session_id,
                                    &operation_params.text,
                                    &operation_principal,
                                    operation_params.reasoning,
                                    operation_params.images,
                                )
                                .await
                                .map_err(|error| JsonRpcError::application(error.to_string()))?;
                            let snapshot = operation_state
                                .session_snapshot(&spawned.session_id, &operation_principal)
                                .await
                                .map_err(|error| JsonRpcError::application(error.to_string()))?;
                            Ok(SendMessageResponse {
                                session_id: spawned.session_id,
                                run_id: spawned.run_id,
                                revision: snapshot.projection.revision,
                                cursor: snapshot.cursor,
                            })
                        },
                    )
                    .await;
                    match outcome {
                        Ok(response) => method_response::<rpc::SendMessage>(id, response),
                        Err(error) => JsonRpcResponse::err(id, error),
                    }
                }
                Ok(_) => JsonRpcResponse::err(
                    id,
                    JsonRpcError::invalid_params("message text must not be empty"),
                ),
                Err(error) => JsonRpcResponse::err(id, error),
            }
        }
        methods::INTERJECT_SESSION => match parse_params::<rpc::InterjectSession>(req.params) {
            Ok(params) if !params.text.trim().is_empty() => {
                let redirect_is_valid = match params.level {
                    atman_proto::InterjectionLevel::Redirect => params
                        .redirect_target
                        .as_deref()
                        .is_some_and(|target| !target.trim().is_empty()),
                    _ => params.redirect_target.is_none(),
                };
                if !redirect_is_valid {
                    return JsonRpcResponse::err(
                        id,
                        JsonRpcError::invalid_params(
                            "redirect_target is required only for redirect interjections",
                        ),
                    );
                }
                let operation_state = state.clone();
                let operation_principal = principal_id.to_owned();
                let operation_params = params.clone();
                let outcome = execute_command::<rpc::InterjectSession, _>(
                    &state,
                    principal_id,
                    params.request_id.clone(),
                    &params,
                    async move {
                        let commit = operation_state
                            .interject_run(
                                &operation_params.session_id,
                                operation_params.run_id.clone(),
                                operation_params.text,
                                interjection_level(operation_params.level),
                                operation_params.redirect_target,
                                &operation_principal,
                            )
                            .await
                            .map_err(|error| JsonRpcError::application(error.to_string()))?;
                        Ok(InterjectSessionResponse {
                            session_id: operation_params.session_id,
                            run_id: operation_params.run_id,
                            injection_id: commit.injection_id,
                            state: atman_proto::InterjectionState::Pending,
                            revision: commit.revision,
                            cursor: commit.cursor,
                        })
                    },
                )
                .await;
                match outcome {
                    Ok(response) => method_response::<rpc::InterjectSession>(id, response),
                    Err(error) => JsonRpcResponse::err(id, error),
                }
            }
            Ok(_) => JsonRpcResponse::err(
                id,
                JsonRpcError::invalid_params("interjection text must not be empty"),
            ),
            Err(error) => JsonRpcResponse::err(id, error),
        },
        methods::RENAME_SESSION => match parse_params::<rpc::RenameSession>(req.params) {
            Ok(params)
                if params
                    .title
                    .as_deref()
                    .is_none_or(|title| !title.trim().is_empty()) =>
            {
                let operation_state = state.clone();
                let operation_principal = principal_id.to_owned();
                let operation_params = params.clone();
                let outcome = execute_command::<rpc::RenameSession, _>(
                    &state,
                    principal_id,
                    params.request_id.clone(),
                    &params,
                    async move {
                        operation_state
                            .rename_session(
                                &operation_params.session_id,
                                operation_params.title,
                                &operation_principal,
                            )
                            .await
                            .map(|commit| RenameSessionResponse {
                                session: commit.session,
                                revision: commit.revision,
                                cursor: commit.cursor,
                            })
                            .map_err(|error| JsonRpcError::application(error.to_string()))
                    },
                )
                .await;
                match outcome {
                    Ok(result) => method_response::<rpc::RenameSession>(id, result),
                    Err(error) => JsonRpcResponse::err(id, error),
                }
            }
            Ok(_) => {
                JsonRpcResponse::err(id, JsonRpcError::invalid_params("title must not be empty"))
            }
            Err(error) => JsonRpcResponse::err(id, error),
        },
        methods::UPDATE_SESSION_TRUST => {
            match parse_params::<rpc::UpdateSessionTrust>(req.params) {
                Ok(params) => {
                    let operation_state = state.clone();
                    let operation_principal = principal_id.to_owned();
                    let operation_params = params.clone();
                    let outcome = execute_command::<rpc::UpdateSessionTrust, _>(
                        &state,
                        principal_id,
                        params.request_id.clone(),
                        &params,
                        async move {
                            operation_state
                                .update_session_trust(
                                    &operation_params.session_id,
                                    operation_params.trust,
                                    &operation_principal,
                                )
                                .await
                                .map(|commit| UpdateSessionTrustResponse {
                                    session_id: operation_params.session_id,
                                    trust: commit.trust,
                                    revision: commit.revision,
                                    cursor: commit.cursor,
                                })
                                .map_err(|error| JsonRpcError::application(error.to_string()))
                        },
                    )
                    .await;
                    match outcome {
                        Ok(result) => method_response::<rpc::UpdateSessionTrust>(id, result),
                        Err(error) => JsonRpcResponse::err(id, error),
                    }
                }
                Err(error) => JsonRpcResponse::err(id, error),
            }
        }
        methods::RELOAD_SESSION_MCP => match parse_params::<rpc::ReloadSessionMcp>(req.params) {
            Ok(params) => {
                let operation_state = state.clone();
                let operation_principal = principal_id.to_owned();
                let operation_params = params.clone();
                let outcome = execute_command::<rpc::ReloadSessionMcp, _>(
                    &state,
                    principal_id,
                    params.request_id.clone(),
                    &params,
                    async move {
                        operation_state
                            .reload_session_mcp(&operation_params.session_id, &operation_principal)
                            .await
                            .map(|commit| ReloadSessionMcpResponse {
                                session_id: operation_params.session_id,
                                active_runs: commit.active_runs,
                                revision: commit.revision,
                                cursor: commit.cursor,
                            })
                            .map_err(|error| JsonRpcError::application(error.to_string()))
                    },
                )
                .await;
                match outcome {
                    Ok(result) => method_response::<rpc::ReloadSessionMcp>(id, result),
                    Err(error) => JsonRpcResponse::err(id, error),
                }
            }
            Err(error) => JsonRpcResponse::err(id, error),
        },
        methods::AUTO_NAME_SESSION => match parse_params::<rpc::AutoNameSession>(req.params) {
            Ok(params) => {
                let operation_state = state.clone();
                let operation_principal = principal_id.to_owned();
                let operation_params = params.clone();
                let outcome = execute_command::<rpc::AutoNameSession, _>(
                    &state,
                    principal_id,
                    params.request_id.clone(),
                    &params,
                    async move {
                        operation_state
                            .auto_name_session(&operation_params.session_id, &operation_principal)
                            .await
                            .map(|commit| AutoNameSessionResponse {
                                session: commit.session,
                                status: commit.status,
                                revision: commit.revision,
                                cursor: commit.cursor,
                            })
                            .map_err(|error| JsonRpcError::application(error.to_string()))
                    },
                )
                .await;
                match outcome {
                    Ok(result) => method_response::<rpc::AutoNameSession>(id, result),
                    Err(error) => JsonRpcResponse::err(id, error),
                }
            }
            Err(error) => JsonRpcResponse::err(id, error),
        },
        methods::MOVE_SESSION => match parse_params::<rpc::MoveSession>(req.params) {
            Ok(params) if !params.project_root.trim().is_empty() => {
                let operation_state = state.clone();
                let operation_principal = principal_id.to_owned();
                let operation_params = params.clone();
                let outcome = execute_command::<rpc::MoveSession, _>(
                    &state,
                    principal_id,
                    params.request_id.clone(),
                    &params,
                    async move {
                        operation_state
                            .move_session(
                                &operation_params.session_id,
                                std::path::Path::new(&operation_params.project_root),
                                &operation_principal,
                            )
                            .await
                            .map(|commit| MoveSessionResponse {
                                session: commit.session,
                                revision: commit.revision,
                                cursor: commit.cursor,
                            })
                            .map_err(|error| JsonRpcError::application(error.to_string()))
                    },
                )
                .await;
                match outcome {
                    Ok(result) => method_response::<rpc::MoveSession>(id, result),
                    Err(error) => JsonRpcResponse::err(id, error),
                }
            }
            Ok(_) => JsonRpcResponse::err(
                id,
                JsonRpcError::invalid_params("project_root must not be empty"),
            ),
            Err(error) => JsonRpcResponse::err(id, error),
        },
        methods::CANCEL_RUN => match parse_params::<rpc::CancelRun>(req.params) {
            Ok(params) => {
                let operation_state = state.clone();
                let operation_principal = principal_id.to_owned();
                let operation_params = params.clone();
                match execute_command::<rpc::CancelRun, _>(
                    &state,
                    principal_id,
                    params.request_id.clone(),
                    &params,
                    async move {
                        operation_state
                            .cancel_run(
                                &operation_params.session_id,
                                &operation_params.run_id,
                                &operation_principal,
                            )
                            .await
                            .map(|commit| CancelRunResponse {
                                cancelled: matches!(
                                    commit.status,
                                    atman_proto::RunCancellationStatus::Accepted
                                        | atman_proto::RunCancellationStatus::AlreadyRequested
                                ),
                                status: commit.status,
                                session_id: operation_params.session_id,
                                run_id: operation_params.run_id,
                                revision: commit.revision,
                                cursor: commit.cursor,
                            })
                            .map_err(|error| JsonRpcError::application(error.to_string()))
                    },
                )
                .await
                {
                    Ok(response) => method_response::<rpc::CancelRun>(id, response),
                    Err(error) => JsonRpcResponse::err(id, error),
                }
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
        methods::LIST_RESOURCES => match parse_params::<rpc::ListResources>(req.params) {
            Ok(params) => match state
                .session_snapshot(&params.session_id, principal_id)
                .await
            {
                Ok(snapshot) => method_response::<rpc::ListResources>(
                    id,
                    ListResourcesResponse {
                        session_id: params.session_id,
                        resources: snapshot.projection.resources,
                        revision: snapshot.projection.revision,
                        cursor: snapshot.cursor,
                    },
                ),
                Err(error) => {
                    JsonRpcResponse::err(id, JsonRpcError::application(error.to_string()))
                }
            },
            Err(error) => JsonRpcResponse::err(id, error),
        },
        methods::INSPECT_RESOURCE => match parse_params::<rpc::InspectResource>(req.params) {
            Ok(params) => match state
                .session_snapshot(&params.session_id, principal_id)
                .await
            {
                Ok(snapshot) => {
                    match snapshot
                        .projection
                        .resources
                        .into_iter()
                        .find(|resource| resource.id == params.resource_id)
                    {
                        Some(resource) => method_response::<rpc::InspectResource>(
                            id,
                            InspectResourceResponse {
                                session_id: params.session_id,
                                resource,
                                revision: snapshot.projection.revision,
                                cursor: snapshot.cursor,
                            },
                        ),
                        None => JsonRpcResponse::err(
                            id,
                            JsonRpcError::application(format!(
                                "resource not found: {}",
                                params.resource_id.0
                            )),
                        ),
                    }
                }
                Err(error) => {
                    JsonRpcResponse::err(id, JsonRpcError::application(error.to_string()))
                }
            },
            Err(error) => JsonRpcResponse::err(id, error),
        },
        methods::TERMINATE_RESOURCE => match parse_params::<rpc::TerminateResource>(req.params) {
            Ok(params) => {
                let operation_state = state.clone();
                let operation_principal = principal_id.to_owned();
                let operation_params = params.clone();
                match execute_command::<rpc::TerminateResource, _>(
                    &state,
                    principal_id,
                    params.request_id.clone(),
                    &params,
                    async move {
                        let commit = operation_state
                            .terminate_resource(
                                &operation_params.session_id,
                                operation_params.resource_id.clone(),
                                &operation_principal,
                            )
                            .await
                            .map_err(|error| JsonRpcError::application(error.to_string()))?;
                        Ok(TerminateResourceResponse {
                            session_id: operation_params.session_id,
                            resource_id: operation_params.resource_id,
                            status: commit.status,
                            revision: commit.revision,
                            cursor: commit.cursor,
                        })
                    },
                )
                .await
                {
                    Ok(response) => method_response::<rpc::TerminateResource>(id, response),
                    Err(error) => JsonRpcResponse::err(id, error),
                }
            }
            Err(error) => JsonRpcResponse::err(id, error),
        },
        methods::RESIZE_TERMINAL_RESOURCE => {
            match parse_params::<rpc::ResizeTerminalResource>(req.params) {
                Ok(params) => {
                    let operation_state = state.clone();
                    let operation_principal = principal_id.to_owned();
                    let operation_params = params.clone();
                    match execute_command::<rpc::ResizeTerminalResource, _>(
                        &state,
                        principal_id,
                        params.request_id.clone(),
                        &params,
                        async move {
                            let commit = operation_state
                                .resize_terminal(
                                    &operation_params.session_id,
                                    operation_params.resource_id.clone(),
                                    operation_params.rows,
                                    operation_params.cols,
                                    &operation_principal,
                                )
                                .await
                                .map_err(|error| JsonRpcError::application(error.to_string()))?;
                            Ok(ResizeTerminalResourceResponse {
                                session_id: operation_params.session_id,
                                resource_id: operation_params.resource_id,
                                rows: operation_params.rows,
                                cols: operation_params.cols,
                                status: commit.status,
                                revision: commit.revision,
                                cursor: commit.cursor,
                            })
                        },
                    )
                    .await
                    {
                        Ok(response) => {
                            method_response::<rpc::ResizeTerminalResource>(id, response)
                        }
                        Err(error) => JsonRpcResponse::err(id, error),
                    }
                }
                Err(error) => JsonRpcResponse::err(id, error),
            }
        }
        methods::RETAIN_RESOURCE => match parse_params::<rpc::RetainResource>(req.params) {
            Ok(params) => {
                let operation_state = state.clone();
                let operation_principal = principal_id.to_owned();
                let operation_params = params.clone();
                match execute_command::<rpc::RetainResource, _>(
                    &state,
                    principal_id,
                    params.request_id.clone(),
                    &params,
                    async move {
                        let commit = operation_state
                            .retain_resource(
                                &operation_params.session_id,
                                operation_params.resource_id,
                                &operation_principal,
                            )
                            .await
                            .map_err(|error| JsonRpcError::application(error.to_string()))?;
                        Ok(RetainResourceResponse {
                            session_id: operation_params.session_id,
                            resource: commit.resource,
                            revision: commit.revision,
                            cursor: commit.cursor,
                        })
                    },
                )
                .await
                {
                    Ok(response) => method_response::<rpc::RetainResource>(id, response),
                    Err(error) => JsonRpcResponse::err(id, error),
                }
            }
            Err(error) => JsonRpcResponse::err(id, error),
        },
        methods::RELEASE_RESOURCE => match parse_params::<rpc::ReleaseResource>(req.params) {
            Ok(params) => {
                let operation_state = state.clone();
                let operation_principal = principal_id.to_owned();
                let operation_params = params.clone();
                match execute_command::<rpc::ReleaseResource, _>(
                    &state,
                    principal_id,
                    params.request_id.clone(),
                    &params,
                    async move {
                        let commit = operation_state
                            .release_resource(
                                &operation_params.session_id,
                                operation_params.resource_id,
                                &operation_principal,
                            )
                            .await
                            .map_err(|error| JsonRpcError::application(error.to_string()))?;
                        Ok(ReleaseResourceResponse {
                            session_id: operation_params.session_id,
                            resource: commit.resource,
                            revision: commit.revision,
                            cursor: commit.cursor,
                        })
                    },
                )
                .await
                {
                    Ok(response) => method_response::<rpc::ReleaseResource>(id, response),
                    Err(error) => JsonRpcResponse::err(id, error),
                }
            }
            Err(error) => JsonRpcResponse::err(id, error),
        },
        methods::RESOLVE_PROMPT => match parse_params::<rpc::ResolvePrompt>(req.params) {
            Ok(params) => {
                let operation_state = state.clone();
                let operation_principal = principal_id.to_owned();
                let operation_params = params.clone();
                match execute_command::<rpc::ResolvePrompt, _>(
                    &state,
                    principal_id,
                    params.request_id.clone(),
                    &params,
                    async move {
                        let commit = operation_state
                            .resolve_prompt(
                                &operation_params.session_id,
                                operation_params.prompt_id.clone(),
                                operation_params.answer,
                                &operation_principal,
                            )
                            .await
                            .map_err(|error| JsonRpcError::application(error.to_string()))?;
                        Ok(ResolvePromptResponse {
                            resolved: commit.status
                                == atman_proto::PromptResolutionStatus::Resolved,
                            status: commit.status,
                            session_id: operation_params.session_id,
                            prompt_id: operation_params.prompt_id,
                            revision: commit.revision,
                            cursor: commit.cursor,
                        })
                    },
                )
                .await
                {
                    Ok(response) => method_response::<rpc::ResolvePrompt>(id, response),
                    Err(error) => JsonRpcResponse::err(id, error),
                }
            }
            Err(error) => JsonRpcResponse::err(id, error),
        },
        methods::SUBMIT_FORM => match parse_params::<rpc::SubmitForm>(req.params) {
            Ok(params) => {
                let operation_state = state.clone();
                let operation_principal = principal_id.to_owned();
                let operation_params = params.clone();
                match execute_command::<rpc::SubmitForm, _>(
                    &state,
                    principal_id,
                    params.request_id.clone(),
                    &params,
                    async move {
                        let commit = operation_state
                            .submit_form(
                                &operation_params.session_id,
                                operation_params.form_id.clone(),
                                operation_params.submission,
                                &operation_principal,
                            )
                            .await
                            .map_err(|error| JsonRpcError::application(error.to_string()))?;
                        Ok(SubmitFormResponse {
                            resolved: commit.status == atman_proto::FormResolutionStatus::Resolved,
                            status: commit.status,
                            session_id: operation_params.session_id,
                            form_id: operation_params.form_id,
                            revision: commit.revision,
                            cursor: commit.cursor,
                        })
                    },
                )
                .await
                {
                    Ok(response) => method_response::<rpc::SubmitForm>(id, response),
                    Err(error) => JsonRpcResponse::err(id, error),
                }
            }
            Err(error) => JsonRpcResponse::err(id, error),
        },
        methods::COMPACT_SESSION => match parse_params::<rpc::CompactSession>(req.params) {
            Ok(params) => {
                let operation_state = state.clone();
                let operation_principal = principal_id.to_owned();
                let operation_params = params.clone();
                match execute_command::<rpc::CompactSession, _>(
                    &state,
                    principal_id,
                    params.request_id.clone(),
                    &params,
                    async move {
                        let commit = operation_state
                            .request_session_compaction(
                                &operation_params.session_id,
                                &operation_principal,
                            )
                            .await
                            .map_err(|error| JsonRpcError::application(error.to_string()))?;
                        Ok(CompactSessionResponse {
                            session_id: operation_params.session_id,
                            status: commit.status,
                            operation_id: commit.operation_id,
                            revision: commit.revision,
                            cursor: commit.cursor,
                        })
                    },
                )
                .await
                {
                    Ok(response) => method_response::<rpc::CompactSession>(id, response),
                    Err(error) => JsonRpcResponse::err(id, error),
                }
            }
            Err(error) => JsonRpcResponse::err(id, error),
        },
        methods::RESOLVE_COMPACT_REVIEW => {
            match parse_params::<rpc::ResolveCompactReview>(req.params) {
                Ok(params) => {
                    let operation_state = state.clone();
                    let operation_principal = principal_id.to_owned();
                    let operation_params = params.clone();
                    match execute_command::<rpc::ResolveCompactReview, _>(
                        &state,
                        principal_id,
                        params.request_id.clone(),
                        &params,
                        async move {
                            let commit = operation_state
                                .resolve_compact_review(
                                    &operation_params.session_id,
                                    operation_params.review_id.clone(),
                                    operation_params.decision,
                                    &operation_principal,
                                )
                                .await
                                .map_err(|error| JsonRpcError::application(error.to_string()))?;
                            Ok(ResolveCompactReviewResponse {
                                resolved: commit.status
                                    == atman_proto::CompactReviewResolutionStatus::Resolved,
                                status: commit.status,
                                session_id: operation_params.session_id,
                                review_id: operation_params.review_id,
                                revision: commit.revision,
                                cursor: commit.cursor,
                            })
                        },
                    )
                    .await
                    {
                        Ok(response) => method_response::<rpc::ResolveCompactReview>(id, response),
                        Err(error) => JsonRpcResponse::err(id, error),
                    }
                }
                Err(error) => JsonRpcResponse::err(id, error),
            }
        }
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
                Ok(params) => {
                    let operation_state = state.clone();
                    let operation_principal = principal_id.to_owned();
                    let operation_params = params.clone();
                    match execute_command::<rpc::CreatePermissionGroup, _>(
                        &state,
                        principal_id,
                        params.request_id.clone(),
                        &params,
                        async move {
                            operation_state
                                .create_permission_group(operation_params, &operation_principal)
                                .await
                                .map_err(|error| JsonRpcError::application(error.to_string()))
                        },
                    )
                    .await
                    {
                        Ok(response) => method_response::<rpc::CreatePermissionGroup>(id, response),
                        Err(error) => JsonRpcResponse::err(id, error),
                    }
                }
                Err(error) => JsonRpcResponse::err(id, error),
            }
        }
        methods::RESOLVE_PERMISSION_REQUESTS => {
            match parse_params::<rpc::ResolvePermissionRequests>(req.params) {
                Ok(params) => {
                    let action = permission_action(params.action.clone());
                    let scope = permission_scope(params.scope.clone());
                    let operation_state = state.clone();
                    let operation_principal = principal_id.to_owned();
                    let operation_params = params.clone();
                    match execute_command::<rpc::ResolvePermissionRequests, _>(
                        &state,
                        principal_id,
                        params.request_id.clone(),
                        &params,
                        async move {
                            operation_state
                                .resolve_permission_requests(
                                    operation_params,
                                    &operation_principal,
                                    action,
                                    scope,
                                )
                                .await
                                .map_err(|error| JsonRpcError::application(error.to_string()))
                        },
                    )
                    .await
                    {
                        Ok(response) => {
                            method_response::<rpc::ResolvePermissionRequests>(id, response)
                        }
                        Err(error) => JsonRpcResponse::err(id, error),
                    }
                }
                Err(error) => JsonRpcResponse::err(id, error),
            }
        }
        methods::START_RUN => {
            let Some(launcher) = state.launcher() else {
                return JsonRpcResponse::err(
                    id,
                    JsonRpcError::application("daemon started without a run launcher"),
                );
            };
            match parse_params::<rpc::StartRun>(req.params) {
                Ok(params) => {
                    let operation_state = state.clone();
                    let operation_principal = principal_id.to_owned();
                    let operation_params = params.clone();
                    let outcome = execute_command::<rpc::StartRun, _>(
                        &state,
                        principal_id,
                        params.request_id.clone(),
                        &params,
                        async move {
                            let spawned = launcher
                                .start_in_session_as_with_options(
                                    operation_state.clone(),
                                    &operation_params.session_id,
                                    &operation_params.flow_path,
                                    runtime_args(operation_params.args),
                                    &operation_principal,
                                    crate::run::RunOptions {
                                        reasoning: operation_params.reasoning,
                                        images: operation_params.images,
                                        flow_name: operation_params.flow_name,
                                        ..Default::default()
                                    },
                                )
                                .await
                                .map_err(|error| JsonRpcError::application(error.to_string()))?;
                            let snapshot = operation_state
                                .session_snapshot(&spawned.session_id, &operation_principal)
                                .await
                                .map_err(|error| JsonRpcError::application(error.to_string()))?;
                            Ok(StartRunResponse {
                                session_id: spawned.session_id,
                                run_id: spawned.run_id,
                                revision: snapshot.projection.revision,
                                cursor: snapshot.cursor,
                            })
                        },
                    )
                    .await;
                    match outcome {
                        Ok(response) => method_response::<rpc::StartRun>(id, response),
                        Err(error) => JsonRpcResponse::err(id, error),
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
                Ok(params) => {
                    let operation_state = state.clone();
                    let operation_principal = principal_id.to_owned();
                    let operation_params = params.clone();
                    let outcome = execute_command::<rpc::RunFlow, _>(
                        &state,
                        principal_id,
                        params.request_id.clone(),
                        &params,
                        async move {
                            let args = runtime_args(operation_params.args);
                            let spawned = launcher
                                .spawn_as_with_options(
                                    operation_state.clone(),
                                    &operation_params.flow_path,
                                    args,
                                    &operation_principal,
                                    crate::run::RunOptions {
                                        reasoning: operation_params.reasoning,
                                        images: operation_params.images,
                                        flow_name: operation_params.flow_name,
                                        project_root: operation_params.project_root,
                                        ..Default::default()
                                    },
                                )
                                .await
                                .map_err(|error| JsonRpcError::application(error.to_string()))?;
                            let snapshot = operation_state
                                .session_snapshot(&spawned.session_id, &operation_principal)
                                .await
                                .map_err(|error| JsonRpcError::application(error.to_string()))?;
                            Ok(RunFlowResponse {
                                session_id: spawned.session_id,
                                run_id: spawned.run_id,
                                revision: snapshot.projection.revision,
                                cursor: snapshot.cursor,
                            })
                        },
                    )
                    .await;
                    match outcome {
                        Ok(response) => method_response::<rpc::RunFlow>(id, response),
                        Err(error) => JsonRpcResponse::err(id, error),
                    }
                }
                Err(error) => JsonRpcResponse::err(id, error),
            }
        }
        other => JsonRpcResponse::err(id, JsonRpcError::method_not_found(other)),
    }
}
