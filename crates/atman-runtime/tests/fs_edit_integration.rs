use atman_runtime::tool::{ToolArgs, ToolCtx};
use atman_runtime::tools::register_tier_zero;
use atman_runtime::value::Value;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

#[tokio::test]
async fn fs_edit_end_to_end_read_then_edit_flow() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("script.py");
    tokio::fs::write(
        &path,
        b"def greet():\n    print(\"hi\")\n\ndef main():\n    greet()\n",
    )
    .await
    .unwrap();

    let mut registry = atman_runtime::tool::ToolRegistry::new();
    register_tier_zero(&mut registry);

    let tracker = Arc::new(Mutex::new(std::collections::HashSet::new()));
    let ctx = ToolCtx::new().with_read_files(tracker.clone());

    let read_tool = registry.get("fs.read").expect("fs.read registered");
    let edit_tool = registry.get("fs.edit").expect("fs.edit registered");

    let edit_before_read = edit_tool
        .call(
            ToolArgs {
                positional: vec![],
                named: vec![
                    ("path".into(), Value::Path(path.clone())),
                    ("old_string".into(), Value::Str("print(\"hi\")".into())),
                    ("new_string".into(), Value::Str("print(\"hello\")".into())),
                ],
            },
            &ctx,
        )
        .await;
    assert!(
        matches!(&edit_before_read, Err(e) if format!("{e}").contains("has not been read")),
        "expected read-before-edit gate, got {edit_before_read:?}"
    );

    read_tool
        .call(
            ToolArgs {
                positional: vec![Value::Path(path.clone())],
                named: vec![],
            },
            &ctx,
        )
        .await
        .expect("fs.read succeeds");

    let edit_result = edit_tool
        .call(
            ToolArgs {
                positional: vec![],
                named: vec![
                    ("path".into(), Value::Path(path.clone())),
                    ("old_string".into(), Value::Str("print(\"hi\")".into())),
                    (
                        "new_string".into(),
                        Value::Str("print(\"hello, world\")".into()),
                    ),
                ],
            },
            &ctx,
        )
        .await
        .expect("edit after read succeeds");
    assert!(matches!(edit_result, Value::Struct(_)));

    let after = tokio::fs::read_to_string(&path).await.unwrap();
    assert!(after.contains("print(\"hello, world\")"));
    assert!(!after.contains("print(\"hi\")"));
    assert!(after.contains("def greet"));
    assert!(after.contains("def main"));
}

#[tokio::test]
async fn fs_edit_ambiguous_match_returns_actionable_error_through_registry() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("todos.txt");
    tokio::fs::write(&path, b"TODO first\nnormal\nTODO second\n")
        .await
        .unwrap();

    let mut registry = atman_runtime::tool::ToolRegistry::new();
    register_tier_zero(&mut registry);

    let tracker = Arc::new(Mutex::new(std::collections::HashSet::new()));
    let ctx = ToolCtx::new().with_read_files(tracker);
    registry
        .get("fs.read")
        .unwrap()
        .call(
            ToolArgs {
                positional: vec![Value::Path(path.clone())],
                named: vec![],
            },
            &ctx,
        )
        .await
        .unwrap();

    let err = registry
        .get("fs.edit")
        .unwrap()
        .call(
            ToolArgs {
                positional: vec![],
                named: vec![
                    ("path".into(), Value::Path(path)),
                    ("old_string".into(), Value::Str("TODO".into())),
                    ("new_string".into(), Value::Str("DONE".into())),
                ],
            },
            &ctx,
        )
        .await
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("matches 2 times"), "msg: {msg}");
    assert!(msg.contains("replace_all=true"));
}
