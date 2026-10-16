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
import json, os, sys, time
from pathlib import Path

with open("pids", "a") as f:
    f.write(str(os.getpid()) + "\n")
sys.stderr.write("x" * (512 * 1024))
sys.stderr.flush()
sample_call = None
for line in sys.stdin:
    req = json.loads(line)
    if "method" not in req:
        text = str(req.get("result", req.get("error")))
        print(json.dumps({"jsonrpc": "2.0", "id": sample_call, "result": {"content": [{"type": "text", "text": text}]}}), flush=True)
        continue
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
            if Path("refreshing").exists():
                Path("refresh-waiting").touch()
                while not Path("release-refresh").exists():
                    time.sleep(0.01)
            result = {"tools": [{"name": os.environ.get("TOOL", "probe"), "inputSchema": {"type": "object"}}]}
    elif method == "tools/call":
        action = req["params"]["arguments"].get("action")
        if action == "reconnect" and not Path("reconnected").exists():
            Path("reconnected").touch()
            sys.exit(0)
        if action == "wait":
            Path("ready").touch()
            continue
        if action == "sample":
            sample_call = req["id"]
            print(json.dumps({"jsonrpc": "2.0", "id": "sample", "method": "sampling/createMessage", "params": {"messages": [], "model": "mock"}}), flush=True)
            continue
        if action == "refresh":
            Path("refreshing").touch()
            print(json.dumps({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}), flush=True)
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

async fn service_runtime(
    config: &atman_runtime::config_hub::ConfigHub,
    project: &std::path::Path,
) -> atman_runtime::AtmanRuntime {
    atman_runtime::AtmanRuntime::build(atman_runtime::AtmanRuntimeOptions {
        events: atman_runtime::event::EventSink::new(),
        mock: true,
        config: config.clone(),
        project: Some(
            atman_runtime::storage::ProjectContext::resolve(config, project, project).unwrap(),
        ),
        home_dir: None,
        workspace_generation: "mcp-service".into(),
    })
    .await
    .unwrap()
}

async fn service_ready(executor: &atman_runtime::Executor) {
    use atman_runtime::tool::Tool;
    let args = ToolArgs {
        named: vec![("server".into(), Value::Str("owned".into()))],
        ..Default::default()
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        atman_runtime::tools::mcp::McpAwait.call(args, &executor.tool_ctx),
    )
    .await
    .unwrap()
    .unwrap();
}

#[tokio::test]
async fn runtime_mcp_owners_isolate_connections_and_share_releasable_session_projections() {
    use atman_runtime::{Session, ValueJson, config_hub::ConfigHub};
    let left = tempfile::tempdir().unwrap();
    let right = tempfile::tempdir().unwrap();
    let left_hub = ConfigHub::from_config_dir(left.path());
    let right_hub = ConfigHub::from_config_dir(right.path());
    left_hub.save_mcp(&[owned_config(left.path())]).unwrap();
    right_hub.save_mcp(&[owned_config(right.path())]).unwrap();
    let left_runtime = service_runtime(&left_hub, left.path()).await;
    let right_runtime = service_runtime(&right_hub, right.path()).await;
    let session = Arc::new(Session::open_ephemeral());
    session.bind_executor(&left_runtime.executor).unwrap();
    let mut projection = session.subscribe_context();
    service_ready(&left_runtime.executor).await;
    service_ready(&right_runtime.executor).await;
    assert!(projection.borrow().mcp_servers[0].is_ok());
    let late = Session::open_ephemeral();
    late.bind_executor(&left_runtime.executor).unwrap();
    assert!(late.subscribe_context().borrow().mcp_servers[0].is_ok());
    assert!(session.bind_executor(&right_runtime.executor).is_err());
    let status = left_runtime.executor.tools.get("mcp.status").unwrap();
    assert_eq!(
        status
            .call(ToolArgs::default(), &left_runtime.executor.tool_ctx)
            .await
            .unwrap()
            .to_json()
            .unwrap()[0]["state"],
        "connected"
    );
    let left_tool = left_runtime.executor.tools.get("mcp.owned.probe").unwrap();
    let right_tool = right_runtime.executor.tools.get("mcp.owned.probe").unwrap();
    for (tool, runtime, path) in [
        (&left_tool, &left_runtime, left.path()),
        (&right_tool, &right_runtime, right.path()),
    ] {
        let output = tool
            .call(ToolArgs::default(), &runtime.executor.tool_ctx)
            .await
            .unwrap()
            .to_json()
            .unwrap();
        assert_eq!(
            std::fs::canonicalize(output["text"].as_str().unwrap()).unwrap(),
            std::fs::canonicalize(path).unwrap()
        );
    }
    let weak = Arc::downgrade(&session);
    drop(session);
    assert!(weak.upgrade().is_none());
    let peer = left_runtime.executor.clone();
    drop(left_runtime);
    assert!(
        left_tool
            .call(ToolArgs::default(), &peer.tool_ctx)
            .await
            .is_ok()
    );
    projection.borrow_and_update();
    left_hub.save_mcp(&[]).unwrap();
    peer.tool_ctx.mcp_service().unwrap().reload().unwrap();
    assert!(peer.tools.get("mcp.owned.probe").is_none());
    assert!(peer.tools.get("mcp.status").is_some());
    assert!(late.subscribe_context().borrow().mcp_servers.is_empty());
    assert!(
        right_tool
            .call(ToolArgs::default(), &right_runtime.executor.tool_ctx)
            .await
            .is_ok()
    );
    #[cfg(unix)]
    assert_processes_stopped(left.path()).await;
    assert!(
        left_tool
            .call(ToolArgs::default(), &peer.tool_ctx)
            .await
            .is_err()
    );
    let owner = Arc::downgrade(right_runtime.executor.tool_ctx.mcp_service().unwrap());
    drop(right_runtime);
    assert!(owner.upgrade().is_none());
    #[cfg(unix)]
    assert_processes_stopped(right.path()).await;
    assert!(
        right_tool
            .call(ToolArgs::default(), &ToolCtx::new())
            .await
            .is_err()
    );
    peer.tool_ctx.mcp_service().unwrap().shutdown().await;
    assert!(peer.tool_ctx.mcp_service().unwrap().reload().is_err());
    let replacement = service_runtime(&left_hub, left.path()).await;
    late.bind_executor(&replacement.executor).unwrap();
    assert!(late.subscribe_context().borrow().mcp_servers.is_empty());
}

#[tokio::test]
async fn runtime_mcp_sampling_uses_its_own_provider_registry() {
    use atman_runtime::{ValueJson, config_hub::ConfigHub, providers::mock::MockProvider};
    let left = tempfile::tempdir().unwrap();
    let right = tempfile::tempdir().unwrap();
    let mut runtimes = Vec::new();
    for (label, dir) in [
        ("left-response", left.path()),
        ("right-response", right.path()),
    ] {
        let hub = ConfigHub::from_config_dir(dir);
        hub.save_mcp(&[owned_config(dir)]).unwrap();
        let runtime = service_runtime(&hub, dir).await;
        runtime.executor.providers.register(Arc::new(
            MockProvider::new("mock").with_fallback(Value::Str(label.into())),
        ));
        service_ready(&runtime.executor).await;
        runtimes.push((label, runtime));
    }
    for (label, runtime) in &runtimes {
        let result = runtime
            .executor
            .tools
            .get("mcp.owned.probe")
            .unwrap()
            .call(
                ToolArgs {
                    named: vec![("action".into(), Value::Str("sample".into()))],
                    ..Default::default()
                },
                &runtime.executor.tool_ctx,
            )
            .await
            .unwrap()
            .to_json()
            .unwrap();
        assert!(result["text"].as_str().unwrap().contains(label), "{result}");
        runtime
            .executor
            .tool_ctx
            .mcp_service()
            .unwrap()
            .shutdown()
            .await;
    }
}

#[tokio::test]
async fn runtime_mcp_reload_cancels_connecting_and_refreshing_generations() {
    use atman_runtime::config_hub::ConfigHub;
    use atman_runtime::tool::Tool;
    for mode in ["pending", "connected"] {
        let dir = tempfile::tempdir().unwrap();
        let hub = ConfigHub::from_config_dir(dir.path());
        let mut cfg = owned_config(dir.path());
        cfg.env.push(("MODE".into(), mode.into()));
        hub.save_mcp(std::slice::from_ref(&cfg)).unwrap();
        let runtime = service_runtime(&hub, dir.path()).await;
        let service = runtime.executor.tool_ctx.mcp_service().unwrap();
        let mut statuses = service.subscribe();
        let mut waiting = Box::pin(atman_runtime::tools::mcp::McpAwait.call(
            ToolArgs {
                named: vec![("server".into(), Value::Str("owned".into()))],
                ..Default::default()
            },
            &runtime.executor.tool_ctx,
        ));
        if mode == "pending" {
            wait_for_file(&dir.path().join("ready")).await;
            assert!(futures::poll!(waiting.as_mut()).is_pending());
        } else {
            service_ready(&runtime.executor).await;
            runtime
                .executor
                .tools
                .get("mcp.owned.probe")
                .unwrap()
                .call(
                    ToolArgs {
                        named: vec![("action".into(), Value::Str("refresh".into()))],
                        ..Default::default()
                    },
                    &runtime.executor.tool_ctx,
                )
                .await
                .unwrap();
            wait_for_file(&dir.path().join("refresh-waiting")).await;
        }
        cfg.disabled = true;
        hub.save_mcp(if mode == "pending" {
            &[]
        } else {
            std::slice::from_ref(&cfg)
        })
        .unwrap();
        service.reload().unwrap();
        let expected = statuses.borrow_and_update().clone();
        if mode == "pending" {
            assert!(expected.is_empty());
            let error = tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
                .await
                .unwrap()
                .unwrap_err();
            assert!(
                error.to_string().contains("no longer configured"),
                "{error}"
            );
        } else {
            assert_eq!(
                expected[0].state,
                atman_runtime::mcp::McpServerState::Disabled
            );
        }
        assert!(runtime.executor.tools.get("mcp.owned.probe").is_none());
        #[cfg(unix)]
        assert_processes_stopped(dir.path()).await;
        std::fs::write(dir.path().join("release-refresh"), "").unwrap();
        assert_eq!(*statuses.borrow(), expected);
        assert!(runtime.executor.tools.get("mcp.owned.probe").is_none());
        tokio::time::timeout(std::time::Duration::from_secs(2), service.shutdown())
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn runtime_mcp_can_start_after_empty_boot_and_server_failure_is_independent() {
    use atman_runtime::{config_hub::ConfigHub, tools::mcp::await_requested_tools};
    let dir = tempfile::tempdir().unwrap();
    let hub = ConfigHub::from_config_dir(dir.path());
    let runtime = service_runtime(&hub, dir.path()).await;
    let service = runtime.executor.tool_ctx.mcp_service().unwrap();
    assert!(service.subscribe().borrow().is_empty());
    let cfg = owned_config(dir.path());
    let mut invalid = cfg.clone();
    invalid.name = "unavailable".into();
    invalid.command = dir
        .path()
        .join("missing-executable")
        .to_string_lossy()
        .into();
    hub.save_mcp(&[cfg, invalid]).unwrap();
    service.reload().unwrap();
    service_ready(&runtime.executor).await;
    await_requested_tools(
        &ToolArgs {
            named: vec![(
                "tools".into(),
                Value::List(vec![Value::Str("mcp.*".into())]),
            )],
            ..Default::default()
        },
        &runtime.executor.tool_ctx,
    )
    .await
    .unwrap();
    assert!(
        await_requested_tools(
            &ToolArgs {
                named: vec![(
                    "tools".into(),
                    Value::List(vec![Value::Str("mcp.unavailable.*".into())])
                )],
                ..Default::default()
            },
            &runtime.executor.tool_ctx
        )
        .await
        .is_err()
    );
    let mut statuses = service.subscribe();
    assert!(statuses.borrow().iter().any(|status| matches!(
        status.state,
        atman_runtime::mcp::McpServerState::Error { .. }
    )));
    service.shutdown().await;
    assert!(statuses.borrow_and_update().iter().all(|status| matches!(
        status.state,
        atman_runtime::mcp::McpServerState::Disconnected { .. }
    )));
    #[cfg(unix)]
    assert_processes_stopped(dir.path()).await;
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
