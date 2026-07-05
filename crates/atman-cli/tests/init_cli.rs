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
fn slash_command_resolver_accepts_multi_flow_agent_at() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = tmp.path().join("atman");
    let data = tmp.path().join("data");
    run_init(&cfg);

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
                .write_all(b"/agent hi\n:exit\n")?;
            child.wait_with_output()
        })
        .expect("spawn repl");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("must contain exactly one flow"),
        "slash resolver regression: multi-flow .at should not be rejected upfront. stderr=\n{stderr}"
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
