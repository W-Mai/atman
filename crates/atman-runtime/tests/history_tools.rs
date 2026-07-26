use atman_runtime::Session;
use atman_runtime::event::TurnId;
use atman_runtime::history_store::{HistoryStore, HistoryStoreImpl};
use atman_runtime::message::Message;
use atman_runtime::tool::{Tool, ToolArgs, ToolCtx};
use atman_runtime::tools::memory::{MemoryHistoryCount, MemoryHistoryRead, MemoryHistorySearch};
use atman_runtime::value::Value;
use std::sync::Arc;

async fn build_session_with_messages(tmp: &tempfile::TempDir) -> Arc<Session> {
    let session = Session::open(tmp.path()).unwrap();
    let msgs = [
        Message::user_text(TurnId::now(), "let's plan the compaction fix"),
        Message::assistant_text(TurnId::now(), "compaction currently writes a placeholder"),
        Message::user_text(TurnId::now(), "we need real LLM summary"),
        Message::assistant_text(TurnId::now(), "ok will use session model"),
    ];
    for m in msgs {
        session.append_message(m, None);
    }
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    Arc::new(session)
}

fn store_from_session(session: &Arc<Session>) -> Arc<dyn HistoryStore> {
    Arc::new(HistoryStoreImpl::new(
        session.project_index(),
        Some(session.clone()),
        Some(session.id().to_string()),
        Some(
            session
                .dir()
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_default(),
        ),
    ))
}

fn ctx_from_session(session: &Arc<Session>) -> ToolCtx {
    let mut ctx = ToolCtx::new().with_session_dir(session.dir().to_path_buf());
    if let Some(idx) = session.project_index() {
        ctx = ctx.with_project_index(idx);
    }
    let store: Arc<dyn HistoryStore> = store_from_session(session);
    ctx = ctx.with_history_store(store);
    ctx
}

#[tokio::test]
async fn history_search_returns_matching_events_in_current_session() {
    let tmp = tempfile::tempdir().unwrap();
    let session = build_session_with_messages(&tmp).await;
    let ctx = ctx_from_session(&session);
    let args = ToolArgs {
        positional: Vec::new(),
        named: vec![("query".into(), Value::Str("compaction".into()))],
    };
    let result = MemoryHistorySearch.call(args, &ctx).await.unwrap();
    let fields = match result {
        Value::Struct(f) => f,
        other => panic!("expected struct, got {other:?}"),
    };
    let total = fields
        .iter()
        .find(|(k, _)| k == "total")
        .and_then(|(_, v)| match v {
            Value::Int(n) => Some(*n),
            _ => None,
        })
        .unwrap();
    assert!(total >= 1, "expected at least one hit, got {total}");
    let hits = fields
        .iter()
        .find(|(k, _)| k == "hits")
        .and_then(|(_, v)| match v {
            Value::List(l) => Some(l.clone()),
            _ => None,
        })
        .unwrap();
    assert!(!hits.is_empty(), "expected at least one hit");
}

#[tokio::test]
async fn history_search_empty_query_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let session = Arc::new(Session::open(tmp.path()).unwrap());
    let ctx = ctx_from_session(&session);
    let args = ToolArgs {
        positional: Vec::new(),
        named: vec![("query".into(), Value::Str("   ".into()))],
    };
    let err = MemoryHistorySearch.call(args, &ctx).await.unwrap_err();
    assert!(format!("{err}").contains("empty query"));
}

#[tokio::test]
async fn history_read_paginates_by_turn_index() {
    let tmp = tempfile::tempdir().unwrap();
    let session = build_session_with_messages(&tmp).await;
    let ctx = ctx_from_session(&session);
    let args = ToolArgs {
        positional: Vec::new(),
        named: vec![
            ("offset".into(), Value::Int(1)),
            ("limit".into(), Value::Int(2)),
        ],
    };
    let out = MemoryHistoryRead.call(args, &ctx).await.unwrap();
    let fields = match out {
        Value::Struct(f) => f,
        other => panic!("expected struct, got {other:?}"),
    };
    let total = fields
        .iter()
        .find(|(k, _)| k == "total")
        .and_then(|(_, v)| match v {
            Value::Int(n) => Some(*n),
            _ => None,
        })
        .unwrap();
    assert_eq!(total, 4);
    let items = fields
        .iter()
        .find(|(k, _)| k == "items")
        .and_then(|(_, v)| match v {
            Value::List(l) => Some(l.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(items.len(), 2);
}

#[tokio::test]
async fn history_read_role_filter_returns_only_matching_role() {
    let tmp = tempfile::tempdir().unwrap();
    let session = build_session_with_messages(&tmp).await;
    let ctx = ctx_from_session(&session);
    let args = ToolArgs {
        positional: Vec::new(),
        named: vec![
            ("offset".into(), Value::Int(1)),
            ("limit".into(), Value::Int(10)),
            ("role_filter".into(), Value::Str("assistant".into())),
        ],
    };
    let out = MemoryHistoryRead.call(args, &ctx).await.unwrap();
    let fields = match out {
        Value::Struct(f) => f,
        other => panic!("expected struct, got {other:?}"),
    };
    let total = fields
        .iter()
        .find(|(k, _)| k == "total")
        .and_then(|(_, v)| match v {
            Value::Int(n) => Some(*n),
            _ => None,
        })
        .unwrap();
    assert_eq!(total, 2, "should count 2 assistant messages");
    let items = fields
        .iter()
        .find(|(k, _)| k == "items")
        .and_then(|(_, v)| match v {
            Value::List(l) => Some(l.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(items.len(), 2, "should have 2 assistant messages");
    for t in items {
        if let Value::Message(m) = t {
            assert_eq!(m.role.as_str(), "assistant");
        } else {
            panic!("expected message");
        }
    }
}

#[tokio::test]
async fn history_count_returns_total() {
    let tmp = tempfile::tempdir().unwrap();
    let session = build_session_with_messages(&tmp).await;
    let ctx = ctx_from_session(&session);
    let args = ToolArgs {
        positional: Vec::new(),
        named: vec![],
    };
    let out = MemoryHistoryCount.call(args, &ctx).await.unwrap();
    match out {
        Value::Int(n) => assert_eq!(n, 4, "should have 4 messages"),
        other => panic!("expected int, got {other:?}"),
    }
}

#[tokio::test]
async fn history_count_with_role_filter() {
    let tmp = tempfile::tempdir().unwrap();
    let session = build_session_with_messages(&tmp).await;
    let ctx = ctx_from_session(&session);
    let args = ToolArgs {
        positional: Vec::new(),
        named: vec![("role_filter".into(), Value::Str("user".into()))],
    };
    let out = MemoryHistoryCount.call(args, &ctx).await.unwrap();
    match out {
        Value::Int(n) => assert_eq!(n, 2, "should have 2 user messages"),
        other => panic!("expected int, got {other:?}"),
    }
}

#[tokio::test]
async fn recent_turns_returns_total_and_items() {
    let tmp = tempfile::tempdir().unwrap();
    let session = build_session_with_messages(&tmp).await;
    let ctx = ctx_from_session(&session);
    let args = ToolArgs {
        positional: Vec::new(),
        named: vec![("n".into(), Value::Int(2))],
    };
    let out = atman_runtime::tools::memory::MemoryRecentTurns
        .call(args, &ctx)
        .await
        .unwrap();
    let fields = match out {
        Value::Struct(f) => f,
        other => panic!("expected struct, got {other:?}"),
    };
    let total = fields
        .iter()
        .find(|(k, _)| k == "total_message_count")
        .and_then(|(_, v)| match v {
            Value::Int(n) => Some(*n),
            _ => None,
        })
        .unwrap();
    assert_eq!(total, 4, "should report 4 total messages");
    let items = fields
        .iter()
        .find(|(k, _)| k == "items")
        .and_then(|(_, v)| match v {
            Value::List(l) => Some(l.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(items.len(), 2, "should return last 2 messages");
}
