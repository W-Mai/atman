use atman_runtime::Session;
use atman_runtime::compaction::{find_compact_range, is_plan_related};
use atman_runtime::event::TurnId;
use atman_runtime::memory::MemoryId;
use atman_runtime::memory::plan::{Plan, PlanStore};
use atman_runtime::memory::todo::{Todo, TodoStatus};
use atman_runtime::message::{Message, MessagePart, MessageRole};
use tempfile::TempDir;

#[tokio::test]
async fn plan_store_persists_across_open_existing() {
    let root = TempDir::new().unwrap();
    let sid = {
        let session = Session::open(root.path()).unwrap();
        let store = PlanStore::at(session.dir());
        store
            .upsert(Plan::new(
                "resume-plan",
                "resume-test",
                vec!["step0".into(), "step1".into()],
            ))
            .await
            .unwrap();
        session.id().to_string()
    };
    let resumed = Session::open_existing(root.path(), &sid).unwrap();
    let store = PlanStore::at(resumed.dir());
    let plan = store.latest().await.unwrap().unwrap();
    assert_eq!(plan.id, "resume-plan");
    assert_eq!(plan.steps.len(), 2);
}

#[test]
fn is_plan_related_matches_plan_tool_use_and_result() {
    let plan_use = Message {
        turn_id: TurnId::now(),
        role: MessageRole::Assistant,
        parts: vec![MessagePart::ToolUse {
            id: "t1".into(),
            name: "plan.write".into(),
            input: serde_json::json!({"title":"x"}),
        }],
    };
    assert!(is_plan_related(&plan_use));
    let plan_result = Message {
        turn_id: TurnId::now(),
        role: MessageRole::Tool,
        parts: vec![MessagePart::ToolResult {
            tool_use_id: "t1".into(),
            content: "# Plan: ship\n_id: p1_\n\n- [ ] a\n".into(),
            is_error: false,
        }],
    };
    assert!(is_plan_related(&plan_result));
    let ordinary = Message::user_text(TurnId::now(), "hello");
    assert!(!is_plan_related(&ordinary));
}

#[test]
fn find_compact_range_skips_plan_messages() {
    let big = "y".repeat(4000);
    let mut msgs: Vec<Message> = Vec::new();
    for i in 0..5 {
        msgs.push(Message::user_text(TurnId::now(), format!("{big} {i}")));
    }
    msgs.insert(
        3,
        Message {
            turn_id: TurnId::now(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::ToolUse {
                id: "t1".into(),
                name: "plan.tick".into(),
                input: serde_json::json!({}),
            }],
        },
    );
    for i in 0..5 {
        msgs.push(Message::user_text(TurnId::now(), format!("{big} tail {i}")));
    }
    let range = find_compact_range(&msgs, 100).expect("expected compaction target");
    let covers_plan = range.start <= 3 && range.end > 3;
    assert!(
        !covers_plan,
        "range {range:?} must not include the plan.tick message at index 3"
    );
}

#[tokio::test]
async fn todos_and_plans_share_session_dir() {
    let dir = TempDir::new().unwrap();
    let session = Session::open(dir.path()).unwrap();
    let todo_store = atman_runtime::memory::todo::TodoStore::at(session.dir());
    todo_store
        .add(Todo {
            id: MemoryId::now(),
            where_: "x".into(),
            why: "y".into(),
            how: "z".into(),
            expected_result: "ok".into(),
            status: TodoStatus::Pending,
        })
        .await
        .unwrap();
    let plan_store = PlanStore::at(session.dir());
    plan_store
        .upsert(Plan::new("p1", "coexist", vec!["a".into()]))
        .await
        .unwrap();
    assert_eq!(todo_store.list().await.unwrap().len(), 1);
    assert_eq!(plan_store.list().await.unwrap().len(), 1);
}
