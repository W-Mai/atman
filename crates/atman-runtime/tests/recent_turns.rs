use std::sync::Arc;

use atman_dsl::parse::parse_file;
use atman_runtime::memory::goal::GoalStore;
use atman_runtime::message::{Message, MessageRole};
use atman_runtime::{Executor, Session, Value};

#[tokio::test]
async fn recent_turns_returns_empty_before_any_message() {
    let tmp = tempfile::tempdir().unwrap();
    let session = Session::open(tmp.path()).unwrap();
    let mut ex = Executor::new();
    atman_runtime::tools::register_tier_zero(&mut ex.tools);
    let todo = Arc::new(atman_runtime::memory::TodoStore::at(session.dir()));
    let conf = Arc::new(atman_runtime::memory::ConfessionStore::at(session.dir()));
    let goal = Arc::new(GoalStore::at(session.dir()));
    atman_runtime::tools::register_memory(&mut ex.tools, todo, conf, goal);

    let src = r#"flow t() -> int {
    xs = memory.recent_turns(n: 5)
    return len(xs)
}
"#;
    let file = parse_file(src).unwrap();
    let user_msg = Message::user_text(atman_runtime::event::TurnId::now(), "run");
    session.begin_turn(user_msg);
    let out = ex
        .run_in_turn(&file, "t", vec![], None, Some(&session))
        .await
        .unwrap();
    session.end_turn();

    match out {
        Value::Int(n) => assert!(n <= 1, "want zero or the just-emitted user msg, got {n}"),
        other => panic!("want int, got {other:?}"),
    }
}

#[tokio::test]
async fn recent_turns_picks_up_appended_messages() {
    let tmp = tempfile::tempdir().unwrap();
    let session = Session::open(tmp.path()).unwrap();

    for role in ["hi", "world"] {
        let m = Message::user_text(atman_runtime::event::TurnId::now(), role);
        session.append_message(m, None);
    }

    let mut ex = Executor::new();
    atman_runtime::tools::register_tier_zero(&mut ex.tools);
    let todo = Arc::new(atman_runtime::memory::TodoStore::at(session.dir()));
    let conf = Arc::new(atman_runtime::memory::ConfessionStore::at(session.dir()));
    let goal = Arc::new(GoalStore::at(session.dir()));
    atman_runtime::tools::register_memory(&mut ex.tools, todo, conf, goal);

    let src = r#"flow t() -> int {
    xs = memory.recent_turns(n: 5)
    return len(xs)
}
"#;
    let file = parse_file(src).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    let user_msg = Message::user_text(atman_runtime::event::TurnId::now(), "run");
    session.begin_turn(user_msg);
    let out = ex
        .run_in_turn(&file, "t", vec![], None, Some(&session))
        .await
        .unwrap();
    session.end_turn();

    match out {
        Value::Int(n) => assert!(n >= 2, "want at least the 2 appended msgs, got {n}"),
        other => panic!("want int, got {other:?}"),
    }
}

#[tokio::test]
async fn recent_turns_caps_output_at_n() {
    let tmp = tempfile::tempdir().unwrap();
    let session = Session::open(tmp.path()).unwrap();

    for i in 0..8 {
        let m = Message {
            role: MessageRole::User,
            parts: vec![atman_runtime::message::MessagePart::Text {
                text: format!("msg{i}"),
            }],
            turn_id: atman_runtime::event::TurnId::now(),
        };
        session.append_message(m, None);
    }

    let mut ex = Executor::new();
    atman_runtime::tools::register_tier_zero(&mut ex.tools);
    let todo = Arc::new(atman_runtime::memory::TodoStore::at(session.dir()));
    let conf = Arc::new(atman_runtime::memory::ConfessionStore::at(session.dir()));
    let goal = Arc::new(GoalStore::at(session.dir()));
    atman_runtime::tools::register_memory(&mut ex.tools, todo, conf, goal);

    let src = r#"flow t() -> int {
    xs = memory.recent_turns(n: 3)
    return len(xs)
}
"#;
    let file = parse_file(src).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    let user_msg = Message::user_text(atman_runtime::event::TurnId::now(), "run");
    session.begin_turn(user_msg);
    let out = ex
        .run_in_turn(&file, "t", vec![], None, Some(&session))
        .await
        .unwrap();
    session.end_turn();

    match out {
        Value::Int(n) => assert!(
            (2..=3).contains(&n),
            "want cap of 3 (or 2 if the just-begun turn isn't flushed yet), got {n}"
        ),
        other => panic!("want int, got {other:?}"),
    }
}
