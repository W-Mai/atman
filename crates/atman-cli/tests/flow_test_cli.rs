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
fn flow_test_writes_fresh_snapshot_when_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("hello.at");
    std::fs::write(&path, "flow greet() -> string { return \"hi atman\" }\n").unwrap();
    let (out, err, code) = run(tmp.path(), &["flow", "test", path.to_str().unwrap()]);
    assert_eq!(code, 0, "first run: stderr={err}");
    assert!(
        out.contains("wrote fresh snapshot"),
        "want fresh note: {out}"
    );
    let snap_path = tmp.path().join("hello.at.snap.json");
    assert!(snap_path.exists(), "snapshot should land next to flow");
    let body: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&snap_path).unwrap()).unwrap();
    assert_eq!(body["greet"], serde_json::json!("hi atman"));
}

#[test]
fn flow_test_matches_existing_snapshot_and_reports_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("hello.at");
    std::fs::write(&path, "flow greet() -> string { return \"hi atman\" }\n").unwrap();
    // first run creates
    let (_, _, code) = run(tmp.path(), &["flow", "test", path.to_str().unwrap()]);
    assert_eq!(code, 0);
    // second run should match
    let (out, err, code) = run(tmp.path(), &["flow", "test", path.to_str().unwrap()]);
    assert_eq!(code, 0, "second run: stderr={err}");
    assert!(out.contains("case(s) match"), "want match note: {out}");
}

#[test]
fn flow_test_drift_exits_nonzero_until_blessed() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("hello.at");
    std::fs::write(&path, "flow greet() -> string { return \"hi atman\" }\n").unwrap();
    let (_, _, code) = run(tmp.path(), &["flow", "test", path.to_str().unwrap()]);
    assert_eq!(code, 0);

    std::fs::write(&path, "flow greet() -> string { return \"hi world\" }\n").unwrap();
    let (out, err, code) = run(tmp.path(), &["flow", "test", path.to_str().unwrap()]);
    assert_ne!(code, 0, "drift should fail");
    assert!(
        out.contains("flow test drift: greet"),
        "want drift line: {out}"
    );
    assert!(
        err.contains("re-run with --bless"),
        "want bless hint: {err}"
    );

    let (out, err, code) = run(
        tmp.path(),
        &["flow", "test", path.to_str().unwrap(), "--bless"],
    );
    assert_eq!(code, 0, "bless should reset: stderr={err}");
    assert!(
        out.contains("refreshed snapshot"),
        "want refresh note: {out}"
    );

    let (_, _, code) = run(tmp.path(), &["flow", "test", path.to_str().unwrap()]);
    assert_eq!(code, 0, "post-bless run should match");
}

#[test]
fn flow_test_removed_flow_reported_as_drift() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("multi.at");
    std::fs::write(
        &path,
        "flow a() -> string { return \"one\" }\nflow b() -> string { return \"two\" }\n",
    )
    .unwrap();
    let (_, _, code) = run(tmp.path(), &["flow", "test", path.to_str().unwrap()]);
    assert_eq!(code, 0);

    std::fs::write(&path, "flow a() -> string { return \"one\" }\n").unwrap();
    let (out, err, code) = run(tmp.path(), &["flow", "test", path.to_str().unwrap()]);
    assert_ne!(code, 0);
    let _ = err;
    assert!(out.contains("b (removed)"), "want removed line: {out}");
}

#[test]
fn flow_test_skips_flows_with_params_and_notes_them() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("mix.at");
    std::fs::write(
        &path,
        "flow zero() -> int { return 1 }\nflow needs(x: int) -> int { return x }\n",
    )
    .unwrap();
    let (out, err, code) = run(tmp.path(), &["flow", "test", path.to_str().unwrap()]);
    assert_eq!(code, 0, "stderr={err}");
    assert!(
        out.contains("skipped flows requiring args: needs"),
        "want skipped note: {out}"
    );
    let snap: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(tmp.path().join("mix.at.snap.json")).unwrap(),
    )
    .unwrap();
    assert!(snap.get("zero").is_some());
    assert!(snap.get("needs").is_none(), "params flow shouldn't land");
}

#[test]
fn flow_test_no_zero_param_flows_prints_hint() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("all_params.at");
    std::fs::write(&path, "flow only(x: int) -> int { return x }\n").unwrap();
    let (out, err, code) = run(tmp.path(), &["flow", "test", path.to_str().unwrap()]);
    assert_eq!(code, 0, "stderr={err}");
    assert!(out.contains("nothing to run"), "want hint: {out}");
    assert!(
        !tmp.path().join("all_params.at.snap.json").exists(),
        "no snapshot should be written"
    );
}
