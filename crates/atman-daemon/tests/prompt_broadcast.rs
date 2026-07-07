use std::sync::Arc;
use std::time::Duration;

use atman_daemon::DaemonState;
use atman_daemon::prompt_bridge::DaemonPromptResolver;
use atman_dsl::parse::parse_file;
use atman_runtime::event::{Event, EventSink};
use atman_runtime::{Executor, tools};
use tempfile::TempDir;

#[tokio::test(flavor = "multi_thread")]
async fn hunk_review_emits_pending_and_resolved_events_to_shared_sink() {
    let tmp = TempDir::new().unwrap();
    let daemon_state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let sink = EventSink::new();

    let file_path = tmp.path().join("input.txt");
    std::fs::write(&file_path, "line1\nline2\nline3\n").unwrap();

    let mut ex = Executor::with_events(sink.clone());
    tools::register_tier_zero(&mut ex.tools);
    ex.tool_ctx.prompt_resolver = Some(Arc::new(DaemonPromptResolver {
        state: daemon_state.clone(),
        sink: sink.clone(),
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
    let resolver_answer = serde_json::json!({"hunks": [1]});
    let answer_clone = resolver_answer.clone();
    let resolver_task = tokio::spawn(async move {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let pending = state_for_resolver.pending_prompt_ids();
            if let Some(pid) = pending.first() {
                let ok = state_for_resolver.resolve_prompt(pid, answer_clone);
                assert!(ok, "resolve should succeed");
                return;
            }
            if std::time::Instant::now() >= deadline {
                panic!("timed out waiting for pending prompt");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });

    let local = tokio::task::LocalSet::new();
    let _result = local
        .run_until(async move { ex.run(&file, "t", vec![]).await })
        .await
        .expect("flow ok");
    resolver_task.await.expect("resolver join");

    let events = sink.snapshot();

    let pending = events.iter().find_map(|e| match e {
        Event::PendingPrompt {
            prompt_id,
            kind,
            payload,
            ..
        } => Some((*prompt_id, kind.clone(), payload.clone())),
        _ => None,
    });
    let (pending_id, kind, payload) = pending.expect("expected PendingPrompt event");
    assert_eq!(kind, "hunk_selection");
    let hunks_json = payload
        .get("hunks")
        .and_then(|v| v.as_array())
        .expect("payload.hunks array");
    assert!(!hunks_json.is_empty(), "at least one hunk expected");
    let first = &hunks_json[0];
    assert!(first.get("id").is_some(), "hunk missing id: {first:?}");
    assert!(
        first
            .get("unified_diff")
            .and_then(|v| v.as_str())
            .map(|s| s.contains("rewritten"))
            .unwrap_or(false),
        "unified_diff must include the change: {first:?}"
    );
    let path_str = payload
        .get("path")
        .and_then(|v| v.as_str())
        .expect("payload.path");
    assert!(path_str.contains("input.txt"), "path: {path_str}");

    let resolved = events.iter().find_map(|e| match e {
        Event::PromptResolved {
            prompt_id, answer, ..
        } if *prompt_id == pending_id => Some(answer.clone()),
        _ => None,
    });
    let answer = resolved.expect("expected PromptResolved for the same prompt_id");
    assert_eq!(answer, resolver_answer);

    let mut pending_seq = None;
    let mut resolved_seq = None;
    for e in &events {
        match e {
            Event::PendingPrompt { seq, prompt_id, .. } if *prompt_id == pending_id => {
                pending_seq = Some(*seq);
            }
            Event::PromptResolved { seq, prompt_id, .. } if *prompt_id == pending_id => {
                resolved_seq = Some(*seq);
            }
            _ => {}
        }
    }
    let (p, r) = (pending_seq.unwrap(), resolved_seq.unwrap());
    assert!(p < r, "resolved seq {r} must come after pending seq {p}");
}

#[tokio::test(flavor = "multi_thread")]
async fn drop_pending_prompt_emits_null_answer_event() {
    let tmp = TempDir::new().unwrap();
    let daemon_state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let sink = EventSink::new();

    let pid = atman_proto::PromptId(uuid::Uuid::now_v7());
    let _rx = daemon_state.register_pending_prompt_broadcast(
        pid.clone(),
        "user_ask",
        serde_json::json!({"prompt": "still there?"}),
        sink.clone(),
    );

    daemon_state.drop_pending_prompt(&pid);

    let events = sink.snapshot();
    let has_pending = events
        .iter()
        .any(|e| matches!(e, Event::PendingPrompt { prompt_id, .. } if *prompt_id == pid.0));
    assert!(has_pending, "pending should be broadcast");
    let resolved = events.iter().find_map(|e| match e {
        Event::PromptResolved {
            prompt_id, answer, ..
        } if *prompt_id == pid.0 => Some(answer.clone()),
        _ => None,
    });
    assert_eq!(resolved.unwrap(), serde_json::Value::Null);
}
