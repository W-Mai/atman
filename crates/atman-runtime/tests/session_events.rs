use std::sync::Arc;

use atman_dsl::parse::parse_file;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::{Executor, Session, Value};
use tempfile::TempDir;

#[tokio::test]
async fn session_writes_events_to_jsonl_file() {
    let root = TempDir::new().unwrap();
    let session = Session::open(root.path()).unwrap();
    let events_path = session.events_path().unwrap().to_path_buf();

    let src = r#"flow t() -> string {
    return llm {
        model: "mock"
        prompt: "hi"
    }
}
"#;
    let file = parse_file(src).unwrap();
    let mut ex = Executor::with_events(session.sink().clone());
    ex.providers.register(Arc::new(
        MockProvider::new("mock").with_model("mock", Value::Str("ok".into())),
    ));
    ex.run(&file, "t", vec![]).await.unwrap();
    session.shutdown().await;

    let contents = tokio::fs::read_to_string(&events_path).await.unwrap();
    let lines: Vec<&str> = contents.lines().collect();

    let types: Vec<String> = lines
        .iter()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            v["type"].as_str().unwrap().to_string()
        })
        .collect();
    assert_eq!(
        types,
        vec!["flow_start", "flow_graph", "llm_call", "flow_end"]
    );
}

#[tokio::test]
async fn ephemeral_session_keeps_events_in_memory_only() {
    let session = Session::open_ephemeral();
    assert!(session.events_path().is_none());
    let sink = session.sink().clone();

    let src = r#"flow t() -> Int {
    return 1
}
"#;
    let file = parse_file(src).unwrap();
    let ex = Executor::with_events(sink);
    ex.run(&file, "t", vec![]).await.unwrap();
    session.shutdown().await;
}
