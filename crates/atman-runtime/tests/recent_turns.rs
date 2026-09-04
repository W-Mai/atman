use std::sync::Arc;

use atman_dsl::parse::parse_file;
use atman_runtime::memory::goal::GoalStore;
use atman_runtime::message::{Message, MessageOrigin, MessageRole};
use atman_runtime::{Executor, Session, Value};

#[tokio::test]
async fn recent_turns_returns_empty_before_any_message() {
    let tmp = tempfile::tempdir().unwrap();
    let session = std::sync::Arc::new(Session::open(tmp.path()).unwrap());
    let ex = Executor::new();
    atman_runtime::tools::register_tier_zero(&ex.tools);
    let todo = Arc::new(atman_runtime::memory::TodoStore::at(session.dir()));
    let conf = Arc::new(atman_runtime::memory::ConfessionStore::at(session.dir()));
    let goal = Arc::new(GoalStore::at(session.dir()));
    let plan = Arc::new(atman_runtime::memory::PlanStore::at(session.dir()));
    atman_runtime::tools::register_memory(&ex.tools, todo, conf, goal, plan);

    let src = r#"flow t() -> int {
    result = memory.recent_turns(n: 5)
    return len(result.items)
}
"#;
    let file = parse_file(src).unwrap();
    let user_msg = Message::user_text(atman_runtime::event::TurnId::now(), "run");
    let turn_id = session.begin_turn(user_msg);
    let out = ex
        .run_in_turn(&file, "t", vec![], None, Some(session.clone()))
        .await
        .unwrap();
    session.end_turn(&turn_id);

    match out {
        Value::Int(n) => assert!(n <= 1, "want zero or the just-emitted user msg, got {n}"),
        other => panic!("want int, got {other:?}"),
    }
}

#[tokio::test]
async fn recent_turns_picks_up_appended_messages() {
    let tmp = tempfile::tempdir().unwrap();
    let session = std::sync::Arc::new(Session::open(tmp.path()).unwrap());

    for role in ["hi", "world"] {
        let m = Message::user_text(atman_runtime::event::TurnId::now(), role);
        session.append_message(m, None);
    }

    let ex = Executor::new();
    atman_runtime::tools::register_tier_zero(&ex.tools);
    let todo = Arc::new(atman_runtime::memory::TodoStore::at(session.dir()));
    let conf = Arc::new(atman_runtime::memory::ConfessionStore::at(session.dir()));
    let goal = Arc::new(GoalStore::at(session.dir()));
    let plan = Arc::new(atman_runtime::memory::PlanStore::at(session.dir()));
    atman_runtime::tools::register_memory(&ex.tools, todo, conf, goal, plan);

    let src = r#"flow t() -> int {
    result = memory.recent_turns(n: 5)
    return len(result.items)
}
"#;
    let file = parse_file(src).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    let user_msg = Message::user_text(atman_runtime::event::TurnId::now(), "run");
    let turn_id = session.begin_turn(user_msg);
    let out = ex
        .run_in_turn(&file, "t", vec![], None, Some(session.clone()))
        .await
        .unwrap();
    session.end_turn(&turn_id);

    match out {
        Value::Int(n) => assert!(n >= 2, "want at least the 2 appended msgs, got {n}"),
        other => panic!("want int, got {other:?}"),
    }
}

#[tokio::test]
async fn recent_turns_caps_output_at_n() {
    let tmp = tempfile::tempdir().unwrap();
    let session = std::sync::Arc::new(Session::open(tmp.path()).unwrap());

    for i in 0..8 {
        let m = Message {
            role: MessageRole::User,
            parts: vec![atman_runtime::message::MessagePart::Text {
                text: format!("msg{i}"),
            }],
            turn_id: atman_runtime::event::TurnId::now(),
            origin: MessageOrigin::User,
        };
        session.append_message(m, None);
    }

    let ex = Executor::new();
    atman_runtime::tools::register_tier_zero(&ex.tools);
    let todo = Arc::new(atman_runtime::memory::TodoStore::at(session.dir()));
    let conf = Arc::new(atman_runtime::memory::ConfessionStore::at(session.dir()));
    let goal = Arc::new(GoalStore::at(session.dir()));
    let plan = Arc::new(atman_runtime::memory::PlanStore::at(session.dir()));
    atman_runtime::tools::register_memory(&ex.tools, todo, conf, goal, plan);

    let src = r#"flow t() -> int {
    result = memory.recent_turns(n: 3)
    return len(result.items)
}
"#;
    let file = parse_file(src).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    let user_msg = Message::user_text(atman_runtime::event::TurnId::now(), "run");
    let turn_id = session.begin_turn(user_msg);
    let out = ex
        .run_in_turn(&file, "t", vec![], None, Some(session.clone()))
        .await
        .unwrap();
    session.end_turn(&turn_id);

    match out {
        Value::Int(n) => assert!(
            (2..=3).contains(&n),
            "want cap of 3 (or 2 if the just-begun turn isn't flushed yet), got {n}"
        ),
        other => panic!("want int, got {other:?}"),
    }
}

#[tokio::test]
async fn recent_turns_reads_lossless_owner_history_after_checkpoint() {
    let ex = Executor::new();
    ex.tools
        .register(Arc::new(atman_runtime::tools::memory::MemoryRecentTurns));
    let session = Arc::new(Session::open_ephemeral());
    session.append_message(
        Message::assistant_text(atman_runtime::event::TurnId::now(), "x".repeat(10_000)),
        None,
    );
    let ctx = atman_runtime::ToolCtx::new().with_session_runtime(session.clone());
    session.append_message(
        Message::user_text(atman_runtime::event::TurnId::now(), "latest-marker"),
        None,
    );
    let original = session.messages();
    let mut replacement = original.to_vec();
    replacement[0] = Message::assistant_text(original[0].turn_id.clone(), "short");
    session
        .commit_rewritten_window(
            session.context(),
            replacement,
            atman_runtime::compaction::estimate_tokens_for_messages(&original),
            &original,
            1,
        )
        .unwrap();
    let args = atman_runtime::ToolArgs {
        positional: Vec::new(),
        named: vec![
            ("n".into(), Value::Int(5)),
            ("excerpt_chars".into(), Value::Int(128)),
        ],
    };

    let result = ex
        .tools
        .get("memory.recent_turns")
        .unwrap()
        .call(args, &ctx)
        .await
        .unwrap();
    let Value::Struct(fields) = result else {
        panic!("expected structured recent-turn result");
    };
    let excerpt = fields
        .iter()
        .find_map(|(name, value)| (name == "excerpt").then_some(value))
        .and_then(|value| match value {
            Value::Str(text) => Some(text),
            _ => None,
        })
        .unwrap();
    let items = fields
        .iter()
        .find_map(|(name, value)| (name == "items").then_some(value))
        .unwrap();
    assert!(excerpt.chars().count() <= 128);
    assert!(excerpt.contains("latest-marker"));
    let Value::List(items) = items else {
        panic!("expected lossless items");
    };
    assert_eq!(items.len(), 2);
    assert!(
        matches!(&items[0], Value::Message(message) if message.text_concat() == "x".repeat(10_000))
    );
    assert_eq!(session.messages()[0].text_concat(), "short");
}
