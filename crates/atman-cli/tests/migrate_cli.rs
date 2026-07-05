use std::path::Path;
use std::process::Command;

fn atman_bin() -> &'static str {
    env!("CARGO_BIN_EXE_atman")
}

fn seed_fixture(root: &Path) {
    let sess = root.join("session/proj_hash");
    std::fs::create_dir_all(&sess).unwrap();
    std::fs::write(
        sess.join("ses_abc.json"),
        r#"{"id":"ses_abc","title":"chat one","directory":"/proj",
            "time":{"created":1000,"updated":2000}}"#,
    )
    .unwrap();
    std::fs::write(
        sess.join("ses_def.json"),
        r#"{"id":"ses_def","title":"chat two","time":{"created":500,"updated":600}}"#,
    )
    .unwrap();

    let msg = root.join("message/ses_abc");
    std::fs::create_dir_all(&msg).unwrap();
    std::fs::write(
        msg.join("msg_1.json"),
        r#"{"id":"msg_1","sessionID":"ses_abc","role":"user","time":{"created":1001}}"#,
    )
    .unwrap();
    std::fs::write(
        msg.join("msg_2.json"),
        r#"{"id":"msg_2","sessionID":"ses_abc","role":"assistant",
            "agent":"explore","model":{"providerID":"opencode","modelID":"big-pickle"},
            "time":{"created":1002}}"#,
    )
    .unwrap();

    let part1 = root.join("part/msg_1");
    std::fs::create_dir_all(&part1).unwrap();
    std::fs::write(
        part1.join("prt_1a.json"),
        r#"{"id":"prt_1a","messageID":"msg_1","type":"text","text":"please investigate"}"#,
    )
    .unwrap();
    let part2 = root.join("part/msg_2");
    std::fs::create_dir_all(&part2).unwrap();
    std::fs::write(
        part2.join("prt_2a.json"),
        r#"{"id":"prt_2a","messageID":"msg_2","type":"text","text":"here is what I found"}"#,
    )
    .unwrap();
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
fn migrate_list_prints_newest_first() {
    let tmp = tempfile::tempdir().unwrap();
    seed_fixture(tmp.path());
    let storage = tmp.path().to_str().unwrap();
    let (out, err, code) = run(
        tmp.path(),
        &[
            "migrate",
            "list",
            "--from",
            "opencode",
            "--storage",
            storage,
        ],
    );
    assert_eq!(code, 0, "list exit: stderr={err}");
    let abc_pos = out.find("ses_abc").expect("abc listed");
    let def_pos = out.find("ses_def").expect("def listed");
    assert!(abc_pos < def_pos, "abc (newer) should come first:\n{out}");
    assert!(out.contains("chat one"), "want title: {out}");
}

#[test]
fn migrate_list_reports_empty_when_storage_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let (out, _, code) = run(
        tmp.path(),
        &[
            "migrate",
            "list",
            "--from",
            "opencode",
            "--storage",
            tmp.path().join("nope").to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0);
    assert!(out.contains("no sessions"), "want empty hint: {out}");
}

#[test]
fn migrate_import_writes_jsonl_transcript() {
    let tmp = tempfile::tempdir().unwrap();
    seed_fixture(tmp.path());
    let storage = tmp.path().to_str().unwrap();
    let out_file = tmp.path().join("import.jsonl");
    let (stdout, stderr, code) = run(
        tmp.path(),
        &[
            "migrate",
            "import",
            "ses_abc",
            "--from",
            "opencode",
            "--storage",
            storage,
            "--out",
            out_file.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "import exit: stderr={stderr}\nstdout={stdout}");
    let body = std::fs::read_to_string(&out_file).unwrap();
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 2, "want 2 messages, got:\n{body}");
    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(first["role"], "user");
    assert_eq!(first["text"], "please investigate");
    assert_eq!(first["source"], "opencode");
    assert_eq!(second["role"], "assistant");
    assert_eq!(second["text"], "here is what I found");
    assert_eq!(second["agent"], "explore");
    assert_eq!(second["model"], "opencode/big-pickle");
}

#[test]
fn migrate_import_unknown_session_errors() {
    let tmp = tempfile::tempdir().unwrap();
    seed_fixture(tmp.path());
    let (_, stderr, code) = run(
        tmp.path(),
        &[
            "migrate",
            "import",
            "ses_nope",
            "--from",
            "opencode",
            "--storage",
            tmp.path().to_str().unwrap(),
            "--out",
            tmp.path().join("out.jsonl").to_str().unwrap(),
        ],
    );
    assert_ne!(code, 0, "want failure on unknown session");
    assert!(
        stderr.contains("no messages directory"),
        "want missing-dir error, got: {stderr}"
    );
}
