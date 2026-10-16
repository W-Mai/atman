use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use atman_daemon::{
    DaemonState,
    http::{HttpState, router},
};

fn atman_bin() -> &'static str {
    env!("CARGO_BIN_EXE_atman")
}

fn seed_events(root: &Path, sid: &str, lines: &[&str]) {
    let dir = root.join("sessions").join(sid);
    std::fs::create_dir_all(&dir).unwrap();
    let mut body = String::new();
    for l in lines {
        body.push_str(l);
        body.push('\n');
    }
    std::fs::write(dir.join("events.jsonl"), body).unwrap();
}

async fn spawn_server(
    root: &Path,
    token: &str,
    session_id: &str,
) -> (u16, tokio_util::sync::CancellationToken) {
    let daemon = Arc::new(DaemonState::new(root.to_path_buf()));
    let session = Arc::new(atman_runtime::Session::open_ephemeral());
    let session_id = atman_proto::SessionId(uuid::Uuid::parse_str(session_id).unwrap());
    daemon.register_broker(
        session_id.clone(),
        session,
        atman_daemon::state::LOCAL_OPERATOR,
    );
    daemon
        .register_live(
            session_id,
            atman_daemon::LiveSession {
                run_id: atman_proto::FlowRunId(uuid::Uuid::now_v7()),
                flow_name: "logs-stream-test".into(),
                cancel: tokio_util::sync::CancellationToken::new(),
                started_at: chrono::Utc::now(),
            },
        )
        .unwrap();
    serve(daemon, token).await
}

async fn serve(
    daemon: Arc<DaemonState>,
    token: &str,
) -> (u16, tokio_util::sync::CancellationToken) {
    let state = Arc::new(HttpState {
        daemon,
        auth_token: token.to_string(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let sh_clone = shutdown.clone();
    tokio::spawn(async move {
        axum::serve(listener, router(state))
            .with_graceful_shutdown(async move { sh_clone.cancelled().await })
            .await
            .ok();
    });
    tokio::time::sleep(Duration::from_millis(80)).await;
    (port, shutdown)
}

#[tokio::test(flavor = "multi_thread")]
async fn instance_logs_resume_atomic_batches_and_report_bad_cursors() {
    use atman_proto::{JsonRpcRequest, flows::FlowCursor, methods};
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, BufReader};
    use uuid::Uuid;

    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let config = tmp.path().join("config");
    std::fs::create_dir(&config).unwrap();
    let daemon = Arc::new(DaemonState::new(data.clone()));
    daemon.set_launcher(Arc::new(
        atman_daemon::run::RunLauncher::new(tmp.path().to_path_buf(), Some(config), None).unwrap(),
    ));
    let sid = Uuid::now_v7();
    let iid = Uuid::now_v7();
    let flow = json!({"session_id": sid, "instance_id": iid});
    let call = |method, params| {
        let daemon = daemon.clone();
        async move {
            let result =
                atman_daemon::dispatch(daemon, JsonRpcRequest::new(1, method, params)).await;
            assert!(result.error.is_none(), "{result:?}");
            result.result.unwrap()
        }
    };
    let created = call(
        methods::SESSION_CREATE,
        json!({"session_id": sid, "title": "CLI events", "project_root": tmp.path()}),
    )
    .await;
    let path = tmp.path().join("main.at");
    std::fs::write(
        &path,
        "flow main() -> string { return final.answer(message: \"cli final\") }",
    )
    .unwrap();
    call(methods::FLOW_LAUNCH, json!({"flow": flow, "request_id": Uuid::now_v7(), "expected_catalog_sequence": created["catalog"]["sequence"], "path": path, "entry": "main"})).await;
    let status = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let status = call(methods::FLOW_GET, json!({"flow": flow})).await;
            if !status["last_completion"].is_null() {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let first = call(methods::FLOW_EVENTS, json!({"flow": flow, "limit": 1})).await;
    let cursor: FlowCursor = serde_json::from_value(first["next"].clone()).unwrap();
    let expected = call(methods::FLOW_EVENTS, json!({"flow": flow, "after": cursor})).await;
    assert_eq!(expected["has_more"], false);
    let (port, shutdown) = serve(daemon.clone(), "instance-token").await;
    let daemon_cfg = tmp.path().join("daemon.toml");
    write_daemon_config(&daemon_cfg, "instance-token");
    let command = || {
        let mut command = tokio::process::Command::new(atman_bin());
        command
            .args([
                "logs",
                "stream",
                &sid.to_string(),
                "--instance",
                &iid.to_string(),
                "--port",
                &port.to_string(),
            ])
            .env("ATMAN_DATA_DIR", &data)
            .env("ATMAN_DAEMON_CONFIG_PATH", &daemon_cfg)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    };
    let mut child = command()
        .args(["--after", &cursor.to_string()])
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let observed = tokio::time::timeout(Duration::from_secs(15), async {
        let mut batches = Vec::new();
        while let Some(line) = lines.next_line().await.unwrap() {
            let batch: Value = serde_json::from_str(&line)
                .unwrap_or_else(|error| panic!("invalid batch: {line:?}: {error}"));
            let finished = batch["receipt"] == status["cursor"];
            batches.push(batch);
            if finished {
                return batches;
            }
        }
        panic!("logs stream ended before the last commit");
    })
    .await
    .unwrap();
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    assert_eq!(json!(observed), expected["batches"]);
    assert_eq!(
        call(methods::FLOW_GET, json!({"flow": flow})).await["execution"],
        status["execution"]
    );

    let bad = FlowCursor {
        journal_id: Uuid::new_v4(),
        ..cursor
    };
    let rejected = tokio::time::timeout(
        Duration::from_secs(10),
        command().args(["--after", &bad.to_string()]).output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("journal cursor"));
    assert!(rejected.stdout.is_empty());
    shutdown.cancel();
}

fn write_daemon_config(cfg_path: &Path, token: &str) {
    if let Some(parent) = cfg_path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(cfg_path, format!("auth_token = \"{token}\"\n")).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn logs_stream_prints_existing_events_from_running_daemon() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let sid = "0198feed-0000-7000-8000-000000000001";
    seed_events(
        &data_dir,
        sid,
        &[
            r#"{"type":"flow_start","seq":1,"run_id":{"raw":"r1"},"flow_name":"hello","ts":"2026-07-05T12:00:00Z"}"#,
            r#"{"type":"llm_call","seq":2,"model":"gpt-4o-mini","provider":"mock","usage":{"input":10,"cached_input":0,"output":5,"cache_write":0},"wallclock_ms":42,"status":"ok","ts":"2026-07-05T12:00:01Z"}"#,
            r#"{"type":"flow_end","seq":3,"run_id":{"raw":"r1"},"flow_name":"hello","status":"ok","ts":"2026-07-05T12:00:02Z"}"#,
        ],
    );

    let token = "logs-stream-test-token";
    let (port, shutdown) = spawn_server(&data_dir, token, sid).await;
    let daemon_cfg = tmp.path().join("daemon.toml");
    write_daemon_config(&daemon_cfg, token);

    let mut child = Command::new(atman_bin())
        .args(["logs", "stream", sid, "--port", &port.to_string()])
        .env("ATMAN_DATA_DIR", data_dir.to_str().unwrap())
        .env("ATMAN_DAEMON_CONFIG_PATH", daemon_cfg.to_str().unwrap())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn atman logs stream");

    let mut stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    use std::io::Read;
    let mut buf = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(4);
    let mut chunk = [0u8; 1024];
    while std::time::Instant::now() < deadline {
        match stdout.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf);
                if text.contains("flow_end") {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break,
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    shutdown.cancel();

    let text = String::from_utf8_lossy(&buf);
    let mut err_body = String::new();
    let _ = std::io::BufReader::new(stderr).read_to_string(&mut err_body);
    assert!(
        text.contains("flow_start"),
        "want flow_start in stdout, got stdout=\n{text}\nstderr=\n{err_body}"
    );
    assert!(
        text.contains("llm_call") && text.contains("gpt-4o-mini"),
        "want llm_call event, got: {text}"
    );
    assert!(text.contains("flow_end"), "want flow_end, got: {text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn logs_stream_since_seq_skips_older_events() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let sid = "0198feed-0000-7000-8000-000000000002";
    let mut lines_owned: Vec<String> = Vec::new();
    for i in 1..=5 {
        lines_owned.push(format!(r#"{{"type":"marker","seq":{i},"tag":"evt{i}"}}"#));
    }
    let lines: Vec<&str> = lines_owned.iter().map(String::as_str).collect();
    seed_events(&data_dir, sid, &lines);

    let token = "logs-stream-since-token";
    let (port, shutdown) = spawn_server(&data_dir, token, sid).await;
    let daemon_cfg = tmp.path().join("daemon.toml");
    write_daemon_config(&daemon_cfg, token);

    let mut child = Command::new(atman_bin())
        .args([
            "logs",
            "stream",
            sid,
            "--port",
            &port.to_string(),
            "--since-seq",
            "3",
        ])
        .env("ATMAN_DATA_DIR", data_dir.to_str().unwrap())
        .env("ATMAN_DAEMON_CONFIG_PATH", daemon_cfg.to_str().unwrap())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn atman logs stream");

    let mut stdout = child.stdout.take().unwrap();
    use std::io::Read;
    let mut buf = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut chunk = [0u8; 1024];
    while std::time::Instant::now() < deadline {
        match stdout.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf);
                if text.contains("evt5") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    shutdown.cancel();

    let text = String::from_utf8_lossy(&buf);
    assert!(
        !text.contains("evt1") && !text.contains("evt3"),
        "since_seq=3 should skip evt1..evt3, got: {text}"
    );
    assert!(text.contains("evt4"), "want evt4, got: {text}");
    assert!(text.contains("evt5"), "want evt5, got: {text}");
}
