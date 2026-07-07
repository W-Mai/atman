use std::sync::Arc;
use std::time::Duration;

use atman_daemon::{DaemonState, prompt_bridge::DaemonPromptResolver};
use atman_dsl::parse::parse_file;
use atman_runtime::{Executor, Value, tools};
use tempfile::TempDir;

#[tokio::test(flavor = "multi_thread")]
async fn hunk_review_reuses_daemon_rendezvous_when_resolver_present() {
    let tmp = TempDir::new().unwrap();
    let daemon_state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));

    let file_path = tmp.path().join("input.txt");
    std::fs::write(&file_path, "line1\nline2\nline3\n").unwrap();

    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    ex.tool_ctx.prompt_resolver = Some(Arc::new(DaemonPromptResolver {
        state: daemon_state.clone(),
        sink: atman_runtime::event::EventSink::new(),
    }));

    let src = format!(
        r#"flow t() -> string {{
    proposal = hunk.plan_edit(
        path: "{}",
        new_content: "line1\nrewritten\nline3\n"
    )
    review = hunk.review(proposal: proposal)
    return to_json_string(review)
}}
"#,
        file_path.display()
    );

    let file = parse_file(&src).unwrap();

    let state_for_resolver = daemon_state.clone();
    let resolver_task = tokio::spawn(async move {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let pending = state_for_resolver.pending_prompt_ids();
            if let Some(pid) = pending.first() {
                assert!(state_for_resolver.resolve_prompt(pid, serde_json::json!({"hunks": [1]})));
                return;
            }
            if std::time::Instant::now() >= deadline {
                panic!("timed out waiting for pending prompt");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });

    let local = tokio::task::LocalSet::new();
    let result = local
        .run_until(async move { ex.run(&file, "t", vec![]).await })
        .await
        .expect("flow ok");

    resolver_task.await.expect("resolver join");

    let s = match result {
        Value::Str(s) => s,
        other => panic!("expected string, got {other:?}"),
    };
    assert!(
        s.contains("resolved"),
        "review mode should be resolved: {s}"
    );
    assert!(s.contains("hunks"), "review must contain hunks list: {s}");
}

#[tokio::test]
async fn hunk_review_falls_back_to_auto_when_no_resolver() {
    let tmp = TempDir::new().unwrap();
    let file_path = tmp.path().join("in.txt");
    std::fs::write(&file_path, "a\nb\nc\n").unwrap();

    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);

    let src = format!(
        r#"flow t() -> string {{
    proposal = hunk.plan_edit(
        path: "{}",
        new_content: "a\nX\nc\n"
    )
    review = hunk.review(proposal: proposal)
    return to_json_string(review)
}}
"#,
        file_path.display()
    );

    let file = parse_file(&src).unwrap();
    let result = ex.run(&file, "t", vec![]).await.expect("flow ok");
    let s = match result {
        Value::Str(s) => s,
        other => panic!("expected string, got {other:?}"),
    };
    assert!(
        s.contains("auto"),
        "review mode without resolver should be auto: {s}"
    );
}
