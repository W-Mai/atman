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
