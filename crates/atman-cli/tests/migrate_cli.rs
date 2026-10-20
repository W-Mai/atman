use std::path::Path;
use std::process::Command;

fn atman_bin() -> &'static str {
    env!("CARGO_BIN_EXE_atman")
}

#[test]
fn migrate_archive_requires_confirmation_and_retains_missing_project_history_idempotently() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let id = uuid::Uuid::now_v7().to_string();
    let source_dir = data.join("sessions").join(&id);
    std::fs::create_dir_all(&source_dir).unwrap();
    let bytes = b"{\"type\":\"mermaid_diagram\",\"source\":\"private history\"}\n{torn";
    std::fs::write(source_dir.join("events.jsonl"), bytes).unwrap();
    let missing_project = temp.path().join("missing-project");
    let meta = serde_json::to_vec(
        &serde_json::json!({"title":"Retained title", "project_root":missing_project}),
    )
    .unwrap();
    std::fs::write(source_dir.join("meta.json"), &meta).unwrap();
    std::fs::write(data.join("config.toml"), "# untouched legacy config\n").unwrap();
    let command = |extra: &[&str]| {
        Command::new(atman_bin())
            .args(["migrate", "archive", &id[..8]])
            .args(extra)
            .current_dir(temp.path())
            .env("ATMAN_DATA_DIR", &data)
            .env("ATMAN_CONFIG_DIR", temp.path().join("config"))
            .output()
            .unwrap()
    };
    let rejected = command(&[]);
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("--confirm-stopped"));
    assert!(!data.join("runtime-v1").exists());
    let output = command(&["--confirm-stopped"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["migrated_from"]["session_id"], id);
    assert_eq!(report["migrated_from"]["records"], 2);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private history"));
    let again = command(&["--confirm-stopped"]);
    assert!(
        again.status.success(),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&again.stdout).unwrap(),
        report
    );
    let retained = data
        .join("runtime-v1/sessions")
        .join(report["session_id"].as_str().unwrap());
    let projection: serde_json::Value =
        serde_json::from_slice(&std::fs::read(retained.join("session.json")).unwrap()).unwrap();
    assert_eq!(projection["value"]["descriptor"]["title"], "Retained title");
    assert_eq!(
        projection["value"]["descriptor"]["project"]["root"],
        temp.path()
            .canonicalize()
            .unwrap()
            .join("missing-project")
            .to_str()
            .unwrap()
    );
    assert_eq!(
        std::fs::read(retained.join("artifacts/legacy-events.jsonl")).unwrap(),
        bytes
    );
    assert!(!missing_project.exists());
    assert!(!temp.path().join("config").exists());
    assert_eq!(
        std::fs::read(source_dir.join("events.jsonl")).unwrap(),
        bytes
    );
    assert_eq!(std::fs::read(source_dir.join("meta.json")).unwrap(), meta);
    assert_eq!(
        std::fs::read_to_string(data.join("config.toml")).unwrap(),
        "# untouched legacy config\n"
    );

    std::fs::write(source_dir.join("events.jsonl"), b"late legacy write\n").unwrap();
    let conflict = command(&["--confirm-stopped"]);
    assert!(!conflict.status.success());
    assert!(String::from_utf8_lossy(&conflict.stderr).contains("source or destination changed"));
    let branch = uuid::Uuid::now_v7().to_string();
    let branched = command(&["--confirm-stopped", "--into", &branch]);
    assert!(
        branched.status.success(),
        "{}",
        String::from_utf8_lossy(&branched.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&branched.stdout).unwrap()["session_id"],
        branch
    );
    assert_eq!(
        std::fs::read_dir(data.join("runtime-v1/sessions"))
            .unwrap()
            .count(),
        2
    );
}

#[test]
fn migrate_inspect_reports_original_records_without_rewriting_or_starting_a_session() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let id = uuid::Uuid::now_v7().to_string();
    let session = data.join("sessions").join(&id);
    std::fs::create_dir_all(&session).unwrap();
    std::fs::write(
        data.join("config.toml"),
        "# retained legacy configuration\n",
    )
    .unwrap();
    let path = session.join("events.jsonl");
    let bytes = b"{\"type\":\"mermaid_diagram\",\"source\":\"private diagram\"}\n{\"type\":\"future_event\"}\n{torn";
    std::fs::write(&path, bytes).unwrap();
    let output = Command::new(atman_bin())
        .args(["migrate", "inspect", &id[..8]])
        .current_dir(temp.path())
        .env("ATMAN_DATA_DIR", &data)
        .env("ATMAN_CONFIG_DIR", temp.path().join("config"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["session_id"], id);
    assert_eq!(report["records"].as_array().unwrap().len(), 3);
    assert_eq!(report["records"][0]["owner"]["kind"], "unassigned");
    assert_eq!(report["records"][2]["issues"][0], "unterminated_line");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private diagram"));
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(std::fs::read_dir(&session).unwrap().count(), 1);
    assert_eq!(std::fs::read_dir(data.join("sessions")).unwrap().count(), 1);
    assert!(!data.join("runtime-v1").exists());
    assert_eq!(
        std::fs::read_to_string(data.join("config.toml")).unwrap(),
        "# retained legacy configuration\n"
    );
    assert!(!temp.path().join("config").exists());
    assert_eq!(std::fs::read_dir(&data).unwrap().count(), 2);

    let missing = Command::new(atman_bin())
        .args(["migrate", "inspect", &uuid::Uuid::now_v7().to_string()])
        .current_dir(temp.path())
        .env("ATMAN_DATA_DIR", &data)
        .env("ATMAN_CONFIG_DIR", temp.path().join("config"))
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(missing.stdout.is_empty());
    assert_eq!(std::fs::read_dir(data.join("sessions")).unwrap().count(), 1);
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
fn migrate_import_into_new_publishes_self_contained_read_only_history() {
    let tmp = tempfile::tempdir().unwrap();
    seed_fixture(tmp.path());
    let storage = tmp.path().to_str().unwrap();
    let data = tmp.path().join("atman_data");
    std::fs::create_dir_all(&data).unwrap();
    let config = tmp.path().join("config");
    std::fs::create_dir(&config).unwrap();
    std::fs::write(config.join("config.toml"), "invalid [ configuration").unwrap();
    std::fs::write(data.join("config.toml"), "# untouched legacy config\n").unwrap();

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
        .env("ATMAN_CONFIG_DIR", &config)
        .env_remove("ATMAN_DISABLE_MIGRATION")
        .output()
        .expect("spawn atman");
    assert!(
        out.status.success(),
        "import --into new exit: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("archived 2 messages"),
        "want archive count, got: {stdout}"
    );

    use atman_runtime::flow_store::RuntimeStore;
    let ids = RuntimeStore::session_ids(&data).unwrap();
    assert_eq!(ids.len(), 1);
    let session = RuntimeStore::inspect_session(&data, ids[0]).unwrap();
    let flow = session.projection.default_flow.as_ref().unwrap();
    let view = session.archived_view(flow).unwrap().unwrap();
    let archive = view.transcript().archived_transcript().unwrap();
    let texts: Vec<_> = archive
        .entries
        .iter()
        .filter_map(|entry| match entry {
            atman_runtime::TranscriptEntry::Message { message, .. } => Some(message.text_concat()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["please investigate", "here is what I found"]);
    assert!(archive.unparsed.is_empty());
    let snapshot = data
        .join("runtime-v1/sessions")
        .join(ids[0].to_string())
        .join("artifacts/legacy-events.jsonl");
    let records: Vec<serde_json::Value> = std::fs::read_to_string(snapshot)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records[0]["ts"], "1970-01-01T00:00:01.001Z");
    assert_eq!(records[1]["imported_from"]["source"], "opencode");
    assert_eq!(records[1]["imported_from"]["session_id"], "ses_abc");
    assert_eq!(records[1]["imported_from"]["agent"], "explore");
    assert_eq!(records[1]["imported_from"]["model"], "opencode/big-pickle");
    assert_eq!(records[0]["turn_id"], records[1]["turn_id"]);
    assert!(!data.join("sessions").exists());
    assert!(!data.join("runtime-v1/host").exists());
    assert_eq!(
        std::fs::read_to_string(data.join("config.toml")).unwrap(),
        "# untouched legacy config\n"
    );
    assert_eq!(
        std::fs::read_to_string(config.join("config.toml")).unwrap(),
        "invalid [ configuration"
    );

    // The published archive retains history after both adapter input and its
    // private normalized snapshot have disappeared.
    assert!(
        !session
            .projection
            .migrated_from
            .as_ref()
            .unwrap()
            .source
            .path
            .exists()
    );
    for part in ["session", "message", "part"] {
        std::fs::remove_dir_all(tmp.path().join(part)).unwrap();
    }
    let output = Command::new(atman_bin())
        .args(["--continue", &ids[0].to_string()])
        .env("ATMAN_DATA_DIR", &data)
        .env("ATMAN_CONFIG_DIR", &config)
        .env("ATMAN_DISABLE_MIGRATION", "1")
        .env("ATMAN_REPL_NON_INTERACTIVE", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("read-only") && text.contains("here is what I found"),
        "{text}"
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
