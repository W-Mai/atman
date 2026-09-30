use atman_rt::parse_file;
use atman_runtime::fs_access::{FsAccessMode, FsAccessPolicy};
use atman_runtime::{Executor, tools};
type Value = atman_rt::Value<atman_runtime::AtmanPayload, atman_runtime::RuntimeError>;

async fn run_terminal_flow(src: &str) -> Value {
    let file = parse_file(src).unwrap();
    let mut ex = Executor::new();
    tools::register_tier_zero(&ex.tools);
    let term_reg = tools::register_terminal(&ex.tools);
    let dir = tempfile::tempdir().unwrap();
    ex.tool_ctx = ex
        .tool_ctx
        .clone()
        .with_term_registry(term_reg)
        .with_session_dir(dir.path().to_path_buf())
        .with_fs_access(FsAccessPolicy {
            mode: FsAccessMode::WorkspaceWrite,
            workspace: Some(std::env::current_dir().unwrap()),
        })
        .with_trust(atman_runtime::trust::TrustConfig {
            mode: atman_runtime::trust::TrustMode::Reckless,
            ..atman_runtime::trust::TrustConfig::default()
        });

    ex.run(&file, "t", vec![]).await.unwrap()
}

#[tokio::test]
async fn term_spawn_in_flow_captures_terminal_output() {
    let src = r#"flow t() -> string {
    contract { capabilities { shell: true } }
    h = term.spawn(cmd: "echo STREAM_MARKER_99", rows: 5, cols: 40)
    c = term.capture(handle: h.handle, format: "text")
    term.kill(handle: h.handle)
    return c.text
}
"#;
    let out = run_terminal_flow(src).await;
    let text = match out {
        Value::Str(s) => s,
        _ => panic!("expected string"),
    };
    assert!(
        text.contains("STREAM_MARKER_99"),
        "capture should contain marker: {text}"
    );
}

#[tokio::test]
async fn term_input_in_flow_captures_response_without_an_extra_wait() {
    let src = r#"flow t() -> string {
    contract { capabilities { shell: true } }
    h = term.spawn(cmd: "stty -echo; read line; echo ACK:$line", rows: 5, cols: 40)
    term.input(handle: h.handle, text: "INPUT_MARKER_42", key: "enter")
    c = term.capture(handle: h.handle, format: "text")
    term.kill(handle: h.handle)
    return c.text
}
"#;
    let out = run_terminal_flow(src).await;
    let text = match out {
        Value::Str(s) => s,
        _ => panic!("expected string"),
    };
    assert!(
        text.contains("ACK:INPUT_MARKER_42"),
        "capture should contain the command response: {text}"
    );
}

#[tokio::test]
async fn term_resize_in_flow_updates_find_without_an_extra_wait() {
    let src = r#"flow t() -> int {
    contract { capabilities { shell: true } }
    h = term.spawn(cmd: "trap 'printf SIZE:; stty size' WINCH; printf READY; while read line; do :; done", rows: 5, cols: 20, wait_ms: 1000)
    term.resize(handle: h.handle, rows: 7, cols: 30, wait_ms: 1000)
    found = term.find(handle: h.handle, pattern: "SIZE:7 30")
    term.kill(handle: h.handle)
    return found.count
}
"#;
    let out = run_terminal_flow(src).await;
    match out {
        Value::Int(count) => assert_eq!(count, 1),
        _ => panic!("expected integer"),
    }
}
