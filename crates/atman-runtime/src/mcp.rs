use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolSchema {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Option<serde_json::Value>,
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

        let pending: PendingCalls = Arc::new(Mutex::new(HashMap::new()));
        let pending_for_reader = pending.clone();
        let reader_task = tokio::spawn(async move {
            reader_loop(stdout, pending_for_reader).await;
        });

        Ok(Self {
            stdin: Arc::new(Mutex::new(stdin)),
            pending,
            next_id: Arc::new(Mutex::new(1)),
            child: Arc::new(Mutex::new(child)),
            reader_task: Arc::new(Mutex::new(Some(reader_task))),
            timeout_ms,
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

async fn reader_loop(stdout: ChildStdout, pending: PendingCalls) {
    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                let parsed: JsonRpcResponse = match serde_json::from_str(&line) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                let Some(id) = parsed.id else {
                    continue;
                };
                let sender = pending.lock().await.remove(&id);
                if let Some(sender) = sender {
                    let outcome = if let Some(err) = parsed.error {
                        Err(McpError::ServerError {
                            code: err.code,
                            message: err.message,
                        })
                    } else {
                        Ok(parsed.result.unwrap_or(serde_json::Value::Null))
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

pub struct McpHttpTransport {
    url: String,
    auth_token: Option<String>,
    client: reqwest::Client,
    next_id: AtomicU64,
    timeout_ms: u64,
    retry_attempts: u32,
}

impl McpHttpTransport {
    pub fn new(url: impl Into<String>, auth_token: Option<String>, timeout_ms: u64) -> Self {
        Self {
            url: url.into(),
            auth_token,
            client: reqwest::Client::new(),
            next_id: AtomicU64::new(1),
            timeout_ms,
            retry_attempts: 3,
        }
    }

    #[doc(hidden)]
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
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
        let call_timeout = Duration::from_millis(self.timeout_ms);
        let attempt_result = tokio::time::timeout(call_timeout, async {
            let mut delay_ms = 100u64;
            let mut last_err: Option<McpError> = None;
            for attempt in 0..=self.retry_attempts {
                let mut req = self.client.post(&self.url).json(&body);
                if let Some(t) = &self.auth_token {
                    req = req.bearer_auth(t);
                }
                match req.send().await {
                    Ok(resp) => {
                        let status = resp.status();
                        if status.is_success() {
                            let text = resp
                                .text()
                                .await
                                .map_err(|e| McpError::Io(format!("mcp http body: {e}")))?;
                            let parsed: JsonRpcResponse = serde_json::from_str(&text)
                                .map_err(|e| McpError::Protocol(format!("mcp http parse: {e}")))?;
                            if let Some(err) = parsed.error {
                                return Err(McpError::ServerError {
                                    code: err.code,
                                    message: err.message,
                                });
                            }
                            return Ok(parsed.result.unwrap_or(serde_json::Value::Null));
                        }
                        if status.is_server_error() {
                            last_err =
                                Some(McpError::Io(format!("mcp http {status}: server error")));
                            if attempt < self.retry_attempts {
                                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                                delay_ms = (delay_ms * 5).min(2000);
                                continue;
                            }
                        }
                        let body_text = resp.text().await.unwrap_or_default();
                        return Err(McpError::Io(format!("mcp http {status}: {body_text}")));
                    }
                    Err(e) => {
                        last_err = Some(McpError::Io(format!("mcp http send: {e}")));
                        if attempt < self.retry_attempts {
                            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                            delay_ms = (delay_ms * 5).min(2000);
                            continue;
                        }
                    }
                }
            }
            Err(last_err.unwrap_or(McpError::Disconnected))
        })
        .await;
        match attempt_result {
            Ok(inner) => inner,
            Err(_) => Err(McpError::Timeout {
                timeout_ms: self.timeout_ms,
                method: method.into(),
            }),
        }
    }

    async fn notify_http(&self, method: &str, params: serde_json::Value) -> Result<(), McpError> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let mut req = self.client.post(&self.url).json(&body);
        if let Some(t) = &self.auth_token {
            req = req.bearer_auth(t);
        }
        // Fire and forget — don't wait for response body.
        let _ = req.send().await;
        Ok(())
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
}

pub struct McpClient {
    pub name: String,
    transport: std::sync::Mutex<Arc<dyn McpTransport>>,
    pub tools: Vec<McpToolSchema>,
    reconnect: Option<ReconnectConfig>,
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
        timeout_ms: u64,
    },
}

impl McpClient {
    pub async fn connect_stdio(
        name: impl Into<String>,
        cmd: &str,
        args: &[String],
        env: &[(String, String)],
        timeout_ms: u64,
    ) -> Result<Self, McpError> {
        let transport: Arc<dyn McpTransport> =
            Arc::new(McpStdioTransport::spawn(cmd, args, env, timeout_ms).await?);
        let name = name.into();
        let mut client = Self::finish_connect(name.clone(), transport).await?;
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
        let url_str: String = url.into();
        let transport: Arc<dyn McpTransport> = Arc::new(McpHttpTransport::new(
            url_str.clone(),
            auth_token.clone(),
            timeout_ms,
        ));
        let name = name.into();
        let mut client = Self::finish_connect(name.clone(), transport).await?;
        client.reconnect = Some(ReconnectConfig::Http {
            url: url_str,
            auth_token,
            timeout_ms,
        });
        Ok(client)
    }

    pub async fn connect_with_transport(
        name: impl Into<String>,
        transport: Arc<dyn McpTransport>,
    ) -> Result<Self, McpError> {
        Self::finish_connect(name.into(), transport).await
    }

    async fn finish_connect(
        name: String,
        transport: Arc<dyn McpTransport>,
    ) -> Result<Self, McpError> {
        let init_params = serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "atman", "version": env!("CARGO_PKG_VERSION")}
        });
        transport.call("initialize", init_params).await?;
        transport
            .notify("notifications/initialized", serde_json::Value::Null)
            .await?;
        let list = transport.call("tools/list", serde_json::json!({})).await?;
        let tools = parse_tools_list(&list)?;
        Ok(Self {
            name,
            transport: std::sync::Mutex::new(transport),
            tools,
            reconnect: None,
        })
    }

    pub fn transport_kind(&self) -> &'static str {
        let t = self.transport.lock().unwrap();
        t.kind()
    }

    async fn reconnect(&self) -> Result<(), McpError> {
        let cfg = self.reconnect.as_ref().ok_or(McpError::Disconnected)?;
        let new_transport: Arc<dyn McpTransport> = match cfg {
            ReconnectConfig::Stdio {
                cmd,
                args,
                env,
                timeout_ms,
            } => Arc::new(McpStdioTransport::spawn(cmd, args, env, *timeout_ms).await?),
            ReconnectConfig::Http {
                url,
                auth_token,
                timeout_ms,
            } => Arc::new(McpHttpTransport::new(url, auth_token.clone(), *timeout_ms)),
        };
        // Re-initialize the new transport.
        let init_params = serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "atman", "version": env!("CARGO_PKG_VERSION")}
        });
        new_transport.call("initialize", init_params).await?;
        new_transport
            .notify("notifications/initialized", serde_json::Value::Null)
            .await?;
        *self.transport.lock().unwrap() = new_transport;
        Ok(())
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
        let transport = { self.transport.lock().unwrap().clone() };
        let result = transport.call("tools/call", params.clone()).await;
        match result {
            Err(McpError::Disconnected) => {
                self.reconnect().await?;
                let transport = { self.transport.lock().unwrap().clone() };
                let retry = transport.call("tools/call", params).await?;
                Ok(mcp_result_to_value(retry))
            }
            other => Ok(mcp_result_to_value(other?)),
        }
    }

    pub async fn list_resources(&self) -> Result<Vec<McpResource>, McpError> {
        let transport = { self.transport.lock().unwrap().clone() };
        let v = transport
            .call("resources/list", serde_json::json!({}))
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
        let transport = { self.transport.lock().unwrap().clone() };
        let v = transport.call("resources/read", params).await?;
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
        let transport = { self.transport.lock().unwrap().clone() };
        let v = transport
            .call("prompts/list", serde_json::json!({}))
            .await?;
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
        let transport = { self.transport.lock().unwrap().clone() };
        let v = transport.call("prompts/get", params).await?;
        serde_json::from_value(v)
            .map_err(|e| McpError::Protocol(format!("prompts/get parse error: {e}")))
    }
}

fn parse_tools_list(v: &serde_json::Value) -> Result<Vec<McpToolSchema>, McpError> {
    let arr = v.get("tools").and_then(|t| t.as_array()).ok_or_else(|| {
        McpError::Protocol(format!("tools/list response missing `tools` array: {v}"))
    })?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let schema: McpToolSchema = serde_json::from_value(item.clone())
            .map_err(|e| McpError::Protocol(format!("tool schema: {e}")))?;
        out.push(schema);
    }
    Ok(out)
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

pub fn value_to_mcp_args(args: &crate::tool::ToolArgs) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (name, value) in &args.named {
        map.insert(name.clone(), value.to_json());
    }
    serde_json::Value::Object(map)
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
    description: Option<String>,
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
        Self {
            qualified_name,
            tool_name,
            tier,
            client,
            reconcile,
            schema,
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

    fn call<'a>(
        &'a self,
        args: crate::tool::ToolArgs,
        _ctx: &'a crate::tool::ToolCtx,
    ) -> crate::tool::BoxFut<'a, crate::tool::ToolResult> {
        Box::pin(async move {
            let params = value_to_mcp_args(&args);
            let params = if let Some(plan) = &self.reconcile {
                let map = match params {
                    serde_json::Value::Object(m) => m,
                    _ => return Err(RuntimeError::ToolFailed("mcp: expected object args".into())),
                };
                reconcile(plan, map)
            } else {
                params
            };
            let v = self.client.call_tool(&self.tool_name, params).await?;
            Ok(v)
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerState {
    Disabled,
    Pending,
    Connecting,
    Connected { tool_count: usize },
    Error { message: String },
    Disconnected { message: String },
    Timeout { message: String },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransportKind {
    #[default]
    Stdio,
    Http,
    Sse,
}

pub struct McpServerConfig {
    pub name: String,
    #[allow(clippy::field_reassign_with_default)]
    pub transport: TransportKind,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub url: Option<String>,
    pub auth_token: Option<String>,
    pub headers: Vec<(String, String)>,
    pub tier: crate::tool::Tier,
    pub timeout_ms: u64,
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
    reg: &mut crate::tool::ToolRegistry,
    configs: &[McpServerConfig],
) -> Vec<Result<McpClientStatus, McpBootError>> {
    let mut out = Vec::with_capacity(configs.len());
    for cfg in configs {
        let outcome = match cfg.transport {
            TransportKind::Stdio => {
                McpClient::connect_stdio(
                    &cfg.name,
                    &cfg.command,
                    &cfg.args,
                    &cfg.env,
                    cfg.timeout_ms,
                )
                .await
            }
            TransportKind::Http => match cfg.url.as_deref() {
                Some(url) => {
                    McpClient::connect_http(&cfg.name, url, cfg.auth_token.clone(), cfg.timeout_ms)
                        .await
                }
                None => Err(McpError::Protocol("http transport requires `url`".into())),
            },
            TransportKind::Sse => match cfg.url.as_deref() {
                Some(url) => {
                    McpClient::connect_http(&cfg.name, url, cfg.auth_token.clone(), cfg.timeout_ms)
                        .await
                }
                None => Err(McpError::Protocol("sse transport requires `url`".into())),
            },
        };
        match outcome {
            Ok(client) => {
                let tool_count = client.tools.len();
                let transport_kind = client.transport_kind();
                let arc_client = Arc::new(client);
                for tool in &arc_client.tools {
                    let adapter = McpToolAdapter::new(
                        arc_client.clone(),
                        &tool.name,
                        cfg.tier,
                        tool.input_schema.as_ref(),
                        tool.description.as_deref(),
                    );
                    reg.register(Arc::new(adapter));
                }
                out.push(Ok(McpClientStatus {
                    name: cfg.name.clone(),
                    tool_count,
                    transport: transport_kind,
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

#[derive(Debug)]
pub struct McpClientStatus {
    pub name: String,
    pub tool_count: usize,
    pub transport: &'static str,
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

        let transport =
            McpStdioTransport::spawn("python3", &[script_path.display().to_string()], &[], 5000)
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

        let transport =
            McpStdioTransport::spawn("python3", &[script_path.display().to_string()], &[], 5000)
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

        let transport =
            McpStdioTransport::spawn("python3", &[script_path.display().to_string()], &[], 200)
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
