use std::path::Path;
use std::process::Command;

fn atman_bin() -> &'static str {
    env!("CARGO_BIN_EXE_atman")
}

fn seed_session(root: &Path, sid: &str, model: &str, input: u64, output: u64, wall: u64) {
    let dir = root.join("sessions").join(sid);
    std::fs::create_dir_all(&dir).unwrap();
    let line = format!(
        r#"{{"type":"llm_call","seq":1,"model":"{model}","provider":"mock","usage":{{"input":{input},"cached_input":0,"output":{output},"cache_write":0}},"wallclock_ms":{wall},"status":"ok","ts":"2026-07-05T12:00:00Z"}}"#,
    );
    std::fs::write(dir.join("events.jsonl"), format!("{line}\n")).unwrap();
}

fn run_cost(data: &Path, args: &[&str]) -> (String, String, i32) {
    let out = Command::new(atman_bin())
        .args(args)
        .env("ATMAN_DATA_DIR", data.to_str().unwrap())
        .output()
        .expect("spawn atman");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn cost_all_aggregates_across_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("atman_data");
    std::fs::create_dir_all(&data).unwrap();
    seed_session(&data, "ses_a", "gpt-4o-mini", 100, 50, 900);
    seed_session(&data, "ses_b", "gpt-4o-mini", 200, 80, 1200);
    seed_session(&data, "ses_c", "claude-3-5-sonnet", 500, 300, 4000);

    let (out, err, code) = run_cost(&data, &["cost", "--all"]);
    assert_eq!(code, 0, "cost --all exit: stderr={err}\nstdout={out}");
    assert!(
        out.contains("across 3 session(s)"),
        "want session count: {out}"
    );
    assert!(
        out.contains("total llm_calls: 3"),
        "want total 3 calls: {out}"
    );
    let gpt_line = out
        .lines()
        .find(|l| l.trim_start().starts_with("gpt-4o-mini"))
        .expect("want per-model row for gpt-4o-mini");
    assert!(
        gpt_line.contains("300") && gpt_line.contains("130"),
        "gpt model totals wrong (in=300 out=130), got: {gpt_line}"
    );
    let claude_line = out
        .lines()
        .find(|l| l.trim_start().starts_with("claude-3-5-sonnet"))
        .expect("want per-model row for claude-3-5-sonnet");
    assert!(
        claude_line.contains("500") && claude_line.contains("300"),
        "claude totals wrong: {claude_line}"
    );

    let a_line = out
        .lines()
        .find(|l| l.trim_start().starts_with("ses_a"))
        .expect("want per-session ses_a row");
    assert!(a_line.contains("900"), "ses_a wall_ms=900: {a_line}");
    let b_line = out
        .lines()
        .find(|l| l.trim_start().starts_with("ses_b"))
        .unwrap();
    assert!(b_line.contains("1200"), "ses_b wall_ms=1200: {b_line}");
}

#[test]
fn cost_all_reports_empty_when_no_llm_calls() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("atman_data");
    std::fs::create_dir_all(data.join("sessions/ses_empty")).unwrap();
    std::fs::write(
        data.join("sessions/ses_empty/events.jsonl"),
        r#"{"type":"turn_start","seq":1,"turn_id":{"raw":"t1"},"ts":"2026-07-05T12:00:00Z"}
"#,
    )
    .unwrap();
    let (out, err, code) = run_cost(&data, &["cost", "--all"]);
    assert_eq!(code, 0, "cost --all exit: stderr={err}");
    assert!(out.contains("no llm_call events"), "want empty hint: {out}");
}

#[test]
fn cost_all_conflicts_with_positional_session_id() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("atman_data");
    std::fs::create_dir_all(&data).unwrap();
    let (_, err, code) = run_cost(&data, &["cost", "ses_x", "--all"]);
    assert_ne!(code, 0);
    assert!(
        err.contains("conflict") || err.contains("cannot be used"),
        "want clap conflict error: {err}"
    );
}

#[test]
fn cost_single_session_still_works_without_all_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("atman_data");
    std::fs::create_dir_all(&data).unwrap();
    seed_session(&data, "ses_a", "gpt-4o-mini", 42, 8, 100);
    let (out, err, code) = run_cost(&data, &["cost", "ses_a"]);
    assert_eq!(code, 0, "cost single: stderr={err}");
    assert!(out.contains("session ses_a"), "want session heading: {out}");
    assert!(out.contains("total llm_calls: 1"), "want 1 call: {out}");
}
