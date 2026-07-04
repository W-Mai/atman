use std::collections::HashMap;
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

    fn kind(&self) -> &'static str {
        "stdio"
    }
}

impl McpStdioTransport {
    pub async fn spawn(cmd: &str, args: &[String], timeout_ms: u64) -> Result<Self, McpError> {
        let mut child = Command::new(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
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
}

impl McpTransport for McpHttpTransport {
    fn call<'a>(
        &'a self,
        method: &'a str,
        params: serde_json::Value,
    ) -> BoxFut<'a, Result<serde_json::Value, McpError>> {
        Box::pin(self.call_http(method, params))
    }

    fn kind(&self) -> &'static str {
        "http"
    }
}

pub struct McpClient {
    pub name: String,
    transport: Arc<dyn McpTransport>,
    pub tools: Vec<McpToolSchema>,
}

impl McpClient {
    pub async fn connect_stdio(
        name: impl Into<String>,
        cmd: &str,
        args: &[String],
        timeout_ms: u64,
    ) -> Result<Self, McpError> {
        let transport: Arc<dyn McpTransport> =
            Arc::new(McpStdioTransport::spawn(cmd, args, timeout_ms).await?);
        Self::finish_connect(name.into(), transport).await
    }

    pub async fn connect_http(
        name: impl Into<String>,
        url: impl Into<String>,
        auth_token: Option<String>,
        timeout_ms: u64,
    ) -> Result<Self, McpError> {
        let transport: Arc<dyn McpTransport> =
            Arc::new(McpHttpTransport::new(url, auth_token, timeout_ms));
        Self::finish_connect(name.into(), transport).await
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
        let _ = transport
            .call("notifications/initialized", serde_json::Value::Null)
            .await;
        let list = transport.call("tools/list", serde_json::json!({})).await?;
        let tools = parse_tools_list(&list)?;
        Ok(Self {
            name,
            transport,
            tools,
        })
    }

    pub fn transport_kind(&self) -> &'static str {
        self.transport.kind()
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
        let result = self.transport.call("tools/call", params).await?;
        Ok(mcp_result_to_value(result))
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

pub struct McpToolAdapter {
    qualified_name: String,
    tool_name: String,
    tier: crate::tool::Tier,
    client: Arc<McpClient>,
}

impl McpToolAdapter {
    pub fn new(
        client: Arc<McpClient>,
        tool_name: impl Into<String>,
        tier: crate::tool::Tier,
    ) -> Self {
        let tool_name = tool_name.into();
        let qualified_name = format!("{}.{}", client.name, tool_name);
        Self {
            qualified_name,
            tool_name,
            tier,
            client,
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

    fn call<'a>(
        &'a self,
        args: crate::tool::ToolArgs,
        _ctx: &'a crate::tool::ToolCtx,
    ) -> crate::tool::BoxFut<'a, crate::tool::ToolResult> {
        Box::pin(async move {
            let params = value_to_mcp_args(&args);
            let v = self.client.call_tool(&self.tool_name, params).await?;
            Ok(v)
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransportKind {
    #[default]
    Stdio,
    Http,
}

pub struct McpServerConfig {
    pub name: String,
    #[allow(clippy::field_reassign_with_default)]
    pub transport: TransportKind,
    pub command: String,
    pub args: Vec<String>,
    pub url: Option<String>,
    pub auth_token: Option<String>,
    pub tier: crate::tool::Tier,
    pub timeout_ms: u64,
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
            url: None,
            auth_token: None,
            tier,
            timeout_ms,
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
            url: Some(url.into()),
            auth_token,
            tier,
            timeout_ms,
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
                McpClient::connect_stdio(&cfg.name, &cfg.command, &cfg.args, cfg.timeout_ms).await
            }
            TransportKind::Http => match cfg.url.as_deref() {
                Some(url) => {
                    McpClient::connect_http(&cfg.name, url, cfg.auth_token.clone(), cfg.timeout_ms)
                        .await
                }
                None => Err(McpError::Protocol("http transport requires `url`".into())),
            },
        };
        match outcome {
            Ok(client) => {
                let tool_count = client.tools.len();
                let transport_kind = client.transport_kind();
                let arc_client = Arc::new(client);
                for tool in &arc_client.tools {
                    let adapter = McpToolAdapter::new(arc_client.clone(), &tool.name, cfg.tier);
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
            McpStdioTransport::spawn("python3", &[script_path.display().to_string()], 5000)
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
            McpStdioTransport::spawn("python3", &[script_path.display().to_string()], 5000)
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
            McpStdioTransport::spawn("python3", &[script_path.display().to_string()], 200)
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
}
