use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const JSONRPC_VERSION: &str = "2.0";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct SessionId(pub Uuid);

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct FlowRunId(pub Uuid);

impl std::fmt::Display for FlowRunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct PromptId(pub Uuid);

impl std::fmt::Display for PromptId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<serde_json::Value>,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcRequest {
    pub fn new(
        id: impl Into<serde_json::Value>,
        method: impl Into<String>,
        params: serde_json::Value,
    ) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id: Some(id.into()),
            method: method.into(),
            params: Some(params),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    pub fn ok(id: Option<serde_json::Value>, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Option<serde_json::Value>, error: JsonRpcError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, thiserror::Error)]
#[error("json-rpc error {code}: {message}")]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl JsonRpcError {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;
    pub const APPLICATION_ERROR: i32 = -32000;

    pub fn parse_error(msg: impl Into<String>) -> Self {
        Self {
            code: Self::PARSE_ERROR,
            message: msg.into(),
            data: None,
        }
    }

    pub fn method_not_found(method: &str) -> Self {
        Self {
            code: Self::METHOD_NOT_FOUND,
            message: format!("method not found: {method}"),
            data: None,
        }
    }

    pub fn invalid_params(msg: impl Into<String>) -> Self {
        Self {
            code: Self::INVALID_PARAMS,
            message: msg.into(),
            data: None,
        }
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            code: Self::INTERNAL_ERROR,
            message: msg.into(),
            data: None,
        }
    }

    pub fn application(msg: impl Into<String>) -> Self {
        Self {
            code: Self::APPLICATION_ERROR,
            message: msg.into(),
            data: None,
        }
    }
}

pub mod methods {
    pub const RUN_FLOW: &str = "run_flow";
    pub const CANCEL_RUN: &str = "cancel_run";
    pub const LIST_SESSIONS: &str = "list_sessions";
    pub const GET_EVENTS: &str = "get_events";
    pub const RESOLVE_PROMPT: &str = "resolve_prompt";
    pub const PING: &str = "ping";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunFlowRequest {
    pub flow_path: String,
    #[serde(default)]
    pub args: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunFlowResponse {
    pub session_id: SessionId,
    pub run_id: FlowRunId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelRunRequest {
    pub run_id: FlowRunId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: SessionId,
    pub event_count: usize,
    pub first_ts: Option<chrono::DateTime<chrono::Utc>>,
    pub status: SessionStatus,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Running,
    Finished,
    Pending,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetEventsRequest {
    pub session_id: SessionId,
    #[serde(default)]
    pub since_seq: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvePromptRequest {
    pub prompt_id: PromptId,
    pub answer: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_rpc_request_round_trip() {
        let req = JsonRpcRequest::new(1, "run_flow", serde_json::json!({"flow_path": "x.at"}));
        let s = serde_json::to_string(&req).unwrap();
        let back: JsonRpcRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(back.method, "run_flow");
        assert_eq!(back.jsonrpc, "2.0");
    }

    #[test]
    fn json_rpc_error_response_shape() {
        let resp = JsonRpcResponse::err(
            Some(serde_json::json!(7)),
            JsonRpcError::method_not_found("foo"),
        );
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains("\"code\":-32601"));
        assert!(s.contains("\"id\":7"));
        assert!(!s.contains("\"result\""));
    }

    #[test]
    fn run_flow_request_deserialize_without_args() {
        let s = r#"{"flow_path":"examples/hello.at"}"#;
        let req: RunFlowRequest = serde_json::from_str(s).unwrap();
        assert_eq!(req.flow_path, "examples/hello.at");
        assert!(req.args.is_empty());
    }

    #[test]
    fn session_status_snake_case() {
        assert_eq!(
            serde_json::to_string(&SessionStatus::Running).unwrap(),
            "\"running\""
        );
    }
}
