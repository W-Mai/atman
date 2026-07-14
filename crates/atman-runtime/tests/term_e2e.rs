use atman_runtime::stream::StreamFrame;
use atman_runtime::tool::{Tool, ToolArgs};
use atman_runtime::tools::{self, term::TermSpawn};
use atman_runtime::{Executor, Value};
use tokio::sync::broadcast;

#[tokio::test]
async fn term_spawn_emits_terminal_chunk_to_stream() {
    let (stream_tx, mut rx) = broadcast::channel::<StreamFrame>(256);
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    let term_reg = tools::register_terminal(&mut ex.tools);
    let dir = std::env::temp_dir().join(format!("atman_term_e2e_{}", uuid::Uuid::now_v7()));
    ex.tool_ctx = ex
        .tool_ctx
        .clone()
        .with_term_registry(term_reg)
        .with_session_dir(dir);
    ex.tool_ctx.stream_tx = Some(stream_tx);

    let args = ToolArgs {
        positional: vec![],
        named: vec![
            ("cmd".into(), Value::Str("echo hello_term_test".into())),
            ("rows".into(), Value::Int(5)),
            ("cols".into(), Value::Int(40)),
        ],
    };
    let v = TermSpawn.call(args, &ex.tool_ctx).await.unwrap();
    let Value::Struct(_) = v else {
        panic!("expected struct")
    };

    let mut got_chunk = false;
    let mut got_exited = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(StreamFrame::TerminalChunk { bytes, .. })) => {
                if String::from_utf8_lossy(&bytes).contains("hello_term_test") {
                    got_chunk = true;
                }
            }
            Ok(Ok(StreamFrame::TerminalExited { .. })) => {
                got_exited = true;
                if got_chunk {
                    break;
                }
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }
    assert!(got_chunk, "TerminalChunk with echo output not received");
    assert!(got_exited, "TerminalExited not received");
}
