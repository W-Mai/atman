use std::path::Path;
use std::process::Command;

fn atman_bin() -> &'static str {
    env!("CARGO_BIN_EXE_atman")
}

fn run(cwd: &Path, args: &[&str]) -> (String, String, i32) {
    let out = Command::new(atman_bin())
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("spawn atman");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn flow_lint_clean_file_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("clean.at");
    std::fs::write(&path, "flow t(x: int) -> int {\n    return x\n}\n").unwrap();
    let (out, err, code) = run(tmp.path(), &["flow", "lint", path.to_str().unwrap()]);
    assert_eq!(code, 0, "exit: stderr={err}");
    assert!(out.contains("clean"), "want clean note: {out}");
}

#[test]
fn flow_lint_reports_many_positional_and_exits_nonzero() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("bad.at");
    std::fs::write(
        &path,
        "flow t() -> string {\n    return stdlib.compose_email_preview(\"s\", \"b\", [\"a\"], \"extra\")\n}\n",
    )
    .unwrap();
    let (out, err, code) = run(tmp.path(), &["flow", "lint", path.to_str().unwrap()]);
    assert_ne!(code, 0, "want non-zero exit");
    assert!(
        out.contains("many-positional"),
        "want rule slug in stdout: {out}"
    );
    assert!(err.contains("1 hit"), "want summary bail: stderr={err}");
}

#[test]
fn flow_lint_reports_unused_flow_param() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("unused.at");
    std::fs::write(
        &path,
        "flow t(used: int, unused: int) -> int {\n    return used\n}\n",
    )
    .unwrap();
    let (out, _, code) = run(tmp.path(), &["flow", "lint", path.to_str().unwrap()]);
    assert_ne!(code, 0);
    assert!(
        out.contains("unused-flow-param") && out.contains("`unused`"),
        "want unused param hit: {out}"
    );
}
