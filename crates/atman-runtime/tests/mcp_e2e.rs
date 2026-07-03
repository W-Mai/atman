use std::sync::Arc;

use atman_runtime::mcp::{McpServerConfig, register_from_configs};
use atman_runtime::tool::{Tier, ToolArgs, ToolCtx, ToolRegistry};
use atman_runtime::value::Value;

fn write_script(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    path
}

const MOCK_SERVER: &str = r#"
import sys, json

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    method = req["method"]
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": req["id"], "result": {"protocolVersion": "2024-11-05", "capabilities": {}}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": req["id"], "result": {"tools": [
            {"name": "echo", "description": "echoes text", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}
        ]}})
    elif method == "tools/call":
        name = req["params"]["name"]
        args = req["params"].get("arguments", {})
        if name == "echo":
            text = args.get("text", "")
            send({"jsonrpc": "2.0", "id": req["id"], "result": {"content": [{"type": "text", "text": f"echoed: {text}"}], "isError": False}})
        else:
            send({"jsonrpc": "2.0", "id": req["id"], "error": {"code": -32601, "message": f"unknown tool: {name}"}})
    else:
        send({"jsonrpc": "2.0", "id": req["id"], "error": {"code": -32601, "message": f"unknown method: {method}"}})
"#;

#[tokio::test]
async fn mcp_server_discovers_tools_and_calls_them_via_registry() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "mock_mcp.py", MOCK_SERVER);
    let mut reg = ToolRegistry::new();
    let cfg = McpServerConfig {
        name: "demo".into(),
        command: "python3".into(),
        args: vec![script.display().to_string()],
        tier: Tier::Three,
        timeout_ms: 5000,
    };
    let statuses = register_from_configs(&mut reg, &[cfg]).await;
    assert_eq!(statuses.len(), 1);
    let Ok(status) = &statuses[0] else {
        panic!("expected boot ok, got: {statuses:?}");
    };
    assert_eq!(status.name, "demo");
    assert_eq!(status.tool_count, 1);

    let tool = reg
        .get("demo.echo")
        .expect("tool should be registered under qualified name");
    let ctx = ToolCtx::new();
    let args = ToolArgs {
        positional: vec![],
        named: vec![("text".into(), Value::Str("hello mcp".into()))],
    };
    let v = tool.call(args, &ctx).await.unwrap();
    let Value::Struct(fields) = v else {
        panic!("expected struct, got {v:?}");
    };
    let text = fields
        .iter()
        .find(|(k, _)| k == "text")
        .map(|(_, v)| v.clone())
        .unwrap();
    assert!(
        matches!(&text, Value::Str(s) if s.contains("echoed: hello mcp")),
        "got: {text:?}"
    );
    let is_error = fields.iter().find(|(k, _)| k == "is_error").unwrap();
    assert!(matches!(is_error.1, Value::Bool(false)));
}

#[tokio::test]
async fn mcp_boot_error_when_command_missing_returns_err_but_does_not_panic() {
    let mut reg = ToolRegistry::new();
    let cfg = McpServerConfig {
        name: "missing".into(),
        command: "/no/such/binary/at/all".into(),
        args: vec![],
        tier: Tier::Three,
        timeout_ms: 500,
    };
    let statuses = register_from_configs(&mut reg, &[cfg]).await;
    assert_eq!(statuses.len(), 1);
    assert!(
        statuses[0].is_err(),
        "expected boot error for missing binary"
    );
    let name = match &statuses[0] {
        Err(e) => e.name.clone(),
        Ok(_) => unreachable!(),
    };
    assert_eq!(name, "missing");
}

#[tokio::test]
async fn mcp_qualified_tool_name_is_server_dot_tool() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "mock_mcp2.py", MOCK_SERVER);
    let mut reg = ToolRegistry::new();
    register_from_configs(
        &mut reg,
        &[McpServerConfig {
            name: "srv-a".into(),
            command: "python3".into(),
            args: vec![script.display().to_string()],
            tier: Tier::Three,
            timeout_ms: 5000,
        }],
    )
    .await;
    assert!(reg.get("srv-a.echo").is_some());
    assert!(reg.get("echo").is_none(), "bare name must not resolve");
}

#[tokio::test]
async fn mcp_two_servers_register_under_distinct_namespaces() {
    let dir = tempfile::tempdir().unwrap();
    let s1 = write_script(dir.path(), "mock_a.py", MOCK_SERVER);
    let s2 = write_script(dir.path(), "mock_b.py", MOCK_SERVER);
    let mut reg = ToolRegistry::new();
    let statuses = register_from_configs(
        &mut reg,
        &[
            McpServerConfig {
                name: "a".into(),
                command: "python3".into(),
                args: vec![s1.display().to_string()],
                tier: Tier::Three,
                timeout_ms: 5000,
            },
            McpServerConfig {
                name: "b".into(),
                command: "python3".into(),
                args: vec![s2.display().to_string()],
                tier: Tier::Three,
                timeout_ms: 5000,
            },
        ],
    )
    .await;
    assert_eq!(statuses.iter().filter(|s| s.is_ok()).count(), 2);
    assert!(reg.get("a.echo").is_some());
    assert!(reg.get("b.echo").is_some());
    // both tools point at their own client — call each and ensure they respond
    let ctx = ToolCtx::new();
    let out_a = Arc::new(reg.get("a.echo").unwrap())
        .call(
            ToolArgs {
                positional: vec![],
                named: vec![("text".into(), Value::Str("A".into()))],
            },
            &ctx,
        )
        .await
        .unwrap();
    let out_b = Arc::new(reg.get("b.echo").unwrap())
        .call(
            ToolArgs {
                positional: vec![],
                named: vec![("text".into(), Value::Str("B".into()))],
            },
            &ctx,
        )
        .await
        .unwrap();
    let extract_text = |v: Value| -> String {
        let Value::Struct(fields) = v else {
            panic!("expected struct");
        };
        let text = fields
            .iter()
            .find(|(k, _)| k == "text")
            .map(|(_, v)| v.clone())
            .unwrap();
        let Value::Str(s) = text else {
            panic!("expected str");
        };
        s
    };
    assert!(extract_text(out_a).contains("A"));
    assert!(extract_text(out_b).contains("B"));
}
