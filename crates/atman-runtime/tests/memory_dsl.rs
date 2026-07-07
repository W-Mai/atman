use std::sync::Arc;

use atman_dsl::parse::parse_file;
use atman_runtime::memory::confession::ConfessionStore;
use atman_runtime::memory::todo::TodoStore;
use atman_runtime::{Executor, Value, tools};
use tempfile::TempDir;

#[tokio::test]
async fn memory_confess_persists_via_dsl_call() {
    let dir = TempDir::new().unwrap();
    let store = Arc::new(ConfessionStore::at(dir.path()));

    let src = r#"flow t() -> Unit {
    memory.confess(
        trigger: "wrote as any again",
        rule_violated: "no-as-any",
        what_i_did: "cast Value::Int as any",
        why: "was tired",
        mitigation: "run cargo check on every edit"
    )
    return 0
}
"#;
    let file = parse_file(src).unwrap();
    let mut ex = Executor::new();
    tools::register_memory(
        &mut ex.tools,
        Arc::new(TodoStore::at(dir.path())),
        store.clone(),
        Arc::new(atman_runtime::memory::GoalStore::at(dir.path())),
        Arc::new(atman_runtime::memory::PlanStore::at(dir.path())),
    );
    ex.run(&file, "t", vec![]).await.unwrap();

    let items = store.list().await.unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].trigger, "wrote as any again");
    assert_eq!(items[0].rule_violated, "no-as-any");
}

#[tokio::test]
async fn memory_todo_set_and_done_via_dsl() {
    let dir = TempDir::new().unwrap();
    let todo_store = Arc::new(TodoStore::at(dir.path()));
    let confession_store = Arc::new(ConfessionStore::at(dir.path()));

    let src = r#"flow t() -> string {
    id = memory.todo.set(
        where: "src/main.rs",
        why: "need helper",
        how: "add validate()",
        expected_result: "test passes"
    )
    return id
}
"#;
    let file = parse_file(src).unwrap();
    let mut ex = Executor::new();
    tools::register_memory(
        &mut ex.tools,
        todo_store.clone(),
        confession_store,
        Arc::new(atman_runtime::memory::GoalStore::at(dir.path())),
        Arc::new(atman_runtime::memory::PlanStore::at(dir.path())),
    );
    let id = ex.run(&file, "t", vec![]).await.unwrap();
    let id_str = match id {
        Value::Str(s) => s,
        other => panic!("expected string id, got {other:?}"),
    };

    let items = todo_store.list().await.unwrap();
    assert_eq!(items.len(), 1);
    assert!(matches!(
        items[0].status,
        atman_runtime::memory::todo::TodoStatus::Pending
    ));
    assert_eq!(items[0].id.to_string(), id_str);
}

#[tokio::test]
async fn memory_fetch_confessions_returns_registered_entries() {
    let dir = TempDir::new().unwrap();
    let store = Arc::new(ConfessionStore::at(dir.path()));
    store
        .append(atman_runtime::memory::confession::Confession {
            id: atman_runtime::memory::MemoryId::now(),
            trigger: "comment discipline".into(),
            rule_violated: "no-narrative-comments".into(),
            what_i_did: "wrote a comment".into(),
            why: "was thinking".into(),
            mitigation: "delete it".into(),
            anchors: vec![],
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();

    let src = r#"flow t() -> Confessions {
    return memory.fetch_confessions()
}
"#;
    let file = parse_file(src).unwrap();
    let mut ex = Executor::new();
    tools::register_memory(
        &mut ex.tools,
        Arc::new(TodoStore::at(dir.path())),
        store,
        Arc::new(atman_runtime::memory::GoalStore::at(dir.path())),
        Arc::new(atman_runtime::memory::PlanStore::at(dir.path())),
    );
    let out = ex.run(&file, "t", vec![]).await.unwrap();
    match out {
        Value::List(items) => {
            assert_eq!(items.len(), 1);
        }
        other => panic!("expected list, got {other:?}"),
    }
}
