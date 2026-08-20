use std::sync::Arc;

use atman_proto::{
    CancelRunRequest, JsonRpcError, JsonRpcRequest, JsonRpcResponse, ResolvePromptRequest,
    RunFlowRequest, RunFlowResponse, methods,
};
use serde_json::json;

pub mod bootstrap;
pub mod config;
pub mod http;
pub mod openapi;
pub mod pidfile;
pub mod prompt_bridge;
pub mod run;
pub mod state;
pub mod unix;

pub use state::{DaemonState, LiveSession};

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
                    let resolved = state.resolve_prompt(&p.prompt_id, p.answer);
                    JsonRpcResponse::ok(id, json!({"resolved": resolved}))
                }
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
                    match launcher.spawn(state.clone(), &p.flow_path, args).await {
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
