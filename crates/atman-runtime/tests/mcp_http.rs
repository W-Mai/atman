use atman_runtime::mcp::{McpServerConfig, TransportKind, register_from_configs};
use atman_runtime::tool::{Tier, ToolArgs, ToolCtx, ToolRegistry};
use atman_runtime::value::Value;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn mount_mcp_stub(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/rpc"))
        .and(body_partial_json(
            serde_json::json!({"method": "initialize"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": { "protocolVersion": "2024-11-05", "capabilities": {} }
        })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/rpc"))
        .and(body_partial_json(
            serde_json::json!({"method": "notifications/initialized"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "result": null
        })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/rpc"))
        .and(body_partial_json(
            serde_json::json!({"method": "tools/list"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "result": {
                "tools": [{
                    "name": "echo",
                    "description": "echoes text",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"text": {"type": "string"}}
                    }
                }]
            }
        })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/rpc"))
        .and(body_partial_json(
            serde_json::json!({"method": "tools/call"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 4,
            "result": {
                "content": [{"type": "text", "text": "echoed: over the wire"}],
                "isError": false
            }
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn http_mcp_server_completes_full_tool_registry_roundtrip() {
    let server = MockServer::start().await;
    mount_mcp_stub(&server).await;

    let mut reg = ToolRegistry::new();
    let cfg = McpServerConfig::http(
        "cloud",
        format!("{}/rpc", server.uri()),
        None,
        Tier::Three,
        5000,
    );
    let statuses = register_from_configs(&mut reg, &[cfg]).await;
    assert_eq!(statuses.len(), 1);
    let status = statuses[0]
        .as_ref()
        .expect("http mcp connect should succeed");
    assert_eq!(status.name, "cloud");
    assert_eq!(status.transport, "http");
    assert_eq!(status.tool_count, 1);

    let tool = reg
        .get("cloud.echo")
        .expect("tool registered under `cloud.echo`");
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
    let Value::Struct(fields) = out else {
        panic!("expected struct");
    };
    let text = fields.iter().find(|(k, _)| k == "text").unwrap().1.clone();
    assert!(matches!(&text, Value::Str(s) if s.contains("over the wire")));
}

#[tokio::test]
async fn http_mcp_sends_bearer_token_when_configured() {
    use wiremock::matchers::header;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/rpc"))
        .and(header("authorization", "Bearer secret-abc"))
        .and(body_partial_json(
            serde_json::json!({"method": "initialize"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": { "protocolVersion": "2024-11-05", "capabilities": {} }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/rpc"))
        .and(header("authorization", "Bearer secret-abc"))
        .and(body_partial_json(
            serde_json::json!({"method": "notifications/initialized"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "result": null
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/rpc"))
        .and(header("authorization", "Bearer secret-abc"))
        .and(body_partial_json(
            serde_json::json!({"method": "tools/list"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "result": {"tools": []}
        })))
        .mount(&server)
        .await;

    let mut reg = ToolRegistry::new();
    let cfg = McpServerConfig::http(
        "auth-cloud",
        format!("{}/rpc", server.uri()),
        Some("secret-abc".to_string()),
        Tier::Three,
        5000,
    );
    let statuses = register_from_configs(&mut reg, &[cfg]).await;
    let msg = statuses[0].as_ref().err().map(|e| e.error.to_string());
    assert!(
        statuses[0].is_ok(),
        "expected bearer auth to succeed, got: {msg:?}"
    );
}

#[tokio::test]
async fn http_mcp_returns_error_when_server_returns_5xx_after_retries() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/rpc"))
        .respond_with(ResponseTemplate::new(503).set_body_string("nope"))
        .mount(&server)
        .await;

    let mut reg = ToolRegistry::new();
    let cfg = McpServerConfig::http(
        "flaky",
        format!("{}/rpc", server.uri()),
        None,
        Tier::Three,
        10_000,
    );
    let statuses = register_from_configs(&mut reg, &[cfg]).await;
    assert_eq!(statuses.len(), 1);
    let err = match &statuses[0] {
        Err(e) => e,
        Ok(_) => panic!("expected boot error"),
    };
    let msg = format!("{}", err.error);
    assert!(msg.contains("503"), "err: {msg}");
}

#[tokio::test]
async fn http_mcp_config_without_url_reports_protocol_error() {
    let mut reg = ToolRegistry::new();
    let cfg = McpServerConfig {
        name: "no-url".into(),
        transport: TransportKind::Http,
        command: String::new(),
        args: vec![],
        url: None,
        auth_token: None,
        tier: Tier::Three,
        timeout_ms: 500,
    };
    let statuses = register_from_configs(&mut reg, &[cfg]).await;
    let err = match &statuses[0] {
        Err(e) => e,
        Ok(_) => panic!("expected boot error"),
    };
    assert!(
        format!("{}", err.error).contains("url"),
        "err: {}",
        err.error
    );
}
