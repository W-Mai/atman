use atman_runtime::AtmanPayload;
type Value = atman_rt::Value<atman_runtime::AtmanPayload, atman_runtime::RuntimeError>;
use atman_runtime::tool::{ToolArgs, ToolCtx};
use atman_runtime::tools::register_tier_zero;
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

    let registry = atman_runtime::tool::ToolRegistry::new();
    register_tier_zero(&registry);

    let tracker = Arc::new(Mutex::new(std::collections::HashSet::new()));
    let ctx = ToolCtx::new().with_read_files(tracker.clone());

    let read_tool = registry.get("fs.read").expect("fs.read registered");
    let edit_tool = registry.get("fs.edit").expect("fs.edit registered");

    let edit_before_read = edit_tool
        .call(
            ToolArgs {
                positional: vec![],
                named: vec![
                    ("path".into(), Value::Host(AtmanPayload::Path(path.clone()))),
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
                positional: vec![Value::Host(AtmanPayload::Path(path.clone()))],
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
                    ("path".into(), Value::Host(AtmanPayload::Path(path.clone()))),
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

    let registry = atman_runtime::tool::ToolRegistry::new();
    register_tier_zero(&registry);

    let tracker = Arc::new(Mutex::new(std::collections::HashSet::new()));
    let ctx = ToolCtx::new().with_read_files(tracker);
    registry
        .get("fs.read")
        .unwrap()
        .call(
            ToolArgs {
                positional: vec![Value::Host(AtmanPayload::Path(path.clone()))],
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
                    ("path".into(), Value::Host(AtmanPayload::Path(path))),
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

#[tokio::test]
async fn fs_edit_preview_and_write_select_the_same_literal_across_line_endings() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("repeat.txt");
    let before = "只读\rsame\r\n  next\r\n分隔\nsame\n  next\n分隔\nsame\r\n  next";
    tokio::fs::write(&path, before).await.unwrap();

    let registry = atman_runtime::tool::ToolRegistry::new();
    register_tier_zero(&registry);
    let tracker = Arc::new(Mutex::new(std::collections::HashSet::new()));
    let ctx = ToolCtx::new().with_read_files(tracker);
    registry
        .get("fs.read")
        .unwrap()
        .call(
            ToolArgs {
                positional: vec![Value::Host(AtmanPayload::Path(path.clone()))],
                named: vec![],
            },
            &ctx,
        )
        .await
        .unwrap();
    let edit = registry.get("fs.edit").unwrap();
    let args = |selection| {
        let mut named = vec![
            ("path".into(), Value::Host(AtmanPayload::Path(path.clone()))),
            ("old_string".into(), Value::Str("same\n  next".into())),
            (
                "new_string".into(),
                Value::Str("changed\n  中\n  end".into()),
            ),
        ];
        named.extend(selection);
        ToolArgs {
            positional: vec![],
            named,
        }
    };
    let ambiguous = args(None);
    assert!(edit.preview_call(&ambiguous, &ctx).await.is_none());
    let error = edit.call(ambiguous, &ctx).await.unwrap_err().to_string();
    assert!(error.contains("line 1, column 4"), "{error}");
    assert!(error.contains("line 4, column 1"), "{error}");
    assert!(error.contains("start_line"), "{error}");
    assert_eq!(tokio::fs::read(&path).await.unwrap(), before.as_bytes());

    let selected = args(Some(("start_line".into(), Value::Int(4))));
    let preview = edit.preview_call(&selected, &ctx).await.unwrap();
    let Value::Struct(fields) = edit.call(selected, &ctx).await.unwrap() else {
        panic!("expected a structured edit receipt");
    };
    assert!(fields.iter().any(|(name, value)| {
        name == "diff" && matches!(value, Value::Str(diff) if diff == &preview)
    }));
    assert_eq!(
        tokio::fs::read(&path).await.unwrap(),
        "只读\rsame\r\n  next\r\n分隔\nchanged\n  中\n  end\n分隔\nsame\r\n  next".as_bytes(),
    );

    tokio::fs::write(&path, before).await.unwrap();
    edit.call(args(Some(("replace_all".into(), Value::Bool(true)))), &ctx)
        .await
        .unwrap();
    assert_eq!(
        tokio::fs::read(&path).await.unwrap(),
        "只读\rchanged\r\n  中\r\n  end\r\n分隔\nchanged\n  中\n  end\n分隔\nchanged\r\n  中\r\n  end".as_bytes(),
    );
}
