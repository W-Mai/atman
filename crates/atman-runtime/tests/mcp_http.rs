use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use atman_runtime::mcp::{
    McpClient, McpError, McpNotification, McpServerConfig, TransportKind, register_from_configs,
};
use atman_runtime::tool::{Tier, ToolArgs, ToolCtx, ToolRegistry};
use axum::http::HeaderMap;
use serde_json::{Value as JsonValue, json};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

type Value = atman_rt::Value<atman_runtime::AtmanPayload, atman_runtime::RuntimeError>;

const PROTOCOL_VERSION: &str = "2025-03-26";
const SESSION: &str = "session-a";

fn configured_server(url: String) -> McpServerConfig {
    let mut config =
        McpServerConfig::http("cloud", url, Some("secret-abc".into()), Tier::Three, 5000);
    config.headers = vec![("X-Api-Key".into(), "configured-key".into())];
    config
}

fn has_required_headers(headers: &HeaderMap, session: Option<&str>) -> bool {
    let accepts = headers
        .get_all("accept")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .collect::<Vec<_>>();
    accepts.contains(&"application/json")
        && accepts.contains(&"text/event-stream")
        && ["application/json", "text/event-stream"]
            .iter()
            .all(|required| {
                accepts
                    .iter()
                    .filter(|value| {
                        value
                            .split(';')
                            .next()
                            .unwrap_or_default()
                            .trim()
                            .eq_ignore_ascii_case(required)
                    })
                    .count()
                    == 1
            })
        && headers.get_all("authorization").iter().count() == 1
        && headers
            .get("authorization")
            .is_some_and(|value| value == "Bearer secret-abc")
        && headers
            .get("content-type")
            .is_some_and(|value| value == "application/json")
        && headers
            .get("x-api-key")
            .is_some_and(|value| value == "configured-key")
        && match session {
            Some(session) => {
                headers
                    .get("mcp-session-id")
                    .is_some_and(|value| value == session)
                    && headers
                        .get("mcp-protocol-version")
                        .is_some_and(|value| value == PROTOCOL_VERSION)
            }
            None => {
                !headers.contains_key("mcp-session-id")
                    && !headers.contains_key("mcp-protocol-version")
            }
        }
}

fn initialization() -> JsonValue {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "serverInfo": {"name": "test-server", "version": "1"}
    })
}

fn tools() -> JsonValue {
    json!({"tools": [{
        "name": "echo",
        "description": "echoes text",
        "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}
    }]})
}

fn content(text: &str) -> JsonValue {
    json!({"content": [{"type": "text", "text": text}], "isError": false})
}

fn rpc_response(body: &JsonValue, result: JsonValue) -> JsonValue {
    json!({"jsonrpc": "2.0", "id": body["id"], "result": result})
}

async fn mount_mcp_stub(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/rpc"))
        .respond_with(|request: &Request| {
            let body: JsonValue = request.body_json().unwrap();
            let method = body["method"].as_str().unwrap();
            let session = (method != "initialize").then_some(SESSION);
            if !has_required_headers(&request.headers, session) {
                return ResponseTemplate::new(400).set_body_string("missing MCP headers");
            }
            let result = match method {
                "initialize" => {
                    return ResponseTemplate::new(200)
                        .insert_header("Mcp-Session-Id", SESSION)
                        .set_body_json(rpc_response(&body, initialization()));
                }
                "notifications/initialized" if body.get("id").is_none() => {
                    return ResponseTemplate::new(202);
                }
                "tools/list" => tools(),
                "tools/call" => content("echoed: over the wire"),
                _ => return ResponseTemplate::new(400),
            };
            ResponseTemplate::new(200).set_body_json(rpc_response(&body, result))
        })
        .mount(server)
        .await;
}

fn result_text(value: &Value) -> &str {
    let Value::Struct(fields) = value else {
        panic!("expected MCP result struct");
    };
    let Some((_, Value::Str(text))) = fields.iter().find(|(key, _)| key == "text") else {
        panic!("expected MCP text content");
    };
    text
}

#[tokio::test]
async fn http_mcp_registry_roundtrip_preserves_configured_headers_and_negotiated_session() {
    let server = MockServer::start().await;
    mount_mcp_stub(&server).await;

    let reg = ToolRegistry::new();
    let configs = atman_runtime::mcp_config::parse_mcp_json(
        &json!({"mcpServers": {"cloud": {
            "type": "http",
            "url": format!("{}/rpc", server.uri()),
            "authToken": "secret-abc",
            "timeout_ms": 5000,
            "headers": {
                "X-Api-Key": "configured-key",
                "AcCePt": "application/json;q=0, text/event-stream;q=0, application/example",
                "aUtHoRiZaTiOn": "Bearer overridden-token",
                "Content-Type": "text/plain",
                "MCP-Protocol-Version": "stale-version",
                "Mcp-Session-Id": "stale-session"
            }
        }}})
        .to_string(),
    );
    assert_eq!(configs.len(), 1);
    let statuses = register_from_configs(&reg, &configs).await;
    assert_eq!(statuses.len(), 1);
    let status = statuses[0].as_ref().expect("http MCP connects");
    assert_eq!(status.name, "cloud");
    assert_eq!(status.transport, "http");
    assert_eq!(status.tool_count, 1);

    let tool = reg.get("mcp.cloud.echo").expect("MCP tool registered");
    let out = tool
        .call(
            ToolArgs {
                positional: vec![],
                named: vec![("text".into(), Value::Str("over the wire".into()))],
            },
            &ToolCtx::new(),
        )
        .await
        .unwrap();
    assert!(result_text(&out).contains("over the wire"));
    let requests = server.received_requests().await.unwrap();
    assert!(
        requests.iter().all(|request| {
            request
                .headers
                .get_all("accept")
                .iter()
                .filter_map(|value| value.to_str().ok())
                .flat_map(|value| value.split(','))
                .any(|media| media.trim() == "application/example")
        }),
        "additional configured Accept media must survive every request"
    );
    let methods = requests
        .iter()
        .map(|request| {
            request.body_json::<JsonValue>().unwrap()["method"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        methods,
        [
            "initialize",
            "notifications/initialized",
            "tools/list",
            "tools/call"
        ]
    );
}

#[tokio::test]
async fn http_mcp_initialized_notification_failure_or_timeout_aborts_connection() {
    for (response, expected) in [
        (ResponseTemplate::new(502), Some("502")),
        (
            ResponseTemplate::new(202).set_delay(Duration::from_secs(3)),
            None,
        ),
    ] {
        let server = MockServer::start().await;
        mount_mcp_stub(&server).await;
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "notifications/initialized"}),
            ))
            .respond_with(response)
            .with_priority(1)
            .expect(1)
            .mount(&server)
            .await;
        let mut cfg = configured_server(format!("{}/rpc", server.uri()));
        cfg.timeout_ms = 500;
        let error = tokio::time::timeout(Duration::from_secs(2), McpClient::connect_config(&cfg))
            .await
            .expect("initialized notification obeys the configured timeout")
            .err()
            .expect("failed initialization notification must not connect");
        if let Some(status) = expected {
            assert!(error.to_string().contains(status), "{error}");
        } else {
            assert!(
                matches!(error, McpError::Timeout { ref method, .. } if method == "notifications/initialized"),
                "{error}"
            );
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests.len(),
            2,
            "tools/list must not run after failed notify"
        );
        assert_eq!(
            requests[1].body_json::<JsonValue>().unwrap()["method"],
            "notifications/initialized"
        );
    }
}

struct HttpServerTask(tokio::task::JoinHandle<()>);

impl Drop for HttpServerTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[tokio::test]
async fn http_mcp_sse_returns_matching_response_before_stream_closes() {
    use axum::body::{Body, Bytes};
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use futures::StreamExt;

    async fn handle(
        State(replies): State<Arc<tokio::sync::Notify>>,
        headers: HeaderMap,
        axum::Json(body): axum::Json<JsonValue>,
    ) -> Response {
        let method = body["method"].as_str();
        if !has_required_headers(&headers, (method != Some("initialize")).then_some(SESSION)) {
            return (StatusCode::BAD_REQUEST, "missing MCP headers").into_response();
        }
        match method {
            Some("initialize") => (
                [("Mcp-Session-Id", SESSION)],
                axum::Json(rpc_response(&body, initialization())),
            )
                .into_response(),
            Some("notifications/initialized") => StatusCode::ACCEPTED.into_response(),
            Some("tools/list") => axum::Json(rpc_response(&body, tools())).into_response(),
            None if body == json!({"jsonrpc": "2.0", "id": "server-ping", "result": {}}) => {
                replies.notify_one();
                StatusCode::ACCEPTED.into_response()
            }
            None if body["jsonrpc"] == "2.0"
                && body["id"] == "server-unknown"
                && body["error"]["code"] == -32601
                && body.get("method").is_none()
                && body.get("result").is_none() =>
            {
                replies.notify_one();
                StatusCode::ACCEPTED.into_response()
            }
            Some("tools/call") => {
                let id = body["id"].as_u64().unwrap();
                let events = format!(
                    ": heartbeat\r\nevent: message\r\ndata: {{\"jsonrpc\":\"2.0\",\r\ndata: \"method\":\"notifications/tools/list_changed\"}}\r\n\r\n\
                     data: {{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{}}}\n\n\
                     data: {{\"jsonrpc\":\"2.0\",\"id\":\"server-ping\",\"method\":\"ping\",\"params\":{{}}}}\r\n\r\n",
                    id + 1000,
                    content("wrong response"),
                );
                let (field, outcome) = if body["params"]["arguments"]["fail"] == true {
                    (
                        "error",
                        json!({"code": -32602, "message": "invalid arguments"}),
                    )
                } else {
                    ("result", content("streamed 中文"))
                };
                let result_events = format!(
                    "event: message\r\ndata: [{{\"jsonrpc\":\"2.0\",\"id\":{id},\r\ndata: \"{field}\":{outcome}}},{{\"jsonrpc\":\"2.0\",\"method\":\"notifications/prompts/list_changed\"}}]\r\n\r\n",
                );
                let chunks = events
                    .as_bytes()
                    .chunks(3)
                    .map(Bytes::copy_from_slice)
                    .collect::<Vec<_>>();
                let result_chunks = result_events
                    .as_bytes()
                    .chunks(3)
                    .map(Bytes::copy_from_slice)
                    .collect::<Vec<_>>();
                let error_reply = replies.clone();
                let unknown_request = futures::stream::once(async move {
                    replies.notified().await;
                    Ok::<_, std::io::Error>(Bytes::from_static(
                        b"data: {\"jsonrpc\":\"2.0\",\"id\":\"server-unknown\",\"method\":\"server/unknown\"}\n\n",
                    ))
                });
                let result_stream = futures::stream::once(async move {
                    error_reply.notified().await;
                    futures::stream::iter(result_chunks.into_iter().map(Ok::<_, std::io::Error>))
                })
                .flatten();
                let stream = futures::stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>))
                    .chain(unknown_request)
                    .chain(result_stream)
                    .chain(futures::stream::pending());
                Response::builder()
                    .header("Content-Type", "text/event-stream")
                    .body(Body::from_stream(stream))
                    .unwrap()
            }
            _ => StatusCode::NOT_FOUND.into_response(),
        }
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let _server = HttpServerTask(tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new()
                .route("/rpc", axum::routing::post(handle))
                .with_state(Arc::new(tokio::sync::Notify::new())),
        )
        .await
        .unwrap();
    }));
    let cfg = configured_server(format!("http://{address}/rpc"));
    let client = McpClient::connect_config(&cfg).await.unwrap();
    let mut notifications = client.subscribe_notifications();
    let out = tokio::time::timeout(Duration::from_secs(2), client.call_tool("echo", json!({})))
        .await
        .expect("matching SSE response must complete without waiting for EOF")
        .unwrap();
    assert_eq!(result_text(&out), "streamed 中文");
    assert!(matches!(
        notifications.try_recv(),
        Ok(McpNotification::ToolsListChanged)
    ));
    assert!(matches!(
        notifications.try_recv(),
        Ok(McpNotification::PromptsListChanged)
    ));
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        client.call_tool("echo", json!({"fail": true})),
    )
    .await
    .expect("matching SSE error must complete without waiting for EOF")
    .unwrap_err();
    assert!(matches!(error, McpError::ServerError { code: -32602, .. }));
    assert!(matches!(
        notifications.try_recv(),
        Ok(McpNotification::ToolsListChanged)
    ));
    assert!(matches!(
        notifications.try_recv(),
        Ok(McpNotification::PromptsListChanged)
    ));
}

#[tokio::test]
async fn http_mcp_reconnects_expired_session_without_retrying_uncertain_tool_execution() {
    let server = MockServer::start().await;
    let initializations = Arc::new(AtomicUsize::new(0));
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let notifications = Arc::new(AtomicUsize::new(0));
    let init_count = initializations.clone();
    let call_count = tool_calls.clone();
    let notification_count = notifications.clone();
    Mock::given(method("POST"))
        .and(path("/rpc"))
        .respond_with(move |request: &Request| {
            let body: JsonValue = request.body_json().unwrap();
            let method = body["method"].as_str().unwrap();
            let session = format!("session-{}", init_count.load(Ordering::SeqCst));
            if !has_required_headers(
                &request.headers,
                (method != "initialize").then_some(session.as_str()),
            ) {
                return ResponseTemplate::new(400)
                    .set_body_string("stale session or missing headers");
            }
            let result = match method {
                "initialize" => {
                    let next = init_count.fetch_add(1, Ordering::SeqCst) + 1;
                    return ResponseTemplate::new(200)
                        .insert_header("Mcp-Session-Id", format!("session-{next}"))
                        .set_body_json(rpc_response(&body, initialization()));
                }
                "notifications/initialized" => {
                    notification_count.fetch_add(1, Ordering::SeqCst);
                    return ResponseTemplate::new(202);
                }
                "tools/list" => tools(),
                "tools/call" => {
                    call_count.fetch_add(1, Ordering::SeqCst);
                    if body["params"]["arguments"]["uncertain"] == true {
                        return ResponseTemplate::new(503)
                            .set_body_string("result delivery failed");
                    }
                    if init_count.load(Ordering::SeqCst) == 1 {
                        return ResponseTemplate::new(404).set_body_string("session expired");
                    }
                    content("reconnected")
                }
                _ => return ResponseTemplate::new(400),
            };
            ResponseTemplate::new(200).set_body_json(rpc_response(&body, result))
        })
        .mount(&server)
        .await;

    let cfg = configured_server(format!("{}/rpc", server.uri()));
    let client = McpClient::connect_config(&cfg).await.unwrap();
    let out = client.call_tool("echo", json!({})).await.unwrap();
    assert_eq!(result_text(&out), "reconnected");
    assert_eq!(initializations.load(Ordering::SeqCst), 2);
    assert_eq!(notifications.load(Ordering::SeqCst), 2);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 2);

    let error = client
        .call_tool("echo", json!({"uncertain": true}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("503"), "{error}");
    assert_eq!(
        tool_calls.load(Ordering::SeqCst),
        3,
        "tool execution must not retry after ambiguous HTTP failure"
    );
    assert_eq!(
        initializations.load(Ordering::SeqCst),
        2,
        "ambiguous failure must not trigger reconnect and replay"
    );
}

#[tokio::test]
async fn http_mcp_returns_error_when_initialization_returns_5xx() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/rpc"))
        .respond_with(ResponseTemplate::new(503).set_body_string("nope"))
        .mount(&server)
        .await;

    let reg = ToolRegistry::new();
    let cfg = configured_server(format!("{}/rpc", server.uri()));
    let statuses = register_from_configs(&reg, &[cfg]).await;
    assert_eq!(statuses.len(), 1);
    let err = statuses[0].as_ref().expect_err("expected boot error");
    assert!(err.error.to_string().contains("503"), "err: {}", err.error);
}

#[tokio::test]
async fn http_mcp_config_without_url_reports_protocol_error() {
    let reg = ToolRegistry::new();
    let cfg = McpServerConfig {
        name: "no-url".into(),
        transport: TransportKind::Http,
        command: String::new(),
        args: vec![],
        env: vec![],
        url: None,
        auth_token: None,
        headers: vec![],
        tier: Tier::Three,
        timeout_ms: 500,
        disabled: false,
    };
    let statuses = register_from_configs(&reg, &[cfg]).await;
    let err = statuses[0].as_ref().expect_err("expected boot error");
    assert!(err.error.to_string().contains("url"), "err: {}", err.error);
}
