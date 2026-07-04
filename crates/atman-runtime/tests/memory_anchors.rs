use std::sync::Arc;

use atman_runtime::memory::confession::{Confession, ConfessionStore};
use atman_runtime::memory::spec::SpecStore;
use atman_runtime::memory::todo::TodoStore;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::{Executor, Value, tools};
use tempfile::TempDir;

#[tokio::test]
async fn memory_confess_auto_fills_flow_run_and_turn_anchors_when_called_from_flow() {
    let tmp = TempDir::new().unwrap();
    let confession_store = Arc::new(ConfessionStore::at(tmp.path()));
    let todo_store = Arc::new(TodoStore::at(tmp.path()));
    let spec_store = Arc::new(SpecStore::new(tmp.path().to_path_buf()));

    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    tools::register_memory(&mut ex.tools, todo_store, confession_store.clone());
    tools::register_spec_memory(&mut ex.tools, spec_store);
    ex.providers.register(Arc::new(
        MockProvider::new("mock").with_fallback(Value::Str("ok".into())),
    ));

    let src = r#"flow t() -> string {
    id = memory.confess(
        trigger: "test trigger",
        rule_violated: "rule x",
        what_i_did: "did the thing",
        why: "because",
        mitigation: "wont again"
    )
    return id
}
"#;
    let file = atman_dsl::parse::parse_file(src).unwrap();
    ex.run(&file, "t", vec![]).await.expect("flow ok");

    let all: Vec<Confession> = confession_store.list().await.unwrap();
    assert_eq!(all.len(), 1);
    let c = &all[0];
    assert!(
        c.anchors.iter().any(|a| a.starts_with("flow_run:")),
        "expected flow_run anchor in {:?}",
        c.anchors
    );
    assert!(
        c.anchors.is_empty() || c.anchors.iter().any(|a| a.starts_with("event_seq:")),
        "expected event_seq anchor in {:?}",
        c.anchors
    );
}

#[tokio::test]
async fn memory_confess_appends_user_anchors_after_auto_anchors() {
    let tmp = TempDir::new().unwrap();
    let confession_store = Arc::new(ConfessionStore::at(tmp.path()));
    let todo_store = Arc::new(TodoStore::at(tmp.path()));
    let spec_store = Arc::new(SpecStore::new(tmp.path().to_path_buf()));

    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    tools::register_memory(&mut ex.tools, todo_store, confession_store.clone());
    tools::register_spec_memory(&mut ex.tools, spec_store);

    let src = r#"flow t() -> string {
    id = memory.confess(
        trigger: "t",
        rule_violated: "r",
        what_i_did: "w",
        why: "y",
        mitigation: "m",
        anchors: ["user:custom", "issue:42"]
    )
    return id
}
"#;
    let file = atman_dsl::parse::parse_file(src).unwrap();
    ex.run(&file, "t", vec![]).await.expect("flow ok");

    let all: Vec<Confession> = confession_store.list().await.unwrap();
    assert_eq!(all.len(), 1);
    let c = &all[0];
    assert!(c.anchors.contains(&"user:custom".to_string()));
    assert!(c.anchors.contains(&"issue:42".to_string()));
    // Auto anchor still present
    assert!(c.anchors.iter().any(|a| a.starts_with("flow_run:")));
}
