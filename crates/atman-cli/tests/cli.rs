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
fn cost_aggregates_llm_calls_from_session() {
    let data = tempfile::tempdir().unwrap();
    let flow_dir = tempfile::tempdir().unwrap();
    let flow_path = flow_dir.path().join("c.at");
    std::fs::write(
        &flow_path,
        r#"flow c(q: string) -> string {
    return llm { model: "mock", prompt: q }
}
"#,
    )
    .unwrap();

    Command::new(atman_binary())
        .env("ATMAN_DATA_DIR", data.path())
        .arg("run")
        .arg("--mock")
        .arg(&flow_path)
        .arg("q=hi")
        .output()
        .unwrap();

    let out = Command::new(atman_binary())
        .env("ATMAN_DATA_DIR", data.path())
        .arg("cost")
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("total llm_calls: 1"));
    assert!(stdout.contains("mock"));
}

#[test]
fn repl_runs_boot_flow_at_startup() {
    let data = tempfile::tempdir().unwrap();
    let cfg = tempfile::tempdir().unwrap();
    std::fs::write(
        cfg.path().join("on_session_start.at"),
        r#"flow boot() -> string {
    return "boot ok"
}
"#,
    )
    .unwrap();

    let mut child = std::process::Command::new(atman_binary())
        .env("ATMAN_DATA_DIR", data.path())
        .env("ATMAN_CONFIG_DIR", cfg.path())
        .env("ATMAN_REPL_NON_INTERACTIVE", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child.stdin.as_mut().unwrap().write_all(b":exit\n").unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("boot ok"), "stdout: {stdout}");
}

#[test]
fn repl_slash_command_runs_flow_from_config_dir() {
    let data = tempfile::tempdir().unwrap();
    let cfg = tempfile::tempdir().unwrap();
    let cmd_dir = cfg.path().join("commands");
    std::fs::create_dir_all(&cmd_dir).unwrap();
    std::fs::write(
        cmd_dir.join("greet.at"),
        r#"flow greet(who: string) -> string {
    return "hi " + who
}
"#,
    )
    .unwrap();

    let mut child = std::process::Command::new(atman_binary())
        .env("ATMAN_DATA_DIR", data.path())
        .env("ATMAN_CONFIG_DIR", cfg.path())
        .env("ATMAN_REPL_NON_INTERACTIVE", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"/greet atman\n:exit\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("hi atman"), "stdout: {stdout}");
}

#[test]
fn repl_slash_command_unknown_reports_error() {
    let data = tempfile::tempdir().unwrap();
    let cfg = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(atman_binary())
        .env("ATMAN_DATA_DIR", data.path())
        .env("ATMAN_CONFIG_DIR", cfg.path())
        .env("ATMAN_REPL_NON_INTERACTIVE", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"/missing\n:exit\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no such command"), "stderr: {stderr}");
}

#[test]
fn repl_help_and_exit_via_stdin() {
    let data = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(atman_binary())
        .env("ATMAN_DATA_DIR", data.path())
        .env("ATMAN_REPL_NON_INTERACTIVE", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    let stdin = child.stdin.as_mut().unwrap();
    stdin.write_all(b":help\n:session\n:exit\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(":help"));
    assert!(stdout.contains("session_id:"));
}

#[test]
fn doctor_reports_paths_and_provider_marks() {
    let data = tempfile::tempdir().unwrap();
    let cfg = tempfile::tempdir().unwrap();
    let out = Command::new(atman_binary())
        .env("ATMAN_DATA_DIR", data.path())
        .env("ATMAN_CONFIG_DIR", cfg.path())
        .arg("doctor")
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("atman v"));
    assert!(stdout.contains("data_dir:"));
    assert!(stdout.contains("providers:"));
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

#[test]
fn repl_routes_bare_input_to_command_via_routes_toml() {
    let data = tempfile::tempdir().unwrap();
    let cfg = tempfile::tempdir().unwrap();
    let cmd_dir = cfg.path().join("commands");
    std::fs::create_dir_all(&cmd_dir).unwrap();
    std::fs::write(
        cmd_dir.join("echo.at"),
        r#"flow echo(msg: string) -> string {
    return "echoed: " + msg
}
"#,
    )
    .unwrap();
    std::fs::write(
        cfg.path().join("routes.toml"),
        "# route bang-prefixed lines to /echo\n\"!\" -> echo\n",
    )
    .unwrap();

    let mut child = std::process::Command::new(atman_binary())
        .env("ATMAN_DATA_DIR", data.path())
        .env("ATMAN_CONFIG_DIR", cfg.path())
        .env("ATMAN_REPL_NON_INTERACTIVE", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"!hello\n:exit\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("echoed: hello"), "stdout: {stdout}");
}

#[test]
fn repl_attach_command_accepts_existing_file() {
    let data = tempfile::tempdir().unwrap();
    let cfg = tempfile::tempdir().unwrap();
    let img = data.path().join("pic.png");
    std::fs::write(&img, b"fake").unwrap();

    let mut child = std::process::Command::new(atman_binary())
        .env("ATMAN_DATA_DIR", data.path())
        .env("ATMAN_CONFIG_DIR", cfg.path())
        .env("ATMAN_REPL_NON_INTERACTIVE", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    let cmd = format!(":attach {}\n:exit\n", img.display());
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(cmd.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("attached"), "stdout: {stdout}");
    assert!(stdout.contains("pending count: 1"), "stdout: {stdout}");
}

#[test]
fn repl_attach_command_rejects_missing_file() {
    let data = tempfile::tempdir().unwrap();
    let cfg = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(atman_binary())
        .env("ATMAN_DATA_DIR", data.path())
        .env("ATMAN_CONFIG_DIR", cfg.path())
        .env("ATMAN_REPL_NON_INTERACTIVE", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b":attach /nope/nope.png\n:exit\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("file not found"), "stderr: {stderr}");
}

#[test]
fn repl_unrouted_input_hints_at_routes_toml() {
    let data = tempfile::tempdir().unwrap();
    let cfg = tempfile::tempdir().unwrap();

    let mut child = std::process::Command::new(atman_binary())
        .env("ATMAN_DATA_DIR", data.path())
        .env("ATMAN_CONFIG_DIR", cfg.path())
        .env("ATMAN_REPL_NON_INTERACTIVE", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"bare input\n:exit\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("routes.toml"), "stdout: {stdout}");
}
