use atman_dsl::parse::parse_file;
use atman_runtime::fs_access::{FsAccessMode, FsAccessPolicy};
use atman_runtime::{Executor, Value, tools};

#[tokio::test]
async fn term_spawn_in_flow_captures_terminal_output() {
    let src = r#"flow t() -> string {
    contract { capabilities { shell: true } }
    h = term.spawn(cmd: "echo STREAM_MARKER_99", rows: 5, cols: 40)
    bash.spawn(block: true, cmd: "sleep 0.3", block_timeout_ms: 2000)
    c = term.capture(handle: h.handle, format: "text")
    term.kill(handle: h.handle)
    return c.text
}
"#;
    let file = parse_file(src).unwrap();
    let mut ex = Executor::new();
    tools::register_tier_zero(&ex.tools);
    let bg = tools::register_bash_bg(&ex.tools);
    let term_reg = tools::register_terminal(&ex.tools);
    let dir = std::env::temp_dir().join(format!("atman_term_tui_test_{}", uuid::Uuid::now_v7()));
    ex.tool_ctx = ex
        .tool_ctx
        .clone()
        .with_bg_registry(bg)
        .with_term_registry(term_reg)
        .with_session_dir(dir)
        .with_fs_access(FsAccessPolicy {
            mode: FsAccessMode::WorkspaceWrite,
            workspace: Some(std::env::current_dir().unwrap()),
        });

    let out = ex.run(&file, "t", vec![]).await.unwrap();
    let text = match out {
        Value::Str(s) => s,
        _ => panic!("expected string"),
    };
    assert!(
        text.contains("STREAM_MARKER_99"),
        "capture should contain marker: {text}"
    );
}
