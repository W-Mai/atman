use crate::value::ValueJson;
use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;

use crate::error::RuntimeError;
use crate::tool::BoxFut;

type PendingCalls = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<serde_json::Value, McpError>>>>>;

pub trait McpTransport: Send + Sync {
    fn call<'a>(
        &'a self,
        method: &'a str,
        params: serde_json::Value,
    ) -> BoxFut<'a, Result<serde_json::Value, McpError>>;

    /// Send a JSON-RPC *notification* — no `id`, no response expected.
    fn notify<'a>(
        &'a self,
        method: &'a str,
        params: serde_json::Value,
    ) -> BoxFut<'a, Result<(), McpError>>;

    fn kind(&self) -> &'static str;

    fn initial_protocol_version(&self) -> &'static str {
        "2024-11-05"
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct JsonRpcRequest {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct JsonRpcResponse {
    #[allow(dead_code)]
    pub jsonrpc: String,
    pub id: Option<u64>,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default)]
    pub data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpToolSchema {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<McpToolAnnotations>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct McpToolAnnotations {
    #[serde(default, rename = "readOnlyHint")]
    pub read_only_hint: Option<bool>,
    #[serde(default, rename = "destructiveHint")]
    pub destructive_hint: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct McpToolSnapshot {
    pub fingerprint: String,
    pub tools: Arc<[McpToolSchema]>,
}

impl McpToolSnapshot {
    fn build(mut tools: Vec<McpToolSchema>) -> Result<Self, McpError> {
        for tool in &mut tools {
            if let Some(schema) = &mut tool.input_schema {
                canonicalize_json(schema);
            }
        }
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        if let Some(duplicate) = tools
            .windows(2)
            .find(|pair| pair[0].name == pair[1].name)
            .map(|pair| pair[0].name.clone())
        {
            return Err(McpError::Protocol(format!(
                "tools/list returned duplicate tool `{duplicate}`"
            )));
        }
        let bytes = serde_json::to_vec(&tools)
            .map_err(|error| McpError::Protocol(format!("serialize tool snapshot: {error}")))?;
        Ok(Self {
            fingerprint: format!("blake3:{}", blake3::hash(&bytes).to_hex()),
            tools: tools.into(),
        })
    }
}

fn canonicalize_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            let mut entries: Vec<_> = std::mem::take(object).into_iter().collect();
            for (_, value) in &mut entries {
                canonicalize_json(value);
            }
            entries.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
            object.extend(entries);
        }
        serde_json::Value::Array(items) => {
            for item in items {
                canonicalize_json(item);
            }
        }
        _ => {}
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct McpResource {
    pub uri: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "mimeType")]
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct McpResourceContent {
    pub uri: String,
    #[serde(default, rename = "mimeType")]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub blob: Option<String>, // base64-encoded
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct McpPrompt {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub arguments: Vec<McpPromptArg>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct McpPromptArg {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct McpPromptMessage {
    pub role: String,
    pub content: serde_json::Value,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct McpPromptContent {
    #[serde(default)]
    pub messages: Vec<McpPromptMessage>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct SamplingRequest {
    #[serde(default)]
    pub model: Option<String>,
    pub messages: Vec<McpPromptMessage>,
    #[serde(default)]
    pub max_tokens: u32,
    #[serde(default, rename = "systemPrompt")]
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct SamplingResponse {
    pub role: String,
    pub content: serde_json::Value,
    #[serde(default)]
    pub model: Option<String>,
}

pub type SamplingHandler = Arc<
    dyn Fn(
            SamplingRequest,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<SamplingResponse, RuntimeError>> + Send>,
        > + Send
        + Sync,
>;

pub type SharedSamplingHandler = Arc<std::sync::Mutex<Option<SamplingHandler>>>;

#[derive(Debug, Clone, thiserror::Error)]
pub enum McpError {
    #[error("mcp io: {0}")]
    Io(String),
    #[error("mcp protocol: {0}")]
    Protocol(String),
    #[error("mcp server error {code}: {message}")]
    ServerError { code: i64, message: String },
    #[error("mcp timeout ({timeout_ms}ms) on {method}")]
    Timeout { timeout_ms: u64, method: String },
    #[error("mcp disconnected")]
    Disconnected,
}

impl From<McpError> for RuntimeError {
    fn from(e: McpError) -> Self {
        RuntimeError::ToolFailed(format!("{e}"))
    }
}

pub struct McpStdioTransport {
    stdin: Arc<Mutex<ChildStdin>>,
    pending: PendingCalls,
    next_id: Arc<Mutex<u64>>,
    #[allow(dead_code)]
    child: Arc<Mutex<Child>>,
    #[allow(dead_code)]
    reader_task: Arc<Mutex<Option<JoinHandle<()>>>>,
    timeout_ms: u64,
    #[allow(dead_code)]
    sampling_handler: SharedSamplingHandler,
}

impl McpTransport for McpStdioTransport {
    fn call<'a>(
        &'a self,
        method: &'a str,
        params: serde_json::Value,
    ) -> BoxFut<'a, Result<serde_json::Value, McpError>> {
        Box::pin(self.call_stdio(method, params))
    }

    fn notify<'a>(
        &'a self,
        method: &'a str,
        params: serde_json::Value,
    ) -> BoxFut<'a, Result<(), McpError>> {
        Box::pin(self.notify_stdio(method, params))
    }

    fn kind(&self) -> &'static str {
        "stdio"
    }
}

impl McpStdioTransport {
    pub async fn spawn(
        cmd: &str,
        args: &[String],
        env: &[(String, String)],
        timeout_ms: u64,
        notification_tx: tokio::sync::broadcast::Sender<McpNotification>,
        sampling_handler: SharedSamplingHandler,
    ) -> Result<Self, McpError> {
        let mut command = Command::new(cmd);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            command.env(k, v);
        }
        let mut child = command
            .spawn()
            .map_err(|e| McpError::Io(format!("spawn {cmd}: {e}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Io("no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Io("no stdout".into()))?;

        let stdin_arc = Arc::new(tokio::sync::Mutex::new(stdin));
        let stdin_for_reader = stdin_arc.clone();
        let pending: PendingCalls = Arc::new(Mutex::new(HashMap::new()));
        let pending_for_reader = pending.clone();
        let notif_for_reader = notification_tx.clone();
        let sampling_for_reader = sampling_handler.clone();
        let reader_task = tokio::spawn(async move {
            reader_loop(
                stdout,
                pending_for_reader,
                notif_for_reader,
                stdin_for_reader,
                sampling_for_reader,
            )
            .await;
        });

        Ok(Self {
            stdin: stdin_arc,
            pending,
            next_id: Arc::new(Mutex::new(1)),
            child: Arc::new(Mutex::new(child)),
            reader_task: Arc::new(Mutex::new(Some(reader_task))),
            timeout_ms,
            sampling_handler,
        })
    }

    async fn call_stdio(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        let id = {
            let mut n = self.next_id.lock().await;
            let v = *n;
            *n += 1;
            v
        };
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method: method.into(),
            params: Some(params),
        };
        let line = serde_json::to_string(&req)
            .map_err(|e| McpError::Protocol(format!("serialize: {e}")))?;
        {
            let mut stdin = self.stdin.lock().await;
            stdin
                .write_all(line.as_bytes())
                .await
                .map_err(|e| McpError::Io(format!("write: {e}")))?;
            stdin
                .write_all(b"\n")
                .await
                .map_err(|e| McpError::Io(format!("write newline: {e}")))?;
            stdin
                .flush()
                .await
                .map_err(|e| McpError::Io(format!("flush: {e}")))?;
        }

        match tokio::time::timeout(Duration::from_millis(self.timeout_ms), rx).await {
            Ok(Ok(inner)) => inner,
            Ok(Err(_)) => {
                self.pending.lock().await.remove(&id);
                Err(McpError::Disconnected)
            }
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(McpError::Timeout {
                    timeout_ms: self.timeout_ms,
                    method: method.into(),
                })
            }
        }
    }

    async fn notify_stdio(&self, method: &str, params: serde_json::Value) -> Result<(), McpError> {
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let line = serde_json::to_string(&req)
            .map_err(|e| McpError::Protocol(format!("serialize: {e}")))?;
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| McpError::Io(format!("write: {e}")))?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|e| McpError::Io(format!("write newline: {e}")))?;
        stdin
            .flush()
            .await
            .map_err(|e| McpError::Io(format!("flush: {e}")))?;
        Ok(())
    }
}

async fn reader_loop(
    stdout: ChildStdout,
    pending: PendingCalls,
    notification_tx: tokio::sync::broadcast::Sender<McpNotification>,
    stdin: Arc<tokio::sync::Mutex<ChildStdin>>,
    sampling_handler: SharedSamplingHandler,
) {
    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                let parsed: serde_json::Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                if let Some(method) = parsed.get("method").and_then(|v| v.as_str()) {
                    if parsed.get("id").is_some() {
                        if method == "sampling/createMessage" {
                            let id = parsed.get("id").cloned();
                            let params = parsed
                                .get("params")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null);
                            let handler_arc = sampling_handler.clone();
                            let stdin_arc = stdin.clone();
                            tokio::spawn(async move {
                                let handler = handler_arc.lock().unwrap().clone();
                                let result = if let Some(h) = handler {
                                    match serde_json::from_value::<SamplingRequest>(params.clone())
                                    {
                                        Ok(req) => match h(req).await {
                                            Ok(resp) => Ok(serde_json::to_value(resp)
                                                .unwrap_or(serde_json::Value::Null)),
                                            Err(e) => Err(serde_json::json!({
                                                "code": -1,
                                                "message": e.to_string()
                                            })),
                                        },
                                        Err(e) => Err(serde_json::json!({
                                            "code": -1,
                                            "message": format!("parse error: {e}")
                                        })),
                                    }
                                } else {
                                    Err(serde_json::json!({
                                        "code": -1,
                                        "message": "sampling not supported"
                                    }))
                                };
                                let response = match result {
                                    Ok(r) => serde_json::json!({
                                        "jsonrpc": "2.0",
                                        "id": id,
                                        "result": r
                                    }),
                                    Err(e) => serde_json::json!({
                                        "jsonrpc": "2.0",
                                        "id": id,
                                        "error": e
                                    }),
                                };
                                let line = serde_json::to_string(&response)
                                    .unwrap_or_else(|_| "{}".into());
                                let mut s = stdin_arc.lock().await;
                                let _ = s.write_all(line.as_bytes()).await;
                                let _ = s.write_all(b"\n").await;
                                let _ = s.flush().await;
                            });
                        }
                    } else {
                        let notif = parse_notification(method, &parsed);
                        let _ = notification_tx.send(notif);
                    }
                    continue;
                }

                let Some(id) = parsed.get("id").and_then(|v| v.as_u64()) else {
                    continue;
                };
                let sender = pending.lock().await.remove(&id);
                if let Some(sender) = sender {
                    let outcome = if let Some(err) = parsed.get("error") {
                        Err(McpError::ServerError {
                            code: err.get("code").and_then(|v| v.as_i64()).unwrap_or(-1),
                            message: err
                                .get("message")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown")
                                .to_string(),
                        })
                    } else {
                        Ok(parsed
                            .get("result")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null))
                    };
                    let _ = sender.send(outcome);
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    let mut pending = pending.lock().await;
    for (_, tx) in pending.drain() {
        let _ = tx.send(Err(McpError::Disconnected));
    }
}

fn parse_http_message(
    parsed: serde_json::Value,
    expected_id: u64,
    allow_other_messages: bool,
    notification_tx: &tokio::sync::broadcast::Sender<McpNotification>,
) -> Result<Option<serde_json::Value>, McpError> {
    if parsed.get("jsonrpc").and_then(|value| value.as_str()) != Some("2.0") {
        return Err(McpError::Protocol("invalid JSON-RPC version".into()));
    }
    if let Some(method) = parsed.get("method") {
        let method = method
            .as_str()
            .ok_or_else(|| McpError::Protocol("invalid JSON-RPC method".into()))?;
        if parsed.get("result").is_some()
            || parsed.get("error").is_some()
            || parsed
                .get("params")
                .is_some_and(|params| !params.is_object() && !params.is_array())
            || parsed
                .get("id")
                .is_some_and(|id| !id.is_string() && id.as_i64().is_none() && id.as_u64().is_none())
        {
            return Err(McpError::Protocol(
                "invalid JSON-RPC request envelope".into(),
            ));
        }
        if allow_other_messages {
            if parsed.get("id").is_none() {
                let _ = notification_tx.send(parse_notification(method, &parsed));
            }
            return Ok(None);
        }
    } else if parsed.get("id").and_then(|value| value.as_u64()) == Some(expected_id) {
        return match (parsed.get("result"), parsed.get("error")) {
            (Some(result), None) => Ok(Some(result.clone())),
            (None, Some(error)) => {
                let error: JsonRpcError =
                    serde_json::from_value(error.clone()).map_err(|error| {
                        McpError::Protocol(format!("invalid JSON-RPC error: {error}"))
                    })?;
                Err(McpError::ServerError {
                    code: error.code,
                    message: error.message,
                })
            }
            _ => Err(McpError::Protocol(
                "JSON-RPC response must contain exactly one of result or error".into(),
            )),
        };
    } else if allow_other_messages && parsed.get("id").is_some() {
        return Ok(None);
    }
    Err(McpError::Protocol(format!(
        "JSON-RPC response does not match request id {expected_id}"
    )))
}

fn configured_http_headers(headers: &[(String, String)]) -> Result<HeaderMap, McpError> {
    let mut parsed = HeaderMap::new();
    let mut accepted = vec![
        "application/json".to_string(),
        "text/event-stream".to_string(),
    ];
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| McpError::Protocol(format!("invalid MCP HTTP header name `{name}`")))?;
        let mut value = HeaderValue::from_str(value)
            .map_err(|_| McpError::Protocol(format!("invalid MCP HTTP header `{name}`")))?;
        if name == ACCEPT {
            let value = value
                .to_str()
                .map_err(|_| McpError::Protocol("invalid MCP HTTP header `accept`".into()))?;
            for media in value
                .split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
            {
                let media_type = media.split(';').next().unwrap_or_default().trim();
                if !media_type.eq_ignore_ascii_case("application/json")
                    && !media_type.eq_ignore_ascii_case("text/event-stream")
                    && !accepted
                        .iter()
                        .any(|existing| existing.eq_ignore_ascii_case(media))
                {
                    accepted.push(media.to_string());
                }
            }
        } else {
            if name == AUTHORIZATION {
                value.set_sensitive(true);
            }
            parsed.insert(name, value);
        }
    }
    parsed.insert(
        ACCEPT,
        HeaderValue::from_str(&accepted.join(", "))
            .map_err(|_| McpError::Protocol("invalid MCP HTTP header `accept`".into()))?,
    );
    parsed.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    parsed.remove("mcp-session-id");
    parsed.remove("mcp-protocol-version");
    Ok(parsed)
}

fn parse_notification(method: &str, parsed: &serde_json::Value) -> McpNotification {
    let params = parsed
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    match method {
        "notifications/tools/list_changed" => McpNotification::ToolsListChanged,
        "notifications/resources/list_changed" => McpNotification::ResourcesListChanged,
        "notifications/prompts/list_changed" => McpNotification::PromptsListChanged,
        "notifications/progress" => McpNotification::Progress {
            progress_token: params
                .get("progressToken")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            progress: params.get("progress").and_then(|v| v.as_f64()),
            total: params.get("total").and_then(|v| v.as_f64()),
            message: params
                .get("message")
                .and_then(|v| v.as_str())
                .map(String::from),
        },
        "notifications/log" => McpNotification::Log {
            level: params
                .get("level")
                .and_then(|v| v.as_str())
                .unwrap_or("info")
                .to_string(),
            data: params
                .get("data")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        },
        "notifications/cancelled" => McpNotification::Cancelled {
            request_id: params
                .get("requestId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            reason: params
                .get("reason")
                .and_then(|v| v.as_str())
                .map(String::from),
        },
        _ => McpNotification::Other {
            method: method.to_string(),
            params,
        },
    }
}

pub struct McpHttpTransport {
    url: String,
    auth_token: Option<String>,
    headers: HeaderMap,
    session: std::sync::Mutex<HttpSession>,
    client: reqwest::Client,
    next_id: AtomicU64,
    timeout_ms: u64,
    notification_tx: tokio::sync::broadcast::Sender<McpNotification>,
}

#[derive(Clone, Default)]
struct HttpSession {
    id: Option<HeaderValue>,
    protocol_version: Option<HeaderValue>,
}

impl McpHttpTransport {
    pub fn new(
        url: impl Into<String>,
        auth_token: Option<String>,
        timeout_ms: u64,
        notification_tx: tokio::sync::broadcast::Sender<McpNotification>,
    ) -> Self {
        Self {
            url: url.into(),
            auth_token,
            headers: configured_http_headers(&[]).expect("static MCP HTTP headers"),
            session: std::sync::Mutex::new(HttpSession::default()),
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("build MCP HTTP client"),
            next_id: AtomicU64::new(1),
            timeout_ms,
            notification_tx,
        }
    }

    pub fn with_headers(mut self, headers: &[(String, String)]) -> Result<Self, McpError> {
        self.headers = configured_http_headers(headers)?;
        Ok(self)
    }

    #[doc(hidden)]
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    fn post(&self, body: &serde_json::Value) -> Result<(reqwest::RequestBuilder, bool), McpError> {
        let session = self.session.lock().unwrap().clone();
        let mut headers = self.headers.clone();
        if let Some(token) = &self.auth_token {
            let mut authorization =
                HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
                    McpError::Protocol("invalid MCP HTTP header `authorization`".into())
                })?;
            authorization.set_sensitive(true);
            headers.insert(AUTHORIZATION, authorization);
        }
        if let Some(id) = &session.id {
            headers.insert("mcp-session-id", id.clone());
        }
        if let Some(version) = session.protocol_version {
            headers.insert("mcp-protocol-version", version);
        }
        Ok((
            self.client.post(&self.url).headers(headers).json(body),
            session.id.is_some(),
        ))
    }

    async fn send(&self, body: &serde_json::Value) -> Result<reqwest::Response, McpError> {
        let (request, had_session) = self.post(body)?;
        let response = request
            .send()
            .await
            .map_err(|error| McpError::Io(format!("mcp http send: {error}")))?;
        let status = response.status();
        // A rejected reply to a server request does not make the original tool call safe to replay.
        if status == reqwest::StatusCode::NOT_FOUND && had_session && body.get("method").is_some() {
            return Err(McpError::Disconnected);
        }
        if !status.is_success() {
            let body = response
                .text()
                .await
                .map_err(|error| McpError::Io(format!("mcp http {status} body: {error}")))?;
            return Err(McpError::Io(format!("mcp http {status}: {body}")));
        }
        Ok(response)
    }

    async fn reply_to_request(&self, method: &str, id: serde_json::Value) -> Result<(), McpError> {
        let response = if method == "ping" {
            serde_json::json!({"jsonrpc":"2.0", "id":id, "result":{}})
        } else {
            serde_json::json!({
                "jsonrpc":"2.0", "id":id,
                "error":{"code":-32601, "message":"Method not found"}
            })
        };
        self.send(&response).await?;
        Ok(())
    }

    async fn read_response(
        &self,
        response: reqwest::Response,
        expected_id: u64,
    ) -> Result<serde_json::Value, McpError> {
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .split(';')
            .next()
            .unwrap_or_default()
            .trim();
        if content_type.eq_ignore_ascii_case("application/json") {
            let parsed = response
                .json()
                .await
                .map_err(|error| McpError::Protocol(format!("mcp http parse: {error}")))?;
            return parse_http_message(parsed, expected_id, false, &self.notification_tx)?
                .ok_or_else(|| McpError::Protocol("missing JSON-RPC response".into()));
        }
        if !content_type.eq_ignore_ascii_case("text/event-stream") {
            return Err(McpError::Protocol(format!(
                "unsupported MCP HTTP response content type `{content_type}`"
            )));
        }
        let mut events = response.bytes_stream().eventsource();
        while let Some(event) = events.next().await {
            let event =
                event.map_err(|error| McpError::Protocol(format!("mcp http SSE: {error}")))?;
            if event.data.is_empty() {
                continue;
            }
            let parsed = serde_json::from_str(&event.data)
                .map_err(|error| McpError::Protocol(format!("mcp http SSE parse: {error}")))?;
            let messages = match parsed {
                serde_json::Value::Array(messages) if !messages.is_empty() => messages,
                message => vec![message],
            };
            let mut result = None;
            for message in messages {
                let server_request = message
                    .get("method")
                    .and_then(|method| method.as_str())
                    .zip(message.get("id"))
                    .map(|(method, id)| (method.to_string(), id.clone()));
                let outcome =
                    match parse_http_message(message, expected_id, true, &self.notification_tx) {
                        Ok(value) => value.map(Ok),
                        Err(error @ McpError::ServerError { .. }) => Some(Err(error)),
                        Err(error) => return Err(error),
                    };
                if let Some((method, id)) = server_request {
                    self.reply_to_request(&method, id).await?;
                }
                if let Some(value) = outcome
                    && result.replace(value).is_some()
                {
                    return Err(McpError::Protocol(
                        "duplicate JSON-RPC response in SSE batch".into(),
                    ));
                }
            }
            if let Some(result) = result {
                return result;
            }
        }
        Err(McpError::Protocol(format!(
            "SSE response missing id {expected_id}"
        )))
    }

    fn accept_initialization(
        &self,
        result: &serde_json::Value,
        mut session_id: Option<HeaderValue>,
    ) -> Result<(), McpError> {
        let version = result
            .get("protocolVersion")
            .and_then(|value| value.as_str())
            .ok_or_else(|| McpError::Protocol("initialize missing protocolVersion".into()))?;
        if !matches!(version, "2025-11-25" | "2025-06-18" | "2025-03-26") {
            return Err(McpError::Protocol(format!(
                "unsupported MCP HTTP protocol version `{version}`"
            )));
        }
        if let Some(id) = &mut session_id {
            if id.is_empty()
                || !id
                    .as_bytes()
                    .iter()
                    .all(|byte| (0x21..=0x7e).contains(byte))
            {
                return Err(McpError::Protocol("invalid MCP session ID".into()));
            }
            id.set_sensitive(true);
        }
        *self.session.lock().unwrap() = HttpSession {
            id: session_id,
            protocol_version: Some(HeaderValue::from_str(version).expect("supported MCP version")),
        };
        Ok(())
    }

    async fn call_http(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if method == "initialize" {
            *self.session.lock().unwrap() = HttpSession::default();
        }
        tokio::time::timeout(Duration::from_millis(self.timeout_ms), async {
            let response = self.send(&body).await?;
            let session_id = response.headers().get("mcp-session-id").cloned();
            let result = self.read_response(response, id).await?;
            if method == "initialize" {
                self.accept_initialization(&result, session_id)?;
            }
            Ok(result)
        })
        .await
        .map_err(|_| McpError::Timeout {
            timeout_ms: self.timeout_ms,
            method: method.into(),
        })?
    }

    async fn notify_http(&self, method: &str, params: serde_json::Value) -> Result<(), McpError> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        tokio::time::timeout(Duration::from_millis(self.timeout_ms), async {
            self.send(&body).await?;
            Ok(())
        })
        .await
        .map_err(|_| McpError::Timeout {
            timeout_ms: self.timeout_ms,
            method: method.into(),
        })?
    }
}

impl McpTransport for McpHttpTransport {
    fn call<'a>(
        &'a self,
        method: &'a str,
        params: serde_json::Value,
    ) -> BoxFut<'a, Result<serde_json::Value, McpError>> {
        Box::pin(self.call_http(method, params))
    }

    fn notify<'a>(
        &'a self,
        method: &'a str,
        params: serde_json::Value,
    ) -> BoxFut<'a, Result<(), McpError>> {
        Box::pin(self.notify_http(method, params))
    }

    fn kind(&self) -> &'static str {
        "http"
    }

    fn initial_protocol_version(&self) -> &'static str {
        "2025-11-25"
    }
}

pub struct McpClient {
    pub name: String,
    transport: std::sync::Mutex<Arc<dyn McpTransport>>,
    tool_snapshot: std::sync::RwLock<Arc<McpToolSnapshot>>,
    tool_refresh: tokio::sync::Mutex<()>,
    reconnect_lock: tokio::sync::Mutex<()>,
    reconnect: Option<ReconnectConfig>,
    notification_tx: tokio::sync::broadcast::Sender<McpNotification>,
    retained_notification_rx:
        std::sync::Mutex<Option<tokio::sync::broadcast::Receiver<McpNotification>>>,
    sampling_handler: SharedSamplingHandler,
}

#[derive(Debug, Clone)]
pub enum McpNotification {
    ToolsListChanged,
    ResourcesListChanged,
    PromptsListChanged,
    Progress {
        progress_token: String,
        progress: Option<f64>,
        total: Option<f64>,
        message: Option<String>,
    },
    Log {
        level: String,
        data: serde_json::Value,
    },
    Cancelled {
        request_id: String,
        reason: Option<String>,
    },
    Other {
        method: String,
        params: serde_json::Value,
    },
}

/// Config needed to re-create the transport on disconnect.
enum ReconnectConfig {
    Stdio {
        cmd: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
        timeout_ms: u64,
    },
    Http {
        url: String,
        auth_token: Option<String>,
        headers: Vec<(String, String)>,
        timeout_ms: u64,
    },
}

impl McpClient {
    pub async fn connect_config(config: &McpServerConfig) -> Result<Self, McpError> {
        match config.transport {
            TransportKind::Stdio => {
                Self::connect_stdio(
                    &config.name,
                    &config.command,
                    &config.args,
                    &config.env,
                    config.timeout_ms,
                )
                .await
            }
            TransportKind::Http | TransportKind::Sse => {
                let url = config.url.as_deref().ok_or_else(|| {
                    McpError::Protocol("HTTP/SSE transport requires `url`".into())
                })?;
                Self::connect_http_with_headers(
                    &config.name,
                    url,
                    config.auth_token.clone(),
                    &config.headers,
                    config.timeout_ms,
                )
                .await
            }
        }
    }

    pub async fn connect_stdio(
        name: impl Into<String>,
        cmd: &str,
        args: &[String],
        env: &[(String, String)],
        timeout_ms: u64,
    ) -> Result<Self, McpError> {
        let (notification_tx, notification_rx) = tokio::sync::broadcast::channel(256);
        let sampling_handler: SharedSamplingHandler = Arc::new(std::sync::Mutex::new(None));
        let transport: Arc<dyn McpTransport> = Arc::new(
            McpStdioTransport::spawn(
                cmd,
                args,
                env,
                timeout_ms,
                notification_tx.clone(),
                sampling_handler.clone(),
            )
            .await?,
        );
        let name = name.into();
        let mut client = Self::finish_connect(
            name.clone(),
            transport,
            notification_tx,
            notification_rx,
            sampling_handler,
        )
        .await?;
        client.reconnect = Some(ReconnectConfig::Stdio {
            cmd: cmd.to_string(),
            args: args.to_vec(),
            env: env.to_vec(),
            timeout_ms,
        });
        Ok(client)
    }

    pub async fn connect_http(
        name: impl Into<String>,
        url: impl Into<String>,
        auth_token: Option<String>,
        timeout_ms: u64,
    ) -> Result<Self, McpError> {
        Self::connect_http_with_headers(name, url, auth_token, &[], timeout_ms).await
    }

    pub async fn connect_http_with_headers(
        name: impl Into<String>,
        url: impl Into<String>,
        auth_token: Option<String>,
        headers: &[(String, String)],
        timeout_ms: u64,
    ) -> Result<Self, McpError> {
        let (notification_tx, notification_rx) = tokio::sync::broadcast::channel(256);
        let url_str: String = url.into();
        let sampling_handler: SharedSamplingHandler = Arc::new(std::sync::Mutex::new(None));
        let transport: Arc<dyn McpTransport> = Arc::new(
            McpHttpTransport::new(
                url_str.clone(),
                auth_token.clone(),
                timeout_ms,
                notification_tx.clone(),
            )
            .with_headers(headers)?,
        );
        let name = name.into();
        let mut client = Self::finish_connect(
            name.clone(),
            transport,
            notification_tx,
            notification_rx,
            sampling_handler,
        )
        .await?;
        client.reconnect = Some(ReconnectConfig::Http {
            url: url_str,
            auth_token,
            headers: headers.to_vec(),
            timeout_ms,
        });
        Ok(client)
    }

    pub async fn connect_with_transport(
        name: impl Into<String>,
        transport: Arc<dyn McpTransport>,
    ) -> Result<Self, McpError> {
        let (notification_tx, notification_rx) = tokio::sync::broadcast::channel(256);
        Self::finish_connect(
            name.into(),
            transport,
            notification_tx,
            notification_rx,
            Arc::new(std::sync::Mutex::new(None)),
        )
        .await
    }

    async fn finish_connect(
        name: String,
        transport: Arc<dyn McpTransport>,
        notification_tx: tokio::sync::broadcast::Sender<McpNotification>,
        notification_rx: tokio::sync::broadcast::Receiver<McpNotification>,
        sampling_handler: SharedSamplingHandler,
    ) -> Result<Self, McpError> {
        initialize_transport(transport.as_ref()).await?;
        let tool_snapshot = fetch_tool_snapshot(transport.as_ref()).await?;
        Ok(Self {
            name,
            transport: std::sync::Mutex::new(transport),
            tool_snapshot: std::sync::RwLock::new(Arc::new(tool_snapshot)),
            tool_refresh: tokio::sync::Mutex::new(()),
            reconnect_lock: tokio::sync::Mutex::new(()),
            reconnect: None,
            notification_tx,
            retained_notification_rx: std::sync::Mutex::new(Some(notification_rx)),
            sampling_handler,
        })
    }

    pub fn set_sampling_handler(&self, handler: SamplingHandler) {
        *self.sampling_handler.lock().unwrap() = Some(handler);
    }

    pub fn transport_kind(&self) -> &'static str {
        let t = self.transport.lock().unwrap();
        t.kind()
    }

    pub fn subscribe_notifications(&self) -> tokio::sync::broadcast::Receiver<McpNotification> {
        self.retained_notification_rx
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| self.notification_tx.subscribe())
    }

    pub fn tool_snapshot(&self) -> Arc<McpToolSnapshot> {
        self.tool_snapshot.read().unwrap().clone()
    }

    pub async fn refresh_tool_snapshot(&self) -> Result<Option<Arc<McpToolSnapshot>>, McpError> {
        let _refresh = self.tool_refresh.lock().await;
        let transport = { self.transport.lock().unwrap().clone() };
        let candidate = match fetch_tool_snapshot(transport.as_ref()).await {
            Err(McpError::Disconnected) => {
                self.reconnect(&transport).await?;
                let transport = { self.transport.lock().unwrap().clone() };
                // Cursors belong to the expired session; restart the complete listing.
                fetch_tool_snapshot(transport.as_ref()).await?
            }
            result => result?,
        };
        let candidate = Arc::new(candidate);
        let mut current = self.tool_snapshot.write().unwrap();
        if current.fingerprint == candidate.fingerprint {
            return Ok(None);
        }
        *current = candidate.clone();
        Ok(Some(candidate))
    }

    async fn reconnect(&self, failed_transport: &Arc<dyn McpTransport>) -> Result<(), McpError> {
        let _reconnect = self.reconnect_lock.lock().await;
        if !Arc::ptr_eq(&self.transport.lock().unwrap(), failed_transport) {
            return Ok(());
        }
        let cfg = self.reconnect.as_ref().ok_or(McpError::Disconnected)?;
        let new_transport: Arc<dyn McpTransport> = match cfg {
            ReconnectConfig::Stdio {
                cmd,
                args,
                env,
                timeout_ms,
            } => Arc::new(
                McpStdioTransport::spawn(
                    cmd,
                    args,
                    env,
                    *timeout_ms,
                    self.notification_tx.clone(),
                    self.sampling_handler.clone(),
                )
                .await?,
            ),
            ReconnectConfig::Http {
                url,
                auth_token,
                headers,
                timeout_ms,
            } => Arc::new(
                McpHttpTransport::new(
                    url,
                    auth_token.clone(),
                    *timeout_ms,
                    self.notification_tx.clone(),
                )
                .with_headers(headers)?,
            ),
        };
        initialize_transport(new_transport.as_ref()).await?;
        *self.transport.lock().unwrap() = new_transport;
        Ok(())
    }

    async fn call_rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        let transport = { self.transport.lock().unwrap().clone() };
        match transport.call(method, params.clone()).await {
            Err(McpError::Disconnected) => {
                self.reconnect(&transport).await?;
                let transport = { self.transport.lock().unwrap().clone() };
                transport.call(method, params).await
            }
            result => result,
        }
    }

    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<crate::value::Value, McpError> {
        let params = serde_json::json!({
            "name": tool_name,
            "arguments": arguments,
        });
        Ok(mcp_result_to_value(
            self.call_rpc("tools/call", params).await?,
        ))
    }

    pub async fn list_resources(&self) -> Result<Vec<McpResource>, McpError> {
        let v = self
            .call_rpc("resources/list", serde_json::json!({}))
            .await?;
        v.get("resources")
            .and_then(|r| r.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|r| serde_json::from_value(r.clone()).ok())
                    .collect()
            })
            .ok_or_else(|| {
                McpError::Protocol(format!("resources/list missing `resources` array: {v}"))
            })
    }

    pub async fn read_resource(&self, uri: &str) -> Result<Vec<McpResourceContent>, McpError> {
        let params = serde_json::json!({ "uri": uri });
        let v = self.call_rpc("resources/read", params).await?;
        v.get("contents")
            .and_then(|c| c.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|c| serde_json::from_value(c.clone()).ok())
                    .collect()
            })
            .ok_or_else(|| {
                McpError::Protocol(format!("resources/read missing `contents` array: {v}"))
            })
    }

    pub async fn list_prompts(&self) -> Result<Vec<McpPrompt>, McpError> {
        let v = self.call_rpc("prompts/list", serde_json::json!({})).await?;
        v.get("prompts")
            .and_then(|p| p.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|p| serde_json::from_value(p.clone()).ok())
                    .collect()
            })
            .ok_or_else(|| McpError::Protocol(format!("prompts/list missing `prompts` array: {v}")))
    }

    pub async fn get_prompt(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<McpPromptContent, McpError> {
        let params = serde_json::json!({ "name": name, "arguments": arguments });
        let v = self.call_rpc("prompts/get", params).await?;
        serde_json::from_value(v)
            .map_err(|e| McpError::Protocol(format!("prompts/get parse error: {e}")))
    }
}

async fn initialize_transport(transport: &dyn McpTransport) -> Result<(), McpError> {
    let init_params = serde_json::json!({
        "protocolVersion": transport.initial_protocol_version(),
        "capabilities": {},
        "clientInfo": {"name": "atman", "version": env!("CARGO_PKG_VERSION")}
    });
    transport.call("initialize", init_params).await?;
    transport
        .notify("notifications/initialized", serde_json::json!({}))
        .await
}

const MAX_TOOLS_LIST_PAGES: usize = 1024;

async fn fetch_tool_snapshot(transport: &dyn McpTransport) -> Result<McpToolSnapshot, McpError> {
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen_cursors = HashSet::new();
    for _ in 0..MAX_TOOLS_LIST_PAGES {
        let params = cursor.as_ref().map_or_else(
            || serde_json::json!({}),
            |cursor| serde_json::json!({"cursor": cursor}),
        );
        let page = transport.call("tools/list", params).await?;
        let (mut page_tools, next_cursor) = parse_tools_list_page(&page)?;
        tools.append(&mut page_tools);
        let Some(next_cursor) = next_cursor else {
            return McpToolSnapshot::build(tools);
        };
        if !seen_cursors.insert(next_cursor.clone()) {
            return Err(McpError::Protocol(format!(
                "tools/list repeated cursor `{next_cursor}`"
            )));
        }
        cursor = Some(next_cursor);
    }
    Err(McpError::Protocol(format!(
        "tools/list exceeded {MAX_TOOLS_LIST_PAGES} pages"
    )))
}

fn parse_tools_list_page(
    v: &serde_json::Value,
) -> Result<(Vec<McpToolSchema>, Option<String>), McpError> {
    let arr = v.get("tools").and_then(|t| t.as_array()).ok_or_else(|| {
        McpError::Protocol(format!("tools/list response missing `tools` array: {v}"))
    })?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let schema: McpToolSchema = serde_json::from_value(item.clone())
            .map_err(|e| McpError::Protocol(format!("tool schema: {e}")))?;
        out.push(schema);
    }
    let next_cursor = match v.get("nextCursor") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(cursor)) => Some(cursor.clone()),
        Some(other) => {
            return Err(McpError::Protocol(format!(
                "tools/list `nextCursor` must be a string: {other}"
            )));
        }
    };
    Ok((out, next_cursor))
}

pub fn mcp_result_to_value(result: serde_json::Value) -> crate::value::Value {
    if let Some(content) = result.get("content").and_then(|c| c.as_array()) {
        let text_parts: Vec<String> = content
            .iter()
            .filter_map(|item| {
                if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                    item.get("text").and_then(|t| t.as_str()).map(String::from)
                } else {
                    None
                }
            })
            .collect();
        let is_error = result
            .get("isError")
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        return crate::value::Value::Struct(vec![
            ("text".into(), crate::value::Value::Str(text_parts.join(""))),
            ("is_error".into(), crate::value::Value::Bool(is_error)),
            ("raw".into(), crate::value::Value::from_json(result)),
        ]);
    }
    crate::value::Value::from_json(result)
}

pub fn value_to_mcp_args(args: &crate::tool::ToolArgs) -> Result<serde_json::Value, RuntimeError> {
    let mut map = serde_json::Map::new();
    for (name, value) in &args.named {
        map.insert(name.clone(), value.to_json()?);
    }
    Ok(serde_json::Value::Object(map))
}

// Some MCP servers use a schema like {action: enum, params: {type: "object"}}
// where params is a catch-all for every argument not named at the top level.
// atman sends flat arguments and Zod drops everything except action+params,
// so we detect the pattern and repack unmatched keys into the container.

#[derive(Debug, Clone)]
struct ReconcilePlan {
    explicit_keys: HashSet<String>,
    container_key: String,
    container_required: bool,
}

fn build_reconcile_plan(schema: &serde_json::Value) -> Option<ReconcilePlan> {
    let props = schema.get("properties")?.as_object()?;
    let required: HashSet<&str> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let mut explicit_keys = HashSet::new();
    let mut catch_all: Option<(&str, bool)> = None;

    for (name, prop) in props {
        let is_obj = prop.get("type").and_then(|t| t.as_str()) == Some("object");
        let has_sub_props = prop.get("properties").is_some_and(|p| p.is_object());
        let add_props_false =
            prop.get("additionalProperties").and_then(|a| a.as_bool()) == Some(false);

        if is_obj && !has_sub_props && !add_props_false {
            if catch_all.is_some() {
                return None;
            }
            catch_all = Some((name.as_str(), required.contains(name.as_str())));
        } else {
            explicit_keys.insert(name.clone());
        }
    }

    let (container_key, container_required) = catch_all?;
    Some(ReconcilePlan {
        explicit_keys,
        container_key: container_key.to_owned(),
        container_required,
    })
}

fn reconcile(
    plan: &ReconcilePlan,
    flat: serde_json::Map<String, serde_json::Value>,
) -> serde_json::Value {
    let mut container = serde_json::Map::new();
    let mut out = serde_json::Map::new();

    for (k, v) in flat {
        if plan.explicit_keys.contains(&k) {
            out.insert(k, v);
        } else if k == plan.container_key {
            if let serde_json::Value::Object(inner) = v {
                for (ik, iv) in inner {
                    container.insert(ik, iv);
                }
            } else {
                container.insert(k, v);
            }
        } else {
            container.insert(k, v);
        }
    }

    if !container.is_empty() || plan.container_required {
        out.insert(
            plan.container_key.clone(),
            serde_json::Value::Object(container),
        );
    }

    serde_json::Value::Object(out)
}

pub struct McpToolAdapter {
    qualified_name: String,
    tool_name: String,
    tier: crate::tool::Tier,
    client: Arc<McpClient>,
    reconcile: Option<ReconcilePlan>,
    schema: serde_json::Value,
    validator: Result<jsonschema::Validator, String>,
    description: Option<String>,
}

struct LocalSchemaReferences;

impl jsonschema::Retrieve for LocalSchemaReferences {
    fn retrieve(
        &self,
        _uri: &jsonschema::Uri<String>,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        Err("external references are not allowed in MCP tool schemas".into())
    }
}

pub(crate) fn mcp_server_scheduling_claim(server: &str) -> crate::tool_schedule::SchedulingClaims {
    crate::tool_schedule::SchedulingClaims::declared([
        crate::tool_schedule::ResourceAccess::exclusive(
            crate::tool_schedule::ResourceKey::McpServer(server.to_owned()),
        ),
    ])
}

impl McpToolAdapter {
    pub fn new(
        client: Arc<McpClient>,
        tool_name: impl Into<String>,
        tier: crate::tool::Tier,
        schema: Option<&serde_json::Value>,
        description: Option<&str>,
    ) -> Self {
        let tool_name = tool_name.into();
        let qualified_name = format!("mcp.{}.{}", client.name, tool_name);
        let reconcile = schema.and_then(build_reconcile_plan);
        let schema = schema
            .cloned()
            .unwrap_or_else(|| serde_json::json!({"type": "object"}));
        let validator = jsonschema::options()
            .with_retriever(LocalSchemaReferences)
            .build(&schema)
            .map_err(|error| error.to_string());
        Self {
            qualified_name,
            tool_name,
            tier,
            client,
            reconcile,
            schema,
            validator,
            description: description.map(str::to_string),
        }
    }
}

impl crate::tool::Tool for McpToolAdapter {
    fn name(&self) -> &str {
        &self.qualified_name
    }

    fn tier(&self) -> crate::tool::Tier {
        self.tier
    }

    fn input_schema(&self) -> serde_json::Value {
        self.schema.clone()
    }

    fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    fn mcp_identity(&self) -> Option<(&str, &str)> {
        Some((&self.client.name, &self.tool_name))
    }

    fn scheduling_claims(
        &self,
        _args: &crate::tool::ToolArgs,
        _ctx: &crate::tool::ToolCtx,
    ) -> Result<crate::tool_schedule::SchedulingClaims, RuntimeError> {
        Ok(mcp_server_scheduling_claim(&self.client.name))
    }

    fn call<'a>(
        &'a self,
        args: crate::tool::ToolArgs,
        _ctx: &'a crate::tool::ToolCtx,
    ) -> crate::tool::BoxFut<'a, crate::tool::ToolResult> {
        Box::pin(async move {
            let params = value_to_mcp_args(&args)?;
            let params = if let Some(plan) = &self.reconcile {
                let map = match params {
                    serde_json::Value::Object(m) => m,
                    _ => return Err(RuntimeError::ToolFailed("mcp: expected object args".into())),
                };
                reconcile(plan, map)
            } else {
                params
            };
            let validator = self.validator.as_ref().map_err(|error| {
                RuntimeError::ToolFailed(format!(
                    "MCP tool `{}` has an invalid input schema: {error}",
                    self.qualified_name,
                ))
            })?;
            validator.validate(&params).map_err(|error| {
                RuntimeError::ToolFailed(format!(
                    "MCP tool `{}` input does not match its schema at {}: {error}",
                    self.qualified_name, error.instance_path,
                ))
            })?;
            let v = self.client.call_tool(&self.tool_name, params).await?;
            Ok(v)
        })
    }
}

/// Name and description of a single tool exposed by an MCP server.
#[derive(Debug, Clone, PartialEq, Eq, documented::Documented, documented::DocumentedFields)]
pub struct McpToolInfo {
    /// The tool name as reported by the MCP server.
    pub name: String,
    /// Human-readable description of what the tool does, if provided.
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerState {
    Disabled,
    Pending,
    Connecting,
    Connected {
        tool_count: usize,
        tools: Vec<McpToolInfo>,
    },
    Error {
        message: String,
    },
    Disconnected {
        message: String,
    },
    Timeout {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerStatus {
    pub name: String,
    pub transport: TransportKind,
    pub state: McpServerState,
}

impl McpServerStatus {
    pub fn is_ok(&self) -> bool {
        matches!(self.state, McpServerState::Connected { .. })
    }
}

/// Derive ok/total counts from a list of server statuses.
pub fn mcp_counts(servers: &[McpServerStatus]) -> (u16, u16) {
    let total = servers.len() as u16;
    let ok = servers.iter().filter(|s| s.is_ok()).count() as u16;
    (ok, total)
}

/// Transport protocol used to communicate with an MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, documented::DocumentedVariants)]
pub enum TransportKind {
    #[default]
    /// Spawn a subprocess and communicate via stdin/stdout (JSON-RPC over stdio).
    Stdio,
    /// Send JSON-RPC requests over HTTP.
    Http,
    /// Receive server-pushed messages via Server-Sent Events.
    Sse,
}

/// Configuration for a single MCP server connection.
#[derive(Debug, Clone, documented::Documented, documented::DocumentedFields)]
pub struct McpServerConfig {
    /// Unique name for this server, used in tool names as `mcp.{name}.{tool}`.
    pub name: String,
    /// Transport protocol: stdio, http, or sse.
    #[allow(clippy::field_reassign_with_default)]
    pub transport: TransportKind,
    /// stdio: the executable command to spawn.
    pub command: String,
    /// stdio: command-line arguments passed to the executable.
    pub args: Vec<String>,
    /// stdio: environment variables for the subprocess.
    pub env: Vec<(String, String)>,
    /// http/sse: the server URL to connect to.
    pub url: Option<String>,
    /// http/sse: bearer token for authentication.
    pub auth_token: Option<String>,
    /// http/sse: custom HTTP headers.
    pub headers: Vec<(String, String)>,
    /// Approval tier applied to all tools from this server.
    pub tier: crate::tool::Tier,
    /// Connection initialization timeout in milliseconds.
    pub timeout_ms: u64,
    /// If true, skip this server at startup.
    pub disabled: bool,
}

impl McpServerConfig {
    pub fn stdio(
        name: impl Into<String>,
        command: impl Into<String>,
        args: Vec<String>,
        tier: crate::tool::Tier,
        timeout_ms: u64,
    ) -> Self {
        Self {
            name: name.into(),
            transport: TransportKind::Stdio,
            command: command.into(),
            args,
            env: Vec::new(),
            url: None,
            auth_token: None,
            headers: Vec::new(),
            tier,
            timeout_ms,
            disabled: false,
        }
    }

    pub fn http(
        name: impl Into<String>,
        url: impl Into<String>,
        auth_token: Option<String>,
        tier: crate::tool::Tier,
        timeout_ms: u64,
    ) -> Self {
        Self {
            name: name.into(),
            transport: TransportKind::Http,
            command: String::new(),
            args: Vec::new(),
            env: Vec::new(),
            url: Some(url.into()),
            auth_token,
            headers: Vec::new(),
            tier,
            timeout_ms,
            disabled: false,
        }
    }

    pub fn sse(
        name: impl Into<String>,
        url: impl Into<String>,
        auth_token: Option<String>,
        tier: crate::tool::Tier,
        timeout_ms: u64,
    ) -> Self {
        Self {
            name: name.into(),
            transport: TransportKind::Sse,
            command: String::new(),
            args: Vec::new(),
            env: Vec::new(),
            url: Some(url.into()),
            auth_token,
            headers: Vec::new(),
            tier,
            timeout_ms,
            disabled: false,
        }
    }
}

pub async fn register_from_configs(
    reg: &crate::tool::ToolRegistry,
    configs: &[McpServerConfig],
) -> Vec<Result<McpClientStatus, McpBootError>> {
    let mut out = Vec::with_capacity(configs.len());
    for cfg in configs {
        let outcome = McpClient::connect_config(cfg).await;
        match outcome {
            Ok(client) => {
                let transport_kind = client.transport_kind();
                let arc_client = Arc::new(client);
                let snapshot = arc_client.tool_snapshot();
                publish_tool_snapshot(reg, arc_client.clone(), cfg.tier, &snapshot);
                out.push(Ok(McpClientStatus {
                    name: cfg.name.clone(),
                    tool_count: snapshot.tools.len(),
                    transport: transport_kind,
                    tools: snapshot
                        .tools
                        .iter()
                        .map(|t| McpToolInfo {
                            name: t.name.clone(),
                            description: t.description.clone(),
                        })
                        .collect(),
                }));
            }
            Err(e) => out.push(Err(McpBootError {
                name: cfg.name.clone(),
                error: e,
            })),
        }
    }
    out
}

pub fn mcp_tool_namespace(server_name: &str) -> String {
    format!("mcp.{server_name}.")
}

pub fn publish_tool_snapshot(
    registry: &crate::tool::ToolRegistry,
    client: Arc<McpClient>,
    tier: crate::tool::Tier,
    snapshot: &McpToolSnapshot,
) {
    let tools = snapshot
        .tools
        .iter()
        .map(|tool| {
            Arc::new(McpToolAdapter::new(
                client.clone(),
                &tool.name,
                tier,
                tool.input_schema.as_ref(),
                tool.description.as_deref(),
            )) as Arc<dyn crate::tool::Tool>
        })
        .collect();
    registry.replace_namespace(&mcp_tool_namespace(&client.name), tools);
}

#[derive(Debug)]
pub struct McpClientStatus {
    pub name: String,
    pub tool_count: usize,
    pub transport: &'static str,
    pub tools: Vec<McpToolInfo>,
}

#[derive(Debug, thiserror::Error)]
#[error("mcp `{name}` failed: {error}")]
pub struct McpBootError {
    pub name: String,
    pub error: McpError,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_responses_require_matching_rpc_identity_and_exactly_one_outcome() {
        let (notifications, mut receiver) = tokio::sync::broadcast::channel(4);
        for response in [
            serde_json::json!({"jsonrpc":"1.0","id":7,"result":null}),
            serde_json::json!({"jsonrpc":"2.0","id":8,"result":null}),
            serde_json::json!({"jsonrpc":"2.0","id":7}),
            serde_json::json!({"jsonrpc":"2.0","id":7,"result":null,"error":{"code":-1,"message":"error"}}),
            serde_json::json!({"jsonrpc":"2.0","id":7,"error":null}),
        ] {
            assert!(matches!(
                parse_http_message(response, 7, false, &notifications),
                Err(McpError::Protocol(_))
            ));
        }
        assert_eq!(
            parse_http_message(
                serde_json::json!({"jsonrpc":"2.0","id":7,"result":null}),
                7,
                false,
                &notifications,
            )
            .unwrap(),
            Some(serde_json::Value::Null)
        );
        assert!(
            parse_http_message(
                serde_json::json!({"jsonrpc":"2.0","id":8,"result":"unrelated"}),
                7,
                true,
                &notifications,
            )
            .unwrap()
            .is_none()
        );
        assert!(
            parse_http_message(
                serde_json::json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"}),
                7,
                true,
                &notifications,
            )
            .unwrap()
            .is_none()
        );
        assert!(matches!(
            receiver.try_recv(),
            Ok(McpNotification::ToolsListChanged)
        ));
    }

    struct PagedToolTransport {
        repeated_cursor: bool,
        calls: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
    }

    impl PagedToolTransport {
        fn new(repeated_cursor: bool) -> Self {
            Self {
                repeated_cursor,
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl McpTransport for PagedToolTransport {
        fn call<'a>(
            &'a self,
            method: &'a str,
            params: serde_json::Value,
        ) -> BoxFut<'a, Result<serde_json::Value, McpError>> {
            self.calls
                .lock()
                .unwrap()
                .push((method.to_string(), params.clone()));
            let result = match method {
                "initialize" => Ok(serde_json::json!({})),
                "tools/call" => Ok(serde_json::json!({"content":[]})),
                "tools/list" if params.get("cursor").is_none() => Ok(serde_json::json!({
                    "tools": [{
                        "name": "zeta",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"z": {"type": "string"}, "a": {"type": "string"}}
                        }
                    }],
                    "nextCursor": "page-2"
                })),
                "tools/list" if self.repeated_cursor => Ok(serde_json::json!({
                    "tools": [],
                    "nextCursor": "page-2"
                })),
                "tools/list" => Ok(serde_json::json!({
                    "tools": [{"name": "alpha", "inputSchema": {"type": "object"}}]
                })),
                other => Err(McpError::Protocol(format!("unexpected call `{other}`"))),
            };
            Box::pin(async move { result })
        }

        fn notify<'a>(
            &'a self,
            _method: &'a str,
            _params: serde_json::Value,
        ) -> BoxFut<'a, Result<(), McpError>> {
            Box::pin(async { Ok(()) })
        }

        fn kind(&self) -> &'static str {
            "test"
        }
    }

    struct MutableToolTransport {
        response: std::sync::Mutex<Result<serde_json::Value, String>>,
    }

    impl MutableToolTransport {
        fn new(tool_name: &str) -> Self {
            Self {
                response: std::sync::Mutex::new(Ok(Self::tool_page(tool_name))),
            }
        }

        fn set_tool(&self, tool_name: &str) {
            *self.response.lock().unwrap() = Ok(Self::tool_page(tool_name));
        }

        fn fail(&self, message: &str) {
            *self.response.lock().unwrap() = Err(message.to_string());
        }

        fn tool_page(tool_name: &str) -> serde_json::Value {
            serde_json::json!({
                "tools": [{"name": tool_name, "inputSchema": {"type": "object"}}]
            })
        }
    }

    impl McpTransport for MutableToolTransport {
        fn call<'a>(
            &'a self,
            method: &'a str,
            _params: serde_json::Value,
        ) -> BoxFut<'a, Result<serde_json::Value, McpError>> {
            let result = match method {
                "initialize" => Ok(serde_json::json!({})),
                "tools/list" => self
                    .response
                    .lock()
                    .unwrap()
                    .clone()
                    .map_err(McpError::Protocol),
                other => Err(McpError::Protocol(format!("unexpected call `{other}`"))),
            };
            Box::pin(async move { result })
        }

        fn notify<'a>(
            &'a self,
            _method: &'a str,
            _params: serde_json::Value,
        ) -> BoxFut<'a, Result<(), McpError>> {
            Box::pin(async { Ok(()) })
        }

        fn kind(&self) -> &'static str {
            "test"
        }
    }

    #[tokio::test]
    async fn dynamic_adapter_and_mcp_call_share_the_exact_server_claim() {
        use crate::tool::Tool as _;

        let client = Arc::new(
            McpClient::connect_with_transport(
                "team.tools",
                Arc::new(MutableToolTransport::new("files.read")),
            )
            .await
            .unwrap(),
        );
        let adapter =
            McpToolAdapter::new(client, "files.read", crate::tool::Tier::Zero, None, None);
        let wrapper_args = crate::tool::ToolArgs {
            named: vec![
                (
                    "server".into(),
                    crate::value::Value::Str("team.tools".into()),
                ),
                ("tool".into(), crate::value::Value::Str("files.read".into())),
            ],
            ..crate::tool::ToolArgs::default()
        };
        let ctx = crate::tool::ToolCtx::new();

        assert_eq!(
            adapter
                .scheduling_claims(&crate::tool::ToolArgs::default(), &ctx)
                .unwrap(),
            crate::tools::mcp::McpCall
                .scheduling_claims(&wrapper_args, &ctx)
                .unwrap()
        );
    }

    #[tokio::test]
    async fn adapter_validates_original_schema_after_reconciliation_without_external_retrieval() {
        use crate::tool::Tool;

        let transport = Arc::new(PagedToolTransport::new(false));
        let client = Arc::new(
            McpClient::connect_with_transport("schema-test", transport.clone())
                .await
                .unwrap(),
        );
        let schema = serde_json::json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema",
            "$defs":{"positive":{"type":"integer","minimum":1}},
            "type":"object",
            "properties":{
                "items":{"type":"array","minItems":1,"items":{"$ref":"#/$defs/positive"}},
                "mode":{"enum":["read","write"]},
                "id":{"oneOf":[{"type":"integer"},{"type":"string","minLength":2}]}
            },
            "required":["items","mode","id"],
            "additionalProperties":false
        });
        let adapter = McpToolAdapter::new(
            client.clone(),
            "validate",
            crate::tool::Tier::Zero,
            Some(&schema),
            None,
        );
        let args = |input: serde_json::Value| crate::tool::ToolArgs {
            positional: Vec::new(),
            named: input
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, value)| (key.clone(), crate::value::Value::from_json(value.clone())))
                .collect(),
        };
        let ctx = crate::tool::ToolCtx::new();
        for input in [
            serde_json::json!({"items":[0],"mode":"read","id":7}),
            serde_json::json!({"items":[1],"mode":"unknown","id":7}),
            serde_json::json!({"items":[1],"mode":"read","id":true}),
            serde_json::json!({"items":[1],"mode":"read","id":"x"}),
            serde_json::json!({"items":[1],"mode":"read","id":7,"extra":1}),
        ] {
            assert!(
                adapter
                    .call(args(input), &ctx)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("input does not match")
            );
        }
        assert!(
            !transport
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|(method, _)| method == "tools/call")
        );
        adapter
            .call(
                args(serde_json::json!({"items":[1],"mode":"read","id":7})),
                &ctx,
            )
            .await
            .unwrap();

        let container_schema = serde_json::json!({
            "type":"object",
            "properties":{
                "action":{"enum":["count"]},
                "params":{"type":"object","additionalProperties":{"type":"integer","minimum":1}}
            },
            "required":["action","params"],
            "additionalProperties":false
        });
        let container = McpToolAdapter::new(
            client.clone(),
            "container",
            crate::tool::Tier::Zero,
            Some(&container_schema),
            None,
        );
        container
            .call(args(serde_json::json!({"action":"count","n":3})), &ctx)
            .await
            .unwrap();
        assert!(
            container
                .call(
                    args(serde_json::json!({"action":"count","n":"wrong"})),
                    &ctx
                )
                .await
                .is_err()
        );
        for invalid_schema in [
            serde_json::json!({"type":42}),
            serde_json::json!({"$ref":"https://schemas.example/tool.json"}),
            serde_json::json!({"$ref":"file:///not-readable/schema.json"}),
        ] {
            let invalid = McpToolAdapter::new(
                client.clone(),
                "invalid",
                crate::tool::Tier::Zero,
                Some(&invalid_schema),
                None,
            );
            assert!(invalid.validator.is_err());
            assert!(
                invalid
                    .call(args(serde_json::json!({})), &ctx)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("invalid input schema")
            );
        }
        let calls = transport.calls.lock().unwrap();
        let sent = calls
            .iter()
            .filter(|(method, _)| method == "tools/call")
            .collect::<Vec<_>>();
        assert_eq!(sent.len(), 2);
        assert_eq!(
            sent[1].1["arguments"],
            serde_json::json!({"action":"count","params":{"n":3}})
        );
        assert_eq!(adapter.input_schema(), schema);
    }

    #[tokio::test]
    async fn connect_reads_every_tools_page_into_one_canonical_snapshot() {
        let transport = Arc::new(PagedToolTransport::new(false));
        let client = McpClient::connect_with_transport("paged", transport.clone())
            .await
            .unwrap();

        let snapshot = client.tool_snapshot();
        assert_eq!(
            snapshot
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "zeta"]
        );
        let list_params = transport
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(method, _)| method == "tools/list")
            .map(|(_, params)| params.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            list_params,
            vec![
                serde_json::json!({}),
                serde_json::json!({"cursor": "page-2"})
            ]
        );
    }

    #[tokio::test]
    async fn connect_rejects_repeated_tools_page_cursor() {
        let transport = Arc::new(PagedToolTransport::new(true));
        let error = match McpClient::connect_with_transport("paged", transport).await {
            Ok(_) => panic!("repeated cursor should fail"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("repeated cursor `page-2`"));
    }

    #[tokio::test]
    async fn notification_receiver_retains_events_until_runtime_subscribes() {
        let transport = Arc::new(MutableToolTransport::new("alpha"));
        let client = McpClient::connect_with_transport("dynamic", transport)
            .await
            .unwrap();

        client
            .notification_tx
            .send(McpNotification::ToolsListChanged)
            .unwrap();
        let mut notifications = client.subscribe_notifications();

        assert!(matches!(
            notifications.try_recv(),
            Ok(McpNotification::ToolsListChanged)
        ));
    }

    #[tokio::test]
    async fn refresh_replaces_changed_namespace_and_preserves_last_good_on_failure() {
        let transport = Arc::new(MutableToolTransport::new("alpha"));
        let client = Arc::new(
            McpClient::connect_with_transport("dynamic", transport.clone())
                .await
                .unwrap(),
        );
        let registry = crate::tool::ToolRegistry::new();
        let initial = client.tool_snapshot();
        publish_tool_snapshot(&registry, client.clone(), crate::tool::Tier::Zero, &initial);

        assert!(client.refresh_tool_snapshot().await.unwrap().is_none());
        transport.set_tool("beta");
        let changed = client
            .refresh_tool_snapshot()
            .await
            .unwrap()
            .expect("changed snapshot");
        publish_tool_snapshot(&registry, client.clone(), crate::tool::Tier::Zero, &changed);
        assert!(!registry.has("mcp.dynamic.alpha"));
        assert!(registry.has("mcp.dynamic.beta"));

        let last_good = client.tool_snapshot().fingerprint.clone();
        transport.fail("refresh unavailable");
        assert!(client.refresh_tool_snapshot().await.is_err());
        assert_eq!(client.tool_snapshot().fingerprint, last_good);
        assert!(registry.has("mcp.dynamic.beta"));
    }

    #[tokio::test]
    async fn stdio_transport_call_returns_result() {
        let script = r#"
import sys, json
for line in sys.stdin:
    req = json.loads(line)
    resp = {"jsonrpc": "2.0", "id": req["id"], "result": {"echo": req.get("params")}}
    print(json.dumps(resp), flush=True)
"#;
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("mcp_echo.py");
        std::fs::write(&script_path, script).unwrap();

        let transport = McpStdioTransport::spawn(
            "python3",
            &[script_path.display().to_string()],
            &[],
            5000,
            tokio::sync::broadcast::channel(256).0,
            Arc::new(std::sync::Mutex::new(None)),
        )
        .await
        .unwrap();

        let result = transport
            .call("hello", serde_json::json!({"x": 1}))
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!({"echo": {"x": 1}}));
    }

    #[tokio::test]
    async fn stdio_transport_propagates_server_error() {
        let script = r#"
import sys, json
for line in sys.stdin:
    req = json.loads(line)
    resp = {"jsonrpc": "2.0", "id": req["id"], "error": {"code": -32601, "message": "not found"}}
    print(json.dumps(resp), flush=True)
"#;
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("mcp_err.py");
        std::fs::write(&script_path, script).unwrap();

        let transport = McpStdioTransport::spawn(
            "python3",
            &[script_path.display().to_string()],
            &[],
            5000,
            tokio::sync::broadcast::channel(256).0,
            Arc::new(std::sync::Mutex::new(None)),
        )
        .await
        .unwrap();
        let err = transport
            .call("boom", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::ServerError { code: -32601, .. }));
    }

    #[tokio::test]
    async fn stdio_transport_call_times_out() {
        let script = r#"
import sys
for line in sys.stdin:
    pass
"#;
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("mcp_silent.py");
        std::fs::write(&script_path, script).unwrap();

        let transport = McpStdioTransport::spawn(
            "python3",
            &[script_path.display().to_string()],
            &[],
            200,
            tokio::sync::broadcast::channel(256).0,
            Arc::new(std::sync::Mutex::new(None)),
        )
        .await
        .unwrap();
        let err = transport
            .call("hangs", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::Timeout { .. }));
    }

    #[test]
    fn tool_schema_deserialize_from_mcp_list_tools_response() {
        let json = serde_json::json!({
            "name": "read_file",
            "description": "reads a file",
            "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}}}
        });
        let s: McpToolSchema = serde_json::from_value(json).unwrap();
        assert_eq!(s.name, "read_file");
        assert!(s.description.as_deref().unwrap().contains("reads"));
    }

    #[test]
    fn no_catch_all_returns_none() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "document_id": {"type": "string"}
            },
            "required": ["document_id"]
        });
        assert!(build_reconcile_plan(&schema).is_none());
    }

    #[test]
    fn single_catch_all_builds_plan() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["get", "list", "update"]},
                "params": {"type": "object"}
            },
            "required": ["action"]
        });
        let plan = build_reconcile_plan(&schema).expect("should build a plan");
        assert!(plan.explicit_keys.contains("action"));
        assert!(!plan.explicit_keys.contains("params"));
        assert_eq!(plan.container_key, "params");
        assert!(!plan.container_required);
    }

    #[test]
    fn container_required_is_detected() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string"},
                "params": {"type": "object"}
            },
            "required": ["action", "params"]
        });
        let plan = build_reconcile_plan(&schema).expect("should build");
        assert!(plan.container_required);
    }

    #[test]
    fn reconcile_moves_unmatched_into_container() {
        let plan = ReconcilePlan {
            explicit_keys: ["action".into()].into(),
            container_key: "params".into(),
            container_required: false,
        };
        let flat: serde_json::Map<_, _> = serde_json::json!({
            "action": "update",
            "guid": "abc",
            "due": "2026-07-17"
        })
        .as_object()
        .unwrap()
        .clone();

        let out = reconcile(&plan, flat);
        assert_eq!(out["action"], "update");
        assert_eq!(out["params"]["guid"], "abc");
        assert_eq!(out["params"]["due"], "2026-07-17");
    }

    #[test]
    fn reconcile_merges_container_key_instead_of_nesting() {
        let plan = ReconcilePlan {
            explicit_keys: ["action".into()].into(),
            container_key: "params".into(),
            container_required: false,
        };
        let flat: serde_json::Map<_, _> = serde_json::json!({
            "action": "list_events",
            "params": {
                "calendar_id": "cal_123",
                "start_time": "2026-07-31T00:00:00+08:00"
            }
        })
        .as_object()
        .unwrap()
        .clone();

        let out = reconcile(&plan, flat);
        assert_eq!(out["action"], "list_events");
        assert_eq!(out["params"]["calendar_id"], "cal_123");
        assert_eq!(out["params"]["start_time"], "2026-07-31T00:00:00+08:00");
        assert!(
            out["params"].get("params").is_none(),
            "params should not be nested inside params"
        );
    }

    #[test]
    fn reconcile_leaves_standard_schema_untouched() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string"},
                "guid": {"type": "string"}
            },
            "required": ["action", "guid"]
        });
        assert!(build_reconcile_plan(&schema).is_none());
    }

    #[test]
    fn two_catch_alls_bails() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string"},
                "params": {"type": "object"},
                "extras": {"type": "object"}
            }
        });
        assert!(build_reconcile_plan(&schema).is_none());
    }

    #[test]
    fn sealed_object_not_catch_all() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string"},
                "lock": {"type": "object", "additionalProperties": false}
            }
        });
        assert!(build_reconcile_plan(&schema).is_none());
    }

    #[test]
    fn nested_object_with_properties_not_catch_all() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string"},
                "address": {
                    "type": "object",
                    "properties": {
                        "street": {"type": "string"},
                        "city": {"type": "string"}
                    }
                }
            }
        });
        assert!(build_reconcile_plan(&schema).is_none());
    }

    #[test]
    fn required_empty_container_still_emitted() {
        let plan = ReconcilePlan {
            explicit_keys: ["action".into()].into(),
            container_key: "body".into(),
            container_required: true,
        };
        let flat: serde_json::Map<_, _> = serde_json::json!({"action": "ping"})
            .as_object()
            .unwrap()
            .clone();

        let out = reconcile(&plan, flat);
        assert_eq!(out["action"], "ping");
        assert!(out.get("body").and_then(|v| v.as_object()).is_some());
    }

    #[test]
    fn empty_non_required_container_omitted() {
        let plan = ReconcilePlan {
            explicit_keys: ["action".into()].into(),
            container_key: "params".into(),
            container_required: false,
        };
        let flat: serde_json::Map<_, _> = serde_json::json!({"action": "list"})
            .as_object()
            .unwrap()
            .clone();

        let out = reconcile(&plan, flat);
        assert_eq!(out["action"], "list");
        assert!(out.get("params").is_none());
    }
}
