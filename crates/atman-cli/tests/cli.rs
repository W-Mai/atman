use std::process::Command;

fn atman_binary() -> String {
    env!("CARGO_BIN_EXE_atman").to_string()
}

#[test]
fn version_subcommand_prints_semver() {
    let out = Command::new(atman_binary())
        .arg("version")
        .output()
        .expect("run atman version");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("atman v"));
}

#[test]
fn run_executes_pure_flow_and_prints_return_value() {
    let dir = tempfile::tempdir().unwrap();
    let flow_path = dir.path().join("hello.at");
    std::fs::write(
        &flow_path,
        r#"flow greet(who: string) -> string {
    return "hi " + who
}
"#,
    )
    .unwrap();
    let out = Command::new(atman_binary())
        .arg("run")
        .arg(&flow_path)
        .arg("who=atman")
        .output()
        .expect("run atman run");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi atman");
}

#[test]
fn run_persists_events_and_logs_tail_reads_them() {
    let data = tempfile::tempdir().unwrap();
    let flow_dir = tempfile::tempdir().unwrap();
    let flow_path = flow_dir.path().join("hello.at");
    std::fs::write(
        &flow_path,
        r#"flow hello() -> string {
    return "hi persist"
}
"#,
    )
    .unwrap();

    let run = Command::new(atman_binary())
        .env("ATMAN_DATA_DIR", data.path())
        .arg("run")
        .arg(&flow_path)
        .output()
        .expect("run");
    assert!(
        run.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "hi persist");

    let sessions = data.path().join("sessions");
    let dirs: Vec<_> = std::fs::read_dir(&sessions)
        .expect("sessions dir")
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(dirs.len(), 1);
    let sid = dirs[0].file_name().into_string().unwrap();

    let tail = Command::new(atman_binary())
        .env("ATMAN_DATA_DIR", data.path())
        .args(["logs", "tail", &sid])
        .output()
        .expect("logs tail");
    assert!(
        tail.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&tail.stderr)
    );
    let stdout = String::from_utf8_lossy(&tail.stdout);
    let lines: Vec<_> = stdout.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("\"type\":\"flow_start\""));
    assert!(lines[1].contains("\"type\":\"flow_end\""));
}

#[test]
fn session_list_prints_rows_sorted_by_mtime() {
    let data = tempfile::tempdir().unwrap();
    let flow_dir = tempfile::tempdir().unwrap();
    let flow_path = flow_dir.path().join("s.at");
    std::fs::write(&flow_path, "flow s() -> Int { return 1 }\n").unwrap();

    for _ in 0..2 {
        Command::new(atman_binary())
            .env("ATMAN_DATA_DIR", data.path())
            .arg("run")
            .arg(&flow_path)
            .output()
            .unwrap();
    }

    let out = Command::new(atman_binary())
        .env("ATMAN_DATA_DIR", data.path())
        .args(["session", "list"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<_> = stdout.lines().collect();
    assert_eq!(lines.len(), 3);
    assert!(lines[0].contains("session_id"));
}

#[test]
fn session_show_prints_event_counts() {
    let data = tempfile::tempdir().unwrap();
    let flow_dir = tempfile::tempdir().unwrap();
    let flow_path = flow_dir.path().join("s.at");
    std::fs::write(&flow_path, "flow s() -> Int { return 1 }\n").unwrap();
    Command::new(atman_binary())
        .env("ATMAN_DATA_DIR", data.path())
        .arg("run")
        .arg(&flow_path)
        .output()
        .unwrap();

    let sid = std::fs::read_dir(data.path().join("sessions"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name()
        .into_string()
        .unwrap();

    let out = Command::new(atman_binary())
        .env("ATMAN_DATA_DIR", data.path())
        .args(["session", "show", &sid])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("flow_start: 1"));
    assert!(stdout.contains("flow_end:   1"));
}

#[test]
fn session_gc_removes_only_empty_sessions() {
    let data = tempfile::tempdir().unwrap();
    let sessions = data.path().join("sessions");
    let empty = sessions.join("019f0000-empty");
    let full = sessions.join("019f0000-full");
    std::fs::create_dir_all(&empty).unwrap();
    std::fs::create_dir_all(&full).unwrap();
    std::fs::write(empty.join("events.jsonl"), "").unwrap();
    std::fs::write(full.join("events.jsonl"), "{\"type\":\"flow_start\"}\n").unwrap();

    let out = Command::new(atman_binary())
        .env("ATMAN_DATA_DIR", data.path())
        .args(["session", "gc"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("removed 1"));
    assert!(!empty.exists());
    assert!(full.exists());
}

#[test]
fn logs_tail_without_session_id_uses_latest() {
    let data = tempfile::tempdir().unwrap();
    let flow_dir = tempfile::tempdir().unwrap();
    let flow_path = flow_dir.path().join("hi.at");
    std::fs::write(&flow_path, "flow hi() -> Int { return 1 }\n").unwrap();

    Command::new(atman_binary())
        .env("ATMAN_DATA_DIR", data.path())
        .arg("run")
        .arg(&flow_path)
        .output()
        .expect("run");

    let tail = Command::new(atman_binary())
        .env("ATMAN_DATA_DIR", data.path())
        .args(["logs", "tail"])
        .output()
        .expect("logs tail");
    assert!(tail.status.success());
    let stdout = String::from_utf8_lossy(&tail.stdout);
    assert!(stdout.contains("\"type\":\"flow_start\""));
}

#[test]
fn run_reports_error_and_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let flow_path = dir.path().join("bad.at");
    std::fs::write(
        &flow_path,
        r#"flow oops() -> Int {
    return missing
}
"#,
    )
    .unwrap();
    let out = Command::new(atman_binary())
        .arg("run")
        .arg(&flow_path)
        .output()
        .expect("run atman run");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("missing"));
}
