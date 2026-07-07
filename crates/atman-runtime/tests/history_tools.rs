use atman_runtime::Session;
use atman_runtime::event::TurnId;
use atman_runtime::message::Message;
use atman_runtime::tool::{Tool, ToolArgs, ToolCtx};
use atman_runtime::tools::memory::{MemoryHistoryRead, MemoryHistorySearch};
use atman_runtime::value::Value;

async fn build_session_with_messages(tmp: &tempfile::TempDir) -> Session {
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
    session
}

#[tokio::test]
async fn history_search_returns_matching_events_in_current_session() {
    let tmp = tempfile::tempdir().unwrap();
    let session = build_session_with_messages(&tmp).await;
    let ctx = ToolCtx::new().with_session_dir(session.dir().to_path_buf());
    let args = ToolArgs {
        positional: Vec::new(),
        named: vec![("query".into(), Value::Str("compaction".into()))],
    };
    let result = MemoryHistorySearch.call(args, &ctx).await.unwrap();
    let items = match result {
        Value::List(v) => v,
        other => panic!("expected list, got {other:?}"),
    };
    assert!(!items.is_empty(), "expected at least one hit");
}

#[tokio::test]
async fn history_search_empty_query_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let session = Session::open(tmp.path()).unwrap();
    let ctx = ToolCtx::new().with_session_dir(session.dir().to_path_buf());
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
    let ctx = ToolCtx::new().with_session_dir(session.dir().to_path_buf());
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
    let header = fields
        .iter()
        .find(|(k, _)| k == "header")
        .and_then(|(_, v)| match v {
            Value::Str(s) => Some(s.clone()),
            _ => None,
        })
        .unwrap();
    assert!(header.contains("turns 0-2 of 4"), "header was {header}");
    let turns = fields
        .iter()
        .find(|(k, _)| k == "turns")
        .and_then(|(_, v)| match v {
            Value::List(l) => Some(l.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(turns.len(), 2);
}

#[tokio::test]
async fn history_read_role_filter_returns_only_matching_role() {
    let tmp = tempfile::tempdir().unwrap();
    let session = build_session_with_messages(&tmp).await;
    let ctx = ToolCtx::new().with_session_dir(session.dir().to_path_buf());
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
    let turns = fields
        .iter()
        .find(|(k, _)| k == "turns")
        .and_then(|(_, v)| match v {
            Value::List(l) => Some(l.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(turns.len(), 2, "should have 2 assistant messages");
    for t in turns {
        if let Value::Message(m) = t {
            assert_eq!(m.role.as_str(), "assistant");
        } else {
            panic!("expected message");
        }
    }
}
