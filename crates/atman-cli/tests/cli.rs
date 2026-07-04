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
    let types: Vec<&str> = lines
        .iter()
        .filter_map(|l| l.split("\"type\":\"").nth(1))
        .filter_map(|s| s.split('"').next())
        .collect();
    assert!(types.contains(&"turn_start"), "types: {types:?}");
    assert!(types.contains(&"user_msg"), "types: {types:?}");
    assert!(types.contains(&"flow_start"), "types: {types:?}");
    assert!(types.contains(&"flow_end"), "types: {types:?}");
    assert!(types.contains(&"turn_end"), "types: {types:?}");
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
fn repl_fires_dsl_on_session_start_body_at_startup() {
    let data = tempfile::tempdir().unwrap();
    let cfg = tempfile::tempdir().unwrap();
    std::fs::write(
        cfg.path().join("lifecycle.at"),
        r#"on session.start {
    memory.todo.set(
        where: "boot",
        why: "dsl lifecycle",
        how: "on session.start",
        expected_result: "todo persisted"
    )
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
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let sessions_dir = data.path().join("sessions");
    let sid_dir = std::fs::read_dir(&sessions_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let todos_path = sid_dir.join("todos.jsonl");
    assert!(
        todos_path.exists(),
        "expected todos.jsonl at {}",
        todos_path.display()
    );
    let contents = std::fs::read_to_string(&todos_path).unwrap();
    assert!(
        contents.contains("dsl lifecycle"),
        "todos.jsonl: {contents}"
    );
}

#[test]
fn repl_route_dsl_dispatches_by_prefix() {
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
    std::fs::write(
        cfg.path().join("routes.at"),
        r#"route "hello" { flow: greet }
flow greet(who: string) -> string {
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
        .write_all(b"hello atman\n:exit\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("hi atman"),
        "expected 'hi atman' in stdout. stdout={stdout} stderr={stderr}"
    );
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
fn repl_attach_list_shows_pending_paths() {
    let data = tempfile::tempdir().unwrap();
    let cfg = tempfile::tempdir().unwrap();
    let img = data.path().join("a.png");
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
    let script = format!(":attach {}\n:attach list\n:exit\n", img.display());
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("a.png"), "stdout: {stdout}");
}

#[test]
fn monitor_serves_sessions_and_events_over_http() {
    let data = tempfile::tempdir().unwrap();
    let sessions = data.path().join("sessions");
    let sid = "019f2800-0000-7000-8000-000000000001";
    let dir = sessions.join(sid);
    std::fs::create_dir_all(&dir).unwrap();
    let ev = "{\"type\":\"flow_start\",\"run_id\":\"r1\",\"flow_name\":\"demo\",\"ts\":\"2026-07-03T00:00:00Z\"}\n{\"type\":\"flow_end\",\"run_id\":\"r1\",\"flow_name\":\"demo\",\"status\":{\"kind\":\"ok\"},\"ts\":\"2026-07-03T00:00:01Z\"}\n";
    std::fs::write(dir.join("events.jsonl"), ev).unwrap();

    let port = 65_000 + (std::process::id() % 500) as u16;
    let mut child = std::process::Command::new(atman_binary())
        .env("ATMAN_DATA_DIR", data.path())
        .args(["monitor", "--port", &port.to_string()])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");
    std::thread::sleep(std::time::Duration::from_millis(800));

    let base = format!("http://127.0.0.1:{port}");
    let sessions_resp = std::process::Command::new("curl")
        .args(["-s", &format!("{base}/api/sessions")])
        .output()
        .expect("curl sessions");
    let events_resp = std::process::Command::new("curl")
        .args(["-s", &format!("{base}/api/sessions/{sid}/events")])
        .output()
        .expect("curl events");
    let index_resp = std::process::Command::new("curl")
        .args(["-s", &format!("{base}/")])
        .output()
        .expect("curl index");
    let _ = child.kill();
    let _ = child.wait();

    let sessions_body = String::from_utf8_lossy(&sessions_resp.stdout);
    assert!(
        sessions_body.contains(sid),
        "sessions body: {sessions_body}"
    );
    assert!(
        sessions_body.contains("\"event_count\":2"),
        "sessions body: {sessions_body}"
    );

    let events_body = String::from_utf8_lossy(&events_resp.stdout);
    assert!(
        events_body.contains("flow_start"),
        "events body: {events_body}"
    );
    assert!(
        events_body.contains("flow_end"),
        "events body: {events_body}"
    );

    let index_body = String::from_utf8_lossy(&index_resp.stdout);
    assert!(
        index_body.contains("atman monitor"),
        "index body must contain title: {index_body}"
    );
}

#[test]
fn doctor_lists_migrated_rules_from_fake_home() {
    let data = tempfile::tempdir().unwrap();
    let cfg = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let home_agents = home.path().join(".config/opencode/AGENTS.md");
    std::fs::create_dir_all(home_agents.parent().unwrap()).unwrap();
    std::fs::write(&home_agents, "# global-rules\ncontent\n").unwrap();
    std::fs::write(
        project.path().join("AGENTS.md"),
        "# project-rules\ncontent\n",
    )
    .unwrap();

    let out = Command::new(atman_binary())
        .env("ATMAN_DATA_DIR", data.path())
        .env("ATMAN_CONFIG_DIR", cfg.path())
        .env("HOME", home.path())
        .current_dir(project.path())
        .arg("doctor")
        .output()
        .expect("doctor");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("migrated rules:"), "stdout: {stdout}");
    assert!(stdout.contains("project-rules"), "stdout: {stdout}");
    assert!(stdout.contains("global-rules"), "stdout: {stdout}");
}

#[test]
fn doctor_reports_no_mcp_servers_when_config_missing() {
    let data = tempfile::tempdir().unwrap();
    let cfg = tempfile::tempdir().unwrap();
    let out = Command::new(atman_binary())
        .env("ATMAN_DATA_DIR", data.path())
        .env("ATMAN_CONFIG_DIR", cfg.path())
        .arg("doctor")
        .output()
        .expect("doctor");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("mcp:"), "stdout: {stdout}");
    assert!(stdout.contains("none configured"), "stdout: {stdout}");
}

#[test]
fn doctor_reports_mcp_boot_failure_when_command_missing() {
    let data = tempfile::tempdir().unwrap();
    let cfg = tempfile::tempdir().unwrap();
    std::fs::write(
        cfg.path().join("config.toml"),
        "[[mcp]]\nname = \"broken\"\ncommand = \"/no/such/binary/here\"\ntimeout_ms = 200\n",
    )
    .unwrap();
    let out = Command::new(atman_binary())
        .env("ATMAN_DATA_DIR", data.path())
        .env("ATMAN_CONFIG_DIR", cfg.path())
        .arg("doctor")
        .output()
        .expect("doctor");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("broken"), "stdout: {stdout}");
    assert!(stdout.contains("[✗]"), "stdout: {stdout}");
}

#[test]
fn doctor_reports_preview_unavailable_when_server_absent() {
    let data = tempfile::tempdir().unwrap();
    let cfg = tempfile::tempdir().unwrap();
    std::fs::write(
        cfg.path().join("config.toml"),
        "[preview]\nbase_url = \"http://127.0.0.1:1\"\ntimeout_ms = 200\n",
    )
    .unwrap();
    let out = Command::new(atman_binary())
        .env("ATMAN_DATA_DIR", data.path())
        .env("ATMAN_CONFIG_DIR", cfg.path())
        .arg("doctor")
        .output()
        .expect("doctor");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("preview:"), "stdout: {stdout}");
    assert!(stdout.contains("http://127.0.0.1:1"), "stdout: {stdout}");
    assert!(
        stdout.contains("server not running") || stdout.contains("✗"),
        "stdout: {stdout}"
    );
}

#[test]
fn repl_at_path_inline_becomes_image_part_in_user_msg_event() {
    let data = tempfile::tempdir().unwrap();
    let cfg = tempfile::tempdir().unwrap();
    let cmd_dir = cfg.path().join("commands");
    std::fs::create_dir_all(&cmd_dir).unwrap();
    std::fs::write(
        cmd_dir.join("noop.at"),
        r#"flow noop() -> string {
    return "done"
}
"#,
    )
    .unwrap();
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
    let script = format!("/noop @{}\n:exit\n", img.display());
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "atman crashed: {:?}", out);

    let sessions = data.path().join("sessions");
    let mut found_image = false;
    for entry in std::fs::read_dir(&sessions).unwrap() {
        let entry = entry.unwrap();
        let events_path = entry.path().join("events.jsonl");
        if !events_path.exists() {
            continue;
        }
        let contents = std::fs::read_to_string(&events_path).unwrap();
        for line in contents.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            if v["type"] == "user_msg"
                && let Some(parts) = v["message"]["parts"].as_array()
                && parts.iter().any(|p| p["type"] == "image")
            {
                found_image = true;
            }
        }
    }
    assert!(found_image, "no user_msg event carried an image part");
}

#[test]
fn repl_attach_clear_empties_pending() {
    let data = tempfile::tempdir().unwrap();
    let cfg = tempfile::tempdir().unwrap();
    let img = data.path().join("b.png");
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
    let script = format!(
        ":attach {}\n:attach clear\n:attach list\n:exit\n",
        img.display()
    );
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("cleared"), "stdout: {stdout}");
    assert!(
        stdout.contains("no pending attachments"),
        "stdout: {stdout}"
    );
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
fn daemon_rotate_token_replaces_token_when_daemon_not_running() {
    let cfg_dir = tempfile::tempdir().unwrap();
    let pid_dir = tempfile::tempdir().unwrap();
    let cfg_path = cfg_dir.path().join("daemon.toml");
    let pid_path = pid_dir.path().join("atman-daemon.pid");

    std::fs::write(&cfg_path, "auth_token = \"old-token-000\"\n").unwrap();

    let out = Command::new(atman_binary())
        .env("ATMAN_DAEMON_CONFIG_PATH", &cfg_path)
        .env("ATMAN_DAEMON_PID_PATH", &pid_path)
        .args(["daemon", "rotate-token"])
        .output()
        .expect("run rotate-token");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let new_token = stdout.trim();
    assert_eq!(new_token.len(), 64, "stdout: {stdout}");
    assert!(new_token.chars().all(|c| c.is_ascii_hexdigit()));

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("restart daemon"), "stderr: {stderr}");

    let re_read = std::fs::read_to_string(&cfg_path).unwrap();
    assert!(re_read.contains(new_token));
    assert!(!re_read.contains("old-token-000"));
}

#[test]
fn daemon_rotate_token_refuses_when_daemon_running() {
    let cfg_dir = tempfile::tempdir().unwrap();
    let pid_dir = tempfile::tempdir().unwrap();
    let cfg_path = cfg_dir.path().join("daemon.toml");
    let pid_path = pid_dir.path().join("atman-daemon.pid");

    std::fs::write(&cfg_path, "auth_token = \"keep-me\"\n").unwrap();
    std::fs::write(&pid_path, std::process::id().to_string()).unwrap();

    let out = Command::new(atman_binary())
        .env("ATMAN_DAEMON_CONFIG_PATH", &cfg_path)
        .env("ATMAN_DAEMON_PID_PATH", &pid_path)
        .args(["daemon", "rotate-token"])
        .output()
        .expect("run rotate-token");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("running"), "stderr: {stderr}");
    assert!(stderr.contains("atman daemon stop"), "stderr: {stderr}");

    let re_read = std::fs::read_to_string(&cfg_path).unwrap();
    assert!(re_read.contains("keep-me"), "token should be unchanged");
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
