use atman_dsl::parse::parse_file;
use atman_runtime::stream::StreamFrame;
use atman_runtime::{Executor, Value, tools};

#[tokio::test]
async fn term_spawn_in_flow_emits_terminal_chunk_to_stream() {
    let src = r#"flow t() -> string {
    contract { capabilities { shell: true } }
    h = term.spawn(cmd: "echo STREAM_MARKER_99", rows: 3, cols: 20)
    bash.spawn(block: true, cmd: "sleep 0.3", block_timeout_ms: 2000)
    c = term.capture(handle: h.handle, format: "text")
    term.kill(handle: h.handle)
    return c.text
}
"#;
    let file = parse_file(src).unwrap();
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    let bg = tools::register_bash_bg(&mut ex.tools);
    let term_reg = tools::register_terminal(&mut ex.tools);
    let dir = std::env::temp_dir().join(format!("atman_term_tui_test_{}", uuid::Uuid::now_v7()));
    ex.tool_ctx = ex
        .tool_ctx
        .clone()
        .with_bg_registry(bg)
        .with_term_registry(term_reg)
        .with_session_dir(dir);

    let (stream_tx, rx) = tokio::sync::broadcast::channel::<StreamFrame>(256);
    ex.tool_ctx.stream_tx = Some(stream_tx);
    let mut rx = rx;

    let out = ex.run(&file, "t", vec![]).await.unwrap();
    let text = match out {
        Value::Str(s) => s,
        _ => panic!("expected string"),
    };
    assert!(
        text.contains("STREAM_MARKER_99"),
        "capture should contain marker: {text}"
    );

    let mut got_chunk = false;
    let mut got_exited = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(StreamFrame::TerminalChunk { bytes, .. })) => {
                if String::from_utf8_lossy(&bytes).contains("STREAM_MARKER_99") {
                    got_chunk = true;
                }
            }
            Ok(Ok(StreamFrame::TerminalExited { .. })) => {
                got_exited = true;
                if got_chunk {
                    break;
                }
            }
            _ => break,
        }
    }
    assert!(
        got_chunk,
        "TerminalChunk with marker not received on stream"
    );
    assert!(got_exited, "TerminalExited not received");
}
