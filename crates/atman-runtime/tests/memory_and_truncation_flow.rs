use std::sync::Arc;

use atman_dsl::parse::parse_file;
use atman_runtime::event::{Event, EventSink};
use atman_runtime::memory::confession::ConfessionStore;
use atman_runtime::memory::spec::SpecStore;
use atman_runtime::memory::todo::TodoStore;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::{Executor, Value, tools};
use tempfile::TempDir;

#[tokio::test]
async fn confess_three_and_fetch_returns_three_via_flow() {
    let dir = TempDir::new().unwrap();
    let confession = Arc::new(ConfessionStore::at(dir.path()));
    let todo = Arc::new(TodoStore::at(dir.path()));
    let spec = Arc::new(SpecStore::new(dir.path().to_path_buf()));

    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    tools::register_memory(&mut ex.tools, todo, confession.clone());
    tools::register_spec_memory(&mut ex.tools, spec);

    let src = r#"flow t() -> Int {
    memory.confess(trigger: "a", rule_violated: "r", what_i_did: "w", why: "y", mitigation: "m")
    memory.confess(trigger: "b", rule_violated: "r", what_i_did: "w", why: "y", mitigation: "m")
    memory.confess(trigger: "c", rule_violated: "r", what_i_did: "w", why: "y", mitigation: "m")
    all = memory.fetch_confessions()
    return len(all)
}
"#;
    let file = parse_file(src).unwrap();
    let val = ex.run(&file, "t", vec![]).await.expect("flow ok");
    match val {
        Value::Int(n) => assert_eq!(n, 3),
        other => panic!("expected int 3, got {other:?}"),
    }

    let all_stored = confession.list().await.unwrap();
    assert_eq!(all_stored.len(), 3);
    let triggers: Vec<&str> = all_stored.iter().map(|c| c.trigger.as_str()).collect();
    assert!(triggers.contains(&"a"));
    assert!(triggers.contains(&"b"));
    assert!(triggers.contains(&"c"));
}

#[tokio::test]
async fn long_prompt_triggers_context_truncated_event_and_flow_completes() {
    let src = format!(
        r#"flow t() -> string {{
    reply = llm {{
        model: "mock"
        prompt: "{}"
        context_budget: 100
    }}
    return reply
}}
"#,
        "long ".repeat(2000)
    );

    let sink = EventSink::new();
    let mut ex = Executor::with_events(sink.clone());
    tools::register_tier_zero(&mut ex.tools);
    ex.providers.register(Arc::new(
        MockProvider::new("mock").with_fallback(Value::Str("summary".into())),
    ));
    let file = parse_file(&src).unwrap();
    let val = ex.run(&file, "t", vec![]).await.expect("flow ok");
    match val {
        Value::Str(s) => assert_eq!(s, "summary"),
        other => panic!("expected string, got {other:?}"),
    }

    let events = sink.snapshot();
    let hit = events
        .iter()
        .find(|e| matches!(e, Event::ContextTruncated { .. }))
        .expect("expected ContextTruncated in long-flow scenario");
    match hit {
        Event::ContextTruncated {
            original_chars,
            result_chars,
            budget_tokens,
            ..
        } => {
            assert!(*original_chars > 5_000);
            assert!(*result_chars < *original_chars);
            assert_eq!(*budget_tokens, 100);
        }
        _ => unreachable!(),
    }
}
