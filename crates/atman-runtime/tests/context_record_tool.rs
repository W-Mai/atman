use std::sync::Arc;

use atman_dsl::parse::parse_file;
use atman_runtime::message::{MessagePart, MessageRole};
use atman_runtime::{Executor, Session, Value};

const FLOW: &str = r#"
flow remember(content: string) -> bool {
    return context.record(key: "agent.rule.review", content: content)
}

flow remember_mistake(item: Mistake) -> bool {
    return context.record(
        key: "agent.mistake." + item.id,
        content: to_json_string(item),
    )
}
"#;

#[tokio::test]
async fn registered_context_record_tool_appends_through_the_root_session() {
    let file = parse_file(FLOW).unwrap();
    let session = Arc::new(Session::open_ephemeral());
    let executor = Executor::with_events(session.sink().clone());

    for (content, appended) in [("first", true), ("first", false), ("second", true)] {
        let value = executor
            .run_in_turn(
                &file,
                "remember",
                vec![("content".into(), Value::Str(content.into()))],
                None,
                Some(Arc::clone(&session)),
            )
            .await
            .unwrap();
        assert!(matches!(value, Value::Bool(value) if value == appended));
    }

    let messages = session.messages();
    let records: Vec<_> = messages
        .iter()
        .flat_map(|message| &message.parts)
        .filter_map(|part| match part {
            MessagePart::ContextRecord(record) => Some(record),
            _ => None,
        })
        .collect();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].revision(), 1);
    assert_eq!(records[1].revision(), 2);
    assert!(messages.iter().all(|message| {
        !message
            .parts
            .iter()
            .any(|part| matches!(part, MessagePart::ContextRecord(_)))
            || message.role == MessageRole::System
    }));
}

#[tokio::test]
async fn confession_structs_keep_their_item_identity_and_content() {
    let file = parse_file(FLOW).unwrap();
    let session = Arc::new(Session::open_ephemeral());
    let executor = Executor::with_events(session.sink().clone());
    let item = Value::Struct(vec![
        ("id".into(), Value::Str("mistake-1".into())),
        ("trigger".into(), Value::Str("rushed edit".into())),
        ("mitigation".into(), Value::Str("read the diff".into())),
    ]);

    let value = executor
        .run_in_turn(
            &file,
            "remember_mistake",
            vec![("item".into(), item)],
            None,
            Some(Arc::clone(&session)),
        )
        .await
        .unwrap();
    assert!(matches!(value, Value::Bool(true)));

    let messages = session.messages();
    let record = messages
        .iter()
        .flat_map(|message| &message.parts)
        .find_map(|part| match part {
            MessagePart::ContextRecord(record) => Some(record),
            _ => None,
        })
        .unwrap();
    assert_eq!(record.key(), "agent.mistake.mistake-1");
    assert!(record.render_for_model().contains("read the diff"));
}
