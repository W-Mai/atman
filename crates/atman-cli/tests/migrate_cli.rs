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
fn migrate_import_picker_reads_stdin_selection() {
    let tmp = tempfile::tempdir().unwrap();
    seed_fixture(tmp.path());
    let out_file = tmp.path().join("picked.jsonl");
    let mut cmd = Command::new(atman_bin());
    cmd.args([
        "migrate",
        "import",
        "--from",
        "opencode",
        "--storage",
        tmp.path().to_str().unwrap(),
        "--out",
        out_file.to_str().unwrap(),
    ])
    .current_dir(tmp.path())
    .stdin(std::process::Stdio::piped())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().expect("spawn atman migrate import");
    use std::io::Write;
    child.stdin.as_mut().unwrap().write_all(b"1\n").unwrap();
    let output = child.wait_with_output().expect("wait");
    assert!(
        output.status.success(),
        "picker exit: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let listing = String::from_utf8_lossy(&output.stderr);
    assert!(
        listing.contains("ses_abc") && listing.contains("pick number 1-2"),
        "picker menu missing, stderr={listing}"
    );
    let body = std::fs::read_to_string(&out_file).unwrap();
    assert_eq!(body.lines().count(), 2);
    assert!(body.contains("please investigate"));
}

#[test]
fn migrate_import_picker_rejects_out_of_range_pick() {
    let tmp = tempfile::tempdir().unwrap();
    seed_fixture(tmp.path());
    let out_file = tmp.path().join("bad.jsonl");
    let mut cmd = Command::new(atman_bin());
    cmd.args([
        "migrate",
        "import",
        "--from",
        "opencode",
        "--storage",
        tmp.path().to_str().unwrap(),
        "--out",
        out_file.to_str().unwrap(),
    ])
    .current_dir(tmp.path())
    .stdin(std::process::Stdio::piped())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().expect("spawn atman migrate import");
    use std::io::Write;
    child.stdin.as_mut().unwrap().write_all(b"99\n").unwrap();
    let output = child.wait_with_output().expect("wait");
    assert!(
        !output.status.success(),
        "want failure on out-of-range pick"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("out of range"),
        "want out-of-range hint, stderr={stderr}"
    );
    assert!(!out_file.exists(), "no file should be written on abort");
}

#[test]
fn migrate_import_picker_empty_pick_aborts() {
    let tmp = tempfile::tempdir().unwrap();
    seed_fixture(tmp.path());
    let out_file = tmp.path().join("empty.jsonl");
    let mut cmd = Command::new(atman_bin());
    cmd.args([
        "migrate",
        "import",
        "--from",
        "opencode",
        "--storage",
        tmp.path().to_str().unwrap(),
        "--out",
        out_file.to_str().unwrap(),
    ])
    .current_dir(tmp.path())
    .stdin(std::process::Stdio::piped())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().expect("spawn atman migrate import");
    use std::io::Write;
    child.stdin.as_mut().unwrap().write_all(b"\n").unwrap();
    let output = child.wait_with_output().expect("wait");
    assert!(!output.status.success(), "want failure on empty pick");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no pick given"),
        "want abort hint, stderr={stderr}"
    );
}

#[test]
fn migrate_import_into_new_writes_session_events() {
    let tmp = tempfile::tempdir().unwrap();
    seed_fixture(tmp.path());
    let storage = tmp.path().to_str().unwrap();
    let data = tmp.path().join("atman_data");
    std::fs::create_dir_all(&data).unwrap();

    let out = Command::new(atman_bin())
        .args([
            "migrate",
            "import",
            "ses_abc",
            "--from",
            "opencode",
            "--storage",
            storage,
            "--into",
            "new",
        ])
        .current_dir(tmp.path())
        .env("ATMAN_DATA_DIR", data.to_str().unwrap())
        .env("ATMAN_DISABLE_MIGRATION", "1")
        .output()
        .expect("spawn atman");
    assert!(
        out.status.success(),
        "import --into new exit: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("replayed 2 messages"),
        "want replay count, got: {stdout}"
    );

    let sessions_root = data.join("sessions");
    let session_dirs: Vec<_> = std::fs::read_dir(&sessions_root)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    assert_eq!(
        session_dirs.len(),
        1,
        "want one session dir: {session_dirs:?}"
    );
    let events = std::fs::read_to_string(session_dirs[0].join("events.jsonl")).unwrap();
    let user_hits = events
        .lines()
        .filter(|l| l.contains("\"type\":\"user_msg\""))
        .count();
    let asst_hits = events
        .lines()
        .filter(|l| l.contains("\"type\":\"assistant_msg\""))
        .count();
    assert_eq!(user_hits, 1, "want 1 user_msg event: {events}");
    assert_eq!(asst_hits, 1, "want 1 assistant_msg event: {events}");
    assert!(
        events.contains("migrated from opencode"),
        "want provenance tag in event body: {events}"
    );
}

#[test]
fn migrate_import_without_out_or_into_bails() {
    let tmp = tempfile::tempdir().unwrap();
    seed_fixture(tmp.path());
    let (_, stderr, code) = run(
        tmp.path(),
        &[
            "migrate",
            "import",
            "ses_abc",
            "--from",
            "opencode",
            "--storage",
            tmp.path().to_str().unwrap(),
        ],
    );
    assert_ne!(code, 0, "want failure without sink");
    assert!(
        stderr.contains("--out") && stderr.contains("--into"),
        "want sink-required hint, got: {stderr}"
    );
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

fn seed_kiro_fixture(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(
        root.join("aaa.json"),
        r#"{"session_id":"aaa","cwd":"/proj","created_at":"2026-04-09T18:52:46.845470Z",
            "title":"kiro chat"}"#,
    )
    .unwrap();
    std::fs::write(
        root.join("aaa.jsonl"),
        r#"{"version":"v1","kind":"Prompt","data":{"content":[{"kind":"text","data":"kiro hello"}],"meta":{"timestamp":1000}}}
{"version":"v1","kind":"AssistantMessage","data":{"content":[{"kind":"text","data":"kiro reply"}]}}
"#,
    )
    .unwrap();
}

#[test]
fn migrate_list_from_kiro_cli_source() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cli");
    seed_kiro_fixture(&root);
    let (out, err, code) = run(
        tmp.path(),
        &[
            "migrate",
            "list",
            "--from",
            "kiro-cli",
            "--storage",
            root.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "list exit: stderr={err}");
    assert!(out.contains("aaa"), "want aaa listed: {out}");
    assert!(out.contains("kiro chat"), "want title: {out}");
}

#[test]
fn migrate_import_from_kiro_writes_jsonl() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cli");
    seed_kiro_fixture(&root);
    let out_file = tmp.path().join("kiro.jsonl");
    let (stdout, stderr, code) = run(
        tmp.path(),
        &[
            "migrate",
            "import",
            "aaa",
            "--from",
            "kiro-cli",
            "--storage",
            root.to_str().unwrap(),
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
    assert_eq!(first["text"], "kiro hello");
    assert_eq!(first["source"], "kiro-cli");
    assert_eq!(second["role"], "assistant");
    assert_eq!(second["text"], "kiro reply");
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
