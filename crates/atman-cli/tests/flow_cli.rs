use std::process::Command;

fn atman_bin() -> &'static str {
    env!("CARGO_BIN_EXE_atman")
}

fn run(cwd: &std::path::Path, args: &[&str]) -> (String, String, i32) {
    let out = Command::new(atman_bin())
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("spawn atman");
    let code = out.status.code().unwrap_or(-1);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        code,
    )
}

#[test]
fn flow_snapshot_versions_rollback_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let flow_path = root.join("greet.at");
    std::fs::write(&flow_path, "flow greet() { return 1 }\n").unwrap();

    let (out, err, code) = run(root, &["flow", "snapshot", "greet.at"]);
    assert_eq!(code, 0, "snapshot exit: stderr={err}");
    assert!(out.contains("snapshot ok"), "stdout: {out}");
    assert!(root.join(".atman/flow-registry.db").exists());

    std::fs::write(&flow_path, "flow greet() { return 2 }\n").unwrap();
    let (_, err, code) = run(root, &["flow", "snapshot", "greet.at"]);
    assert_eq!(code, 0, "second snapshot exit: stderr={err}");

    let (out, err, code) = run(root, &["flow", "versions", "greet"]);
    assert_eq!(code, 0, "versions exit: stderr={err}");
    let hash_rows = out.lines().filter(|l| l.contains("hash:")).count();
    assert_eq!(hash_rows, 2, "should list 2 revisions, got:\n{out}");

    let hash_first_short = {
        let src_v1 = "flow greet() { return 1 }\n";
        atman_runtime::flow_meta::FlowMeta::short_hash(src_v1)
    };
    let target = root.join("restored.at");
    let (out, err, code) = run(
        root,
        &[
            "flow",
            "rollback",
            "greet",
            &hash_first_short,
            "--to",
            target.to_str().unwrap(),
            "--yes",
        ],
    );
    assert_eq!(code, 0, "rollback exit: stderr={err}\nstdout={out}");
    let restored = std::fs::read_to_string(&target).unwrap();
    assert_eq!(restored, "flow greet() { return 1 }\n");
}

#[test]
fn flow_versions_on_unknown_flow_is_ok_and_prints_hint() {
    let tmp = tempfile::tempdir().unwrap();
    let (out, err, code) = run(tmp.path(), &["flow", "versions", "nobody"]);
    assert_eq!(code, 0, "unknown-name should exit 0: stderr={err}");
    assert!(out.contains("no revisions"), "stdout: {out}");
}

#[test]
fn flow_diff_between_two_revisions() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let flow_path = root.join("g.at");
    std::fs::write(&flow_path, "flow g() { return 1 }\n").unwrap();
    let (_, err, code) = run(root, &["flow", "snapshot", "g.at"]);
    assert_eq!(code, 0, "snap v1: {err}");
    std::fs::write(&flow_path, "flow g() { return 2 }\n").unwrap();
    let (_, err, code) = run(root, &["flow", "snapshot", "g.at"]);
    assert_eq!(code, 0, "snap v2: {err}");

    let hash_v1 = atman_runtime::flow_meta::FlowMeta::short_hash("flow g() { return 1 }\n");
    let hash_v2 = atman_runtime::flow_meta::FlowMeta::short_hash("flow g() { return 2 }\n");

    let (out, err, code) = run(root, &["flow", "diff", "g", &hash_v1, &hash_v2]);
    assert_eq!(code, 0, "diff exit: stderr={err}");
    assert!(
        out.contains("-flow g() { return 1 }"),
        "want deletion, got:\n{out}"
    );
    assert!(
        out.contains("+flow g() { return 2 }"),
        "want insertion, got:\n{out}"
    );
}
