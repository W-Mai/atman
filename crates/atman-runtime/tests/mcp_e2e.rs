use std::sync::Arc;

type Value = atman_rt::Value<atman_runtime::AtmanPayload, atman_runtime::RuntimeError>;
use atman_runtime::mcp::{McpServerConfig, register_from_configs};
use atman_runtime::tool::{Tier, ToolArgs, ToolCtx, ToolRegistry};

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

const OWNED_SERVER: &str = r#"
import json, os, sys
from pathlib import Path

with open("pids", "a") as f:
    f.write(str(os.getpid()) + "\n")
sys.stderr.write("x" * (512 * 1024))
sys.stderr.flush()
for line in sys.stdin:
    req = json.loads(line)
    method = req["method"]
    if "id" not in req:
        continue
    result = {}
    if method == "initialize":
        if os.environ.get("MODE") == "pending":
            Path("ready").touch()
            continue
        result = {"protocolVersion": "2024-11-05", "capabilities": {}}
    elif method == "tools/list":
        if os.environ.get("MODE") != "invalid":
            result = {"tools": [{"name": "probe", "inputSchema": {"type": "object"}}]}
    elif method == "tools/call":
        action = req["params"]["arguments"].get("action")
        if action == "reconnect" and not Path("reconnected").exists():
            Path("reconnected").touch()
            sys.exit(0)
        if action == "wait":
            Path("ready").touch()
            continue
        if action == "sample":
            print(json.dumps({"jsonrpc": "2.0", "id": "sample", "method": "sampling/createMessage", "params": {"messages": []}}), flush=True)
            continue
        result = {"content": [{"type": "text", "text": os.getcwd()}]}
    print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": result}), flush=True)
"#;

fn owned_config(dir: &std::path::Path) -> McpServerConfig {
    write_script(dir, "owned_server.py", OWNED_SERVER);
    McpServerConfig::stdio(
        "owned",
        "python3",
        vec!["owned_server.py".into()],
        Tier::Zero,
        10_000,
    )
}

async fn wait_for_file(path: &std::path::Path) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !path.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[cfg(unix)]
async fn assert_processes_stopped(dir: &std::path::Path) {
    let pids = std::fs::read_to_string(dir.join("pids")).unwrap();
    for pid in pids.lines() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let alive = tokio::process::Command::new("kill")
                    .args(["-0", pid])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .await
                    .unwrap()
                    .success();
                if !alive {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn owned_mcp_reconnects_in_its_directory_and_shutdown_cannot_resurrect_it() {
    use atman_runtime::{
        ValueJson,
        mcp::{McpClient, McpError},
    };
    let dir = tempfile::tempdir().unwrap();
    let cfg = owned_config(dir.path());
    assert!(
        McpClient::connect_config_in(&cfg, Some(std::path::Path::new("relative")))
            .await
            .is_err()
    );
    let client = Arc::new(
        McpClient::connect_config_in(&cfg, Some(dir.path()))
            .await
            .unwrap(),
    );
    let value = client
        .call_tool("probe", serde_json::json!({"action":"reconnect"}))
        .await
        .unwrap();
    assert_eq!(
        std::fs::canonicalize(value.to_json().unwrap()["text"].as_str().unwrap()).unwrap(),
        std::fs::canonicalize(dir.path()).unwrap()
    );
    let count = std::fs::read_to_string(dir.path().join("pids"))
        .unwrap()
        .lines()
        .count();
    assert_eq!(count, 2);
    let waiting = {
        let client = client.clone();
        tokio::spawn(async move {
            client
                .call_tool("probe", serde_json::json!({"action":"wait"}))
                .await
        })
    };
    wait_for_file(&dir.path().join("ready")).await;
    client.shutdown();
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap(),
        Err(McpError::Disconnected)
    ));
    assert!(matches!(
        client.call_tool("probe", serde_json::json!({})).await,
        Err(McpError::Disconnected)
    ));
    assert!(matches!(
        client.refresh_tool_snapshot().await,
        Err(McpError::Disconnected)
    ));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("pids"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    #[cfg(unix)]
    assert_processes_stopped(dir.path()).await;
}

#[tokio::test]
async fn owned_mcp_releases_processes_after_failed_abandoned_and_completed_connections() {
    use atman_runtime::mcp::McpClient;
    for mode in ["invalid", "pending", "connected"] {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = owned_config(dir.path());
        cfg.env.push(("MODE".into(), mode.into()));
        if mode == "pending" {
            let cwd = dir.path().to_path_buf();
            let connecting =
                tokio::spawn(async move { McpClient::connect_config_in(&cfg, Some(&cwd)).await });
            wait_for_file(&dir.path().join("ready")).await;
            connecting.abort();
            assert!(matches!(connecting.await, Err(error) if error.is_cancelled()));
        } else {
            let result = McpClient::connect_config_in(&cfg, Some(dir.path())).await;
            assert_eq!(result.is_ok(), mode == "connected");
            drop(result);
        }
        #[cfg(unix)]
        assert_processes_stopped(dir.path()).await;
    }
}

#[tokio::test]
async fn owned_mcp_shutdown_cancels_sampling_owned_by_its_reader() {
    use atman_runtime::mcp::McpClient;
    let dir = tempfile::tempdir().unwrap();
    let client = Arc::new(
        McpClient::connect_config_in(&owned_config(dir.path()), Some(dir.path()))
            .await
            .unwrap(),
    );
    let started = Arc::new(tokio::sync::Notify::new());
    let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel::<()>();
    let dropped = Arc::new(std::sync::Mutex::new(Some(dropped_tx)));
    client.set_sampling_handler(Arc::new({
        let started = started.clone();
        move |_| {
            let started = started.clone();
            let drop_guard = dropped.lock().unwrap().take().unwrap();
            Box::pin(async move {
                let _drop_guard = drop_guard;
                started.notify_one();
                std::future::pending().await
            })
        }
    }));
    let task = {
        let client = client.clone();
        tokio::spawn(async move {
            client
                .call_tool("probe", serde_json::json!({"action":"sample"}))
                .await
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    client.shutdown();
    assert!(task.await.unwrap().is_err());
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(2), dropped_rx)
            .await
            .unwrap()
            .is_err()
    );
    #[cfg(unix)]
    assert_processes_stopped(dir.path()).await;
}

#[tokio::test]
async fn mcp_server_discovers_tools_and_calls_them_via_registry() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "mock_mcp.py", MOCK_SERVER);
    let reg = ToolRegistry::new();
    let cfg = McpServerConfig::stdio(
        "demo",
        "python3",
        vec![script.display().to_string()],
        Tier::Three,
        5000,
    );
    let statuses = register_from_configs(&reg, &[cfg]).await;
    assert_eq!(statuses.len(), 1);
    let Ok(status) = &statuses[0] else {
        panic!("expected boot ok, got: {statuses:?}");
    };
    assert_eq!(status.name, "demo");
    assert_eq!(status.tool_count, 1);
    assert_eq!(status.transport, "stdio");

    let tool = reg
        .get("mcp.demo.echo")
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
    let reg = ToolRegistry::new();
    let cfg = McpServerConfig::stdio(
        "missing",
        "/no/such/binary/at/all",
        vec![],
        Tier::Three,
        500,
    );
    let statuses = register_from_configs(&reg, &[cfg]).await;
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
async fn mcp_qualified_tool_name_is_mcp_dot_server_dot_tool() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "mock_mcp2.py", MOCK_SERVER);
    let reg = ToolRegistry::new();
    register_from_configs(
        &reg,
        &[McpServerConfig::stdio(
            "srv-a",
            "python3",
            vec![script.display().to_string()],
            Tier::Three,
            5000,
        )],
    )
    .await;
    assert!(reg.get("mcp.srv-a.echo").is_some());
    assert!(reg.get("echo").is_none(), "bare name must not resolve");
}

#[tokio::test]
async fn mcp_two_servers_register_under_distinct_namespaces() {
    let dir = tempfile::tempdir().unwrap();
    let s1 = write_script(dir.path(), "mock_a.py", MOCK_SERVER);
    let s2 = write_script(dir.path(), "mock_b.py", MOCK_SERVER);
    let reg = ToolRegistry::new();
    let statuses = register_from_configs(
        &reg,
        &[
            McpServerConfig::stdio(
                "a",
                "python3",
                vec![s1.display().to_string()],
                Tier::Three,
                5000,
            ),
            McpServerConfig::stdio(
                "b",
                "python3",
                vec![s2.display().to_string()],
                Tier::Three,
                5000,
            ),
        ],
    )
    .await;
    assert_eq!(statuses.iter().filter(|s| s.is_ok()).count(), 2);
    assert!(reg.get("mcp.a.echo").is_some());
    assert!(reg.get("mcp.b.echo").is_some());
    // both tools point at their own client — call each and ensure they respond
    let ctx = ToolCtx::new();
    let out_a = Arc::new(reg.get("mcp.a.echo").unwrap())
        .call(
            ToolArgs {
                positional: vec![],
                named: vec![("text".into(), Value::Str("A".into()))],
            },
            &ctx,
        )
        .await
        .unwrap();
    let out_b = Arc::new(reg.get("mcp.b.echo").unwrap())
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
