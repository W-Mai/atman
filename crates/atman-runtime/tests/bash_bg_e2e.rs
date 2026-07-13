use atman_dsl::parse::parse_file;
use atman_runtime::event::TurnId;
use atman_runtime::tools;
use atman_runtime::{Executor, Value};

async fn exec_with_bg(src: &str, flow: &str) -> Value {
    let file = parse_file(src).unwrap();
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    tools::register_shell(&mut ex.tools);
    let bg_registry = tools::register_bash_bg(&mut ex.tools);
    ex.tool_ctx = ex
        .tool_ctx
        .clone()
        .with_bg_registry(bg_registry)
        .with_session_dir(
            std::env::temp_dir().join(format!("atman_bash_bg_test_{}", uuid::Uuid::now_v7())),
        );
    ex.run(&file, flow, vec![]).await.unwrap()
}

#[tokio::test]
async fn bash_exec_timeout_kills_long_process() {
    let src = r#"flow t() -> string {
    contract { capabilities { shell: true } }
    r = bash.exec(cmd: "sleep 30", timeout_ms: 500)
    return to_json_string(r)
}
"#;
    let v = exec_with_bg(src, "t").await;
    let s = match v {
        Value::Str(s) => s,
        _ => panic!("expected str, got {v:?}"),
    };
    assert!(s.contains("timed_out"), "expected timed_out in {s}");
}

#[tokio::test]
async fn bash_exec_timeout_zero_means_no_timeout() {
    let src = r#"flow t() -> string {
    contract { capabilities { shell: true } }
    r = bash.exec(cmd: "echo fast", timeout_ms: 0)
    return to_json_string(r)
}
"#;
    let v = exec_with_bg(src, "t").await;
    let s = match v {
        Value::Str(s) => s,
        _ => panic!("expected str"),
    };
    assert!(s.contains("false"), "expected timed_out=false in {s}");
}

#[tokio::test]
async fn bash_spawn_returns_running_handle() {
    let src = r#"flow t() -> string {
    contract { capabilities { shell: true } }
    h = bash.spawn(cmd: "echo spawn_test")
    return to_json_string(h)
}
"#;
    let v = exec_with_bg(src, "t").await;
    let s = match v {
        Value::Str(s) => s,
        _ => panic!("expected str"),
    };
    assert!(s.contains("running"), "status=running in {s}");
    assert!(s.contains("bg_"), "handle in {s}");
}

#[tokio::test]
async fn bash_spawn_then_status_reaches_exited() {
    let src = r#"flow t() -> string {
    contract { capabilities { shell: true } }
    h = bash.spawn(cmd: "echo done")
    bash.exec(cmd: "sleep 0.3")
    s = bash.status(handle: h.handle)
    return to_json_string(s)
}
"#;
    let v = exec_with_bg(src, "t").await;
    let s = match v {
        Value::Str(s) => s,
        _ => panic!("expected str"),
    };
    assert!(s.contains("exited"), "expected status=exited in {s}");
    assert!(s.contains("exit_code"), "expected exit_code in {s}");
}

#[tokio::test]
async fn bash_spawn_output_captures_stdout() {
    let src = r#"flow t() -> string {
    contract { capabilities { shell: true } }
    h = bash.spawn(cmd: "echo line1; echo line2")
    bash.exec(cmd: "sleep 0.3")
    o = bash.output(handle: h.handle)
    return o.chunk
}
"#;
    let v = exec_with_bg(src, "t").await;
    let chunk = match v {
        Value::Str(s) => s,
        _ => panic!("expected str"),
    };
    assert!(
        chunk.contains("line1"),
        "chunk should contain line1: {chunk}"
    );
    assert!(
        chunk.contains("line2"),
        "chunk should contain line2: {chunk}"
    );
}

#[tokio::test]
async fn bash_kill_terminates_running_process() {
    let src = r#"flow t() -> string {
    contract { capabilities { shell: true } }
    h = bash.spawn(cmd: "sleep 100")
    bash.exec(cmd: "sleep 0.2")
    bash.kill(handle: h.handle)
    bash.exec(cmd: "sleep 0.3")
    s = bash.status(handle: h.handle)
    return to_json_string(s)
}
"#;
    let v = exec_with_bg(src, "t").await;
    let s = match v {
        Value::Str(s) => s,
        _ => panic!("expected str"),
    };
    assert!(s.contains("killed"), "expected status=killed in {s}");
}

#[tokio::test]
async fn bash_spawn_timeout_kills_process() {
    let src = r#"flow t() -> string {
    contract { capabilities { shell: true } }
    h = bash.spawn(cmd: "sleep 100", timeout_ms: 500)
    bash.exec(cmd: "sleep 1")
    s = bash.status(handle: h.handle)
    return to_json_string(s)
}
"#;
    let v = exec_with_bg(src, "t").await;
    let s = match v {
        Value::Str(s) => s,
        _ => panic!("expected str"),
    };
    assert!(s.contains("timed_out"), "expected status=timed_out in {s}");
}

#[tokio::test]
async fn bash_exec_output_overflow_truncates_stdout() {
    let src = r#"flow t() -> string {
    contract { capabilities { shell: true } }
    r = bash.exec(cmd: "yes overflow_line | head -200000")
    return r.stdout
}
"#;
    let v = exec_with_bg(src, "t").await;
    let s = match v {
        Value::Str(s) => s,
        _ => panic!("expected str"),
    };
    assert!(
        s.contains("truncated at"),
        "expected truncation marker in stdout tail: {}",
        &s[s.len().saturating_sub(200)..]
    );
}

#[tokio::test]
async fn bash_exec_process_group_kills_grandchildren() {
    let src = r#"flow t() -> string {
    contract { capabilities { shell: true } }
    bash.exec(cmd: "sleep 100 & sleep 100 & sleep 100 & wait", timeout_ms: 500)
    remaining = bash.exec(cmd: "pgrep -c sleep || echo 0")
    return remaining.stdout
}
"#;
    let v = exec_with_bg(src, "t").await;
    let count = match v {
        Value::Str(s) => s.trim().to_string(),
        _ => panic!("expected str"),
    };
    let n: i64 = count.parse().unwrap_or(999);
    assert_eq!(n, 0, "no orphan sleep processes should remain, got {count}");
}

#[tokio::test]
async fn bash_output_cursor_incremental_read() {
    let src = r#"flow t() -> string {
    contract { capabilities { shell: true } }
    h = bash.spawn(cmd: "for i in 1 2 3 4 5; do echo num_$i; done")
    bash.exec(cmd: "sleep 0.3")
    a = bash.output(handle: h.handle, cursor: 0, limit_bytes: 15)
    b = bash.output(handle: h.handle, cursor: a.next_cursor, limit_bytes: 1000)
    return to_json_string([a.chunk, b.chunk, b.eof])
}
"#;
    let v = exec_with_bg(src, "t").await;
    let s = match v {
        Value::Str(s) => s,
        _ => panic!("expected str"),
    };
    assert!(s.contains("num_1"), "first chunk should contain num_1: {s}");
    assert!(s.contains("num_5"), "second chunk should reach num_5: {s}");
    assert!(s.contains("true"), "eof should be true: {s}");
}

#[tokio::test]
async fn bash_spawn_cross_session_rejected() {
    let src = r#"flow t() -> string {
    contract { capabilities { shell: true } }
    h = bash.spawn(cmd: "sleep 100")
    return h.handle
}
"#;
    let file = parse_file(src).unwrap();
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    tools::register_shell(&mut ex.tools);
    let bg = tools::register_bash_bg(&mut ex.tools);
    let turn_a = TurnId::now();
    ex.tool_ctx = ex.tool_ctx.clone().with_bg_registry(bg).with_session_dir(
        std::env::temp_dir().join(format!("atman_test_{}", uuid::Uuid::now_v7())),
    );
    let v = ex
        .run_in_turn(&file, "t", vec![], Some(turn_a.clone()), None)
        .await
        .unwrap();
    let handle = match v {
        Value::Str(s) => s,
        _ => panic!("expected str"),
    };

    let mut ex2 = Executor::new();
    tools::register_tier_zero(&mut ex2.tools);
    tools::register_shell(&mut ex2.tools);
    let bg2 = tools::register_bash_bg(&mut ex2.tools);
    let turn_b = TurnId::now();
    ex2.tool_ctx = ex2.tool_ctx.clone().with_bg_registry(bg2).with_session_dir(
        std::env::temp_dir().join(format!("atman_test_{}", uuid::Uuid::now_v7())),
    );

    let src2 = format!(
        r#"flow t() -> string {{
    contract {{ capabilities {{ shell: true }} }}
    s = bash.status(handle: "{handle}")
    return to_json_string(s)
}}
"#
    );
    let file2 = parse_file(&src2).unwrap();
    let err = ex2
        .run_in_turn(&file2, "t", vec![], Some(turn_b), None)
        .await
        .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("does not belong to session"),
        "expected cross-session rejection, got: {msg}"
    );
}
