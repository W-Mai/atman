use atman_runtime::flow_authority::EffectiveAuthority;
use atman_runtime::fs_access::{FsAccessMode, FsAccessPolicy};
use atman_runtime::permission::PermissionBroker;
use atman_runtime::stream::StreamFrame;
use atman_runtime::tool::{Tool, ToolArgs};
use atman_runtime::tools::{self, agent_ctrl::FlowRegistry, term::TermSpawn};
use atman_runtime::trust::{TrustConfig, TrustMode};
use atman_runtime::{Executor, FlowRunId, Tier, Value};
use std::sync::Arc;
use tokio::sync::broadcast;

#[tokio::test]
async fn term_spawn_emits_terminal_chunk_to_stream() {
    let (stream_tx, mut rx) = broadcast::channel::<StreamFrame>(256);
    let mut ex = Executor::new();
    tools::register_tier_zero(&ex.tools);
    let term_reg = tools::register_terminal(&ex.tools);
    let dir = std::env::temp_dir().join(format!("atman_term_e2e_{}", uuid::Uuid::now_v7()));
    let trust = TrustConfig {
        mode: TrustMode::Reckless,
        ..TrustConfig::default()
    };
    let flows = Arc::new(FlowRegistry::new());
    let broker = PermissionBroker::shared(Arc::clone(&flows));
    let run_id = FlowRunId::now();
    let identity = flows
        .register_root(
            "term-e2e".into(),
            run_id.clone(),
            EffectiveAuthority::root(&trust, true, None),
        )
        .unwrap();
    ex.tool_ctx = ex
        .tool_ctx
        .clone()
        .with_term_registry(term_reg)
        .with_session_dir(dir)
        .with_trust(trust)
        .with_flow_registry(flows)
        .with_permission_broker(broker)
        .with_anchors(None, Some(run_id), None)
        .with_fs_access(FsAccessPolicy {
            mode: FsAccessMode::WorkspaceWrite,
            workspace: Some(std::env::current_dir().unwrap()),
        });
    ex.tool_ctx.flow_identity = Some(identity);
    ex.tool_ctx.stream_tx = Some(stream_tx);

    let args = ToolArgs {
        positional: vec![],
        named: vec![
            ("cmd".into(), Value::Str("echo hello_term_test".into())),
            ("rows".into(), Value::Int(5)),
            ("cols".into(), Value::Int(40)),
        ],
    };
    let invocation_ctx = ex.tool_ctx.clone().for_tool_invocation(Tier::Four);
    let call_ctx = atman_runtime::approval::authorize_tool_invocation(
        &invocation_ctx,
        "term-e2e-call",
        "term.spawn",
        &args,
        &TermSpawn,
    )
    .await
    .unwrap();
    let v = TermSpawn.call(args, &call_ctx).await.unwrap();
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
