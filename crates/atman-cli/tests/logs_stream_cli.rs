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

async fn spawn_server(root: &Path, token: &str) -> (u16, tokio_util::sync::CancellationToken) {
    let daemon = Arc::new(DaemonState::new(root.to_path_buf()));
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
    let (port, shutdown) = spawn_server(&data_dir, token).await;
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
    let (port, shutdown) = spawn_server(&data_dir, token).await;
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
