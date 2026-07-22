use std::path::Path;
use std::process::Command;

fn atman_bin() -> &'static str {
    env!("CARGO_BIN_EXE_atman")
}

fn run_init(cfg: &Path) -> (String, String, i32) {
    let out = Command::new(atman_bin())
        .arg("init")
        .env("ATMAN_CONFIG_DIR", cfg.to_str().unwrap())
        .output()
        .expect("spawn atman init");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn init_writes_config_tree_and_prints_next_steps() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = tmp.path().join("atman");
    let (out, err, code) = run_init(&cfg);
    assert_eq!(code, 0, "stderr={err}");
    assert!(out.contains("wrote 5 template"), "want write count: {out}");
    assert!(out.contains("next steps:"), "want next-steps block: {out}");
    for entry in [
        "config.toml",
        "routes.at",
        "on_session_start.at",
        "commands/agent.at",
        "commands/hello.at",
    ] {
        assert!(
            cfg.join(entry).exists(),
            "missing {entry} under {}",
            cfg.display()
        );
    }
}

#[test]
fn init_is_idempotent_and_never_overwrites_user_edits() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = tmp.path().join("atman");
    run_init(&cfg);
    let hello = cfg.join("commands/hello.at");
    std::fs::write(&hello, "flow hello() -> string { return \"customised\" }\n").unwrap();

    let (out, err, code) = run_init(&cfg);
    assert_eq!(code, 0, "stderr={err}");
    assert!(
        out.contains("already fully populated"),
        "want idempotent hint: {out}"
    );
    let body = std::fs::read_to_string(&hello).unwrap();
    assert!(body.contains("customised"), "user edit lost: {body}");
}

#[test]
fn repl_goal_builtin_set_show_clear() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = tmp.path().join("atman");
    let data = tmp.path().join("data");
    run_init(&cfg);

    use std::io::Write;
    let mut cmd = Command::new(atman_bin());
    cmd.env("ATMAN_CONFIG_DIR", cfg.to_str().unwrap())
        .env("ATMAN_DATA_DIR", data.to_str().unwrap())
        .env("ATMAN_REPL_NON_INTERACTIVE", "1")
        .env("ATMAN_DISABLE_MIGRATION", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().expect("spawn repl");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b":goal\n:goal ship an agent\n:goal\n:goal clear\n:goal\n:exit\n")
        .expect("write");
    let out = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("no session goal set"),
        "want empty-goal hint: {stdout}"
    );
    assert!(
        stdout.contains("goal set: ship an agent"),
        "want set confirmation: {stdout}"
    );
    assert!(
        stdout.contains("goal: ship an agent"),
        "want current goal readback: {stdout}"
    );
    assert!(
        stdout.contains("goal cleared"),
        "want clear confirmation: {stdout}"
    );
}

#[test]
fn slash_command_resolver_accepts_multi_flow_agent_at() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = tmp.path().join("atman");
    let data = tmp.path().join("data");
    run_init(&cfg);

    let mut child = Command::new(atman_bin())
        .env("ATMAN_CONFIG_DIR", cfg.to_str().unwrap())
        .env("ATMAN_DATA_DIR", data.to_str().unwrap())
        .env("ATMAN_REPL_NON_INTERACTIVE", "1")
        .env("ATMAN_DISABLE_MIGRATION", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn repl");

    use std::io::Write;
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"/agent hi\n:exit\n")
        .expect("write stdin");
    // drop stdin so the REPL sees EOF if it's still reading
    drop(child.stdin.take());

    // The slash resolver check happens at parse time — we don't need the
    // agent flow to finish.  Give the process a generous window to parse
    // and print any stderr, then kill it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut out = None;
    while std::time::Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => {
                out = Some((
                    status,
                    child.stdout.take().and_then(|mut s| {
                        use std::io::Read;
                        let mut buf = Vec::new();
                        s.read_to_end(&mut buf).ok()?;
                        Some(buf)
                    }),
                    child.stderr.take().and_then(|mut s| {
                        use std::io::Read;
                        let mut buf = Vec::new();
                        s.read_to_end(&mut buf).ok()?;
                        Some(buf)
                    }),
                ));
                break;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
            Err(_) => break,
        }
    }
    let (_, _, stderr_bytes) = out.unwrap_or_else(|| {
        let _ = child.kill();
        let _ = child.wait();
        (std::process::ExitStatus::default(), None, None)
    });
    let stderr = stderr_bytes
        .as_deref()
        .map(String::from_utf8_lossy)
        .unwrap_or_default();
    assert!(
        !stderr.contains("must contain exactly one flow"),
        "slash resolver regression: multi-flow .at should not be rejected upfront. stderr=\n{stderr}"
    );
}

#[test]
fn slash_command_passes_multi_word_bare_text_intact_to_single_string_param() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = tmp.path().join("atman");
    let data = tmp.path().join("data");
    std::fs::create_dir_all(cfg.join("commands")).unwrap();
    std::fs::write(
        cfg.join("commands").join("echo.at"),
        "flow echo(msg: string) -> string { return msg }\n",
    )
    .unwrap();

    let out = Command::new(atman_bin())
        .env("ATMAN_CONFIG_DIR", cfg.to_str().unwrap())
        .env("ATMAN_DATA_DIR", data.to_str().unwrap())
        .env("ATMAN_REPL_NON_INTERACTIVE", "1")
        .env("ATMAN_DISABLE_MIGRATION", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(b"/echo OK short story about atman\n:exit\n")?;
            child.wait_with_output()
        })
        .expect("spawn repl");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("extra positional argument"),
        "spaces in bare text got wrongly split into positional args: stderr=\n{stderr}"
    );
    assert!(
        stdout.contains("OK short story about atman"),
        "want full multi-word string echoed back, got stdout=\n{stdout}"
    );
}

#[test]
fn init_produces_flows_that_actually_run() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = tmp.path().join("atman");
    run_init(&cfg);

    let data = tmp.path().join("data");
    let out = Command::new(atman_bin())
        .args([
            "run",
            cfg.join("commands/hello.at").to_str().unwrap(),
            "--ephemeral",
        ])
        .env("ATMAN_CONFIG_DIR", cfg.to_str().unwrap())
        .env("ATMAN_DATA_DIR", data.to_str().unwrap())
        .env("ATMAN_DISABLE_MIGRATION", "1")
        .output()
        .expect("spawn atman run hello.at");
    assert!(
        out.status.success(),
        "hello.at should run cleanly: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("hello from atman"),
        "want hello output, got: {stdout}"
    );
}
