use atman_runtime::Session;
use atman_runtime::event::TurnId;
use atman_runtime::message::{Message, MessageOrigin, MessagePart, MessageRole};
use atman_runtime::tools::tool_output::{MAX_TOOL_RESULT_CHARS, ToolOutputBudget};

fn output_id(notice: &str) -> String {
    notice
        .split_once("output_id=")
        .unwrap()
        .1
        .split([',', '.'])
        .next()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn append_tool_result_returns_opaque_output_id() {
    let tmp = tempfile::tempdir().unwrap();
    let session = Session::open(tmp.path()).unwrap();
    let sid = session.id().to_string();
    let full = "X".repeat(MAX_TOOL_RESULT_CHARS + 5000);
    session.append_message(
        Message {
            role: MessageRole::Tool,
            parts: vec![MessagePart::ToolResult {
                tool_use_id: "tu_overflow".into(),
                content: full,
                is_error: false,
            }],
            turn_id: TurnId::now(),
            origin: MessageOrigin::User,
        },
        None,
    );
    session.shutdown().await;

    let reopened = Session::open_existing(tmp.path(), &sid).unwrap();
    let messages = reopened.messages();
    let MessagePart::ToolResult { content, .. } = &messages[0].parts[0] else {
        panic!()
    };
    assert!(content.contains("output_id="));
    assert!(content.contains("output.read"));
    assert!(!content.contains("fs.read"));
    assert!(!content.contains(reopened.dir().to_string_lossy().as_ref()));
    reopened.shutdown().await;
}

#[tokio::test]
async fn small_tool_result_not_truncated() {
    let tmp = tempfile::tempdir().unwrap();
    let session = Session::open(tmp.path()).unwrap();
    let sid = session.id().to_string();
    let content = "small result".to_string();
    session.append_message(
        Message {
            role: MessageRole::Tool,
            parts: vec![MessagePart::ToolResult {
                tool_use_id: "tu_small".into(),
                content: content.clone(),
                is_error: false,
            }],
            turn_id: TurnId::now(),
            origin: MessageOrigin::User,
        },
        None,
    );
    session.shutdown().await;

    let reopened = Session::open_existing(tmp.path(), &sid).unwrap();
    let messages = reopened.messages();
    let MessagePart::ToolResult {
        content: actual, ..
    } = &messages[0].parts[0]
    else {
        panic!()
    };
    assert_eq!(actual, &content);
    assert!(!reopened.dir().join("tool_outputs").exists());
    reopened.shutdown().await;
}

#[tokio::test]
async fn session_append_uses_the_configured_tool_output_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let session = Session::open(tmp.path()).unwrap();
    let budget = ToolOutputBudget {
        max_lines: 4,
        max_bytes: 64,
        max_line_bytes: 64,
    };
    session.set_tool_output_budget(budget);
    session.append_message(
        Message {
            role: MessageRole::Tool,
            parts: vec![MessagePart::ToolResult {
                tool_use_id: "tu_configured".into(),
                content: "X".repeat(256),
                is_error: false,
            }],
            turn_id: TurnId::now(),
            origin: MessageOrigin::User,
        },
        None,
    );

    let messages = session.messages();
    let MessagePart::ToolResult { content, .. } = &messages[0].parts[0] else {
        panic!()
    };
    let excerpt = content.split("\n\n[Output truncated:").next().unwrap();
    assert_eq!(excerpt.len(), budget.max_bytes);
    assert!(content.contains("output_id="));
    session.shutdown().await;
}

#[tokio::test]
async fn reopened_session_reads_full_output_by_byte_offset() {
    let tmp = tempfile::tempdir().unwrap();
    let session = Session::open(tmp.path()).unwrap();
    let sid = session.id().to_string();
    let full = "你好🚀".repeat(5_000);
    session.append_message(
        Message {
            role: MessageRole::Tool,
            parts: vec![MessagePart::ToolResult {
                tool_use_id: "tu_output".into(),
                content: full.clone(),
                is_error: false,
            }],
            turn_id: TurnId::now(),
            origin: MessageOrigin::User,
        },
        None,
    );
    session.shutdown().await;

    let reopened = Session::open_existing(tmp.path(), &sid).unwrap();
    let messages = reopened.messages();
    let MessagePart::ToolResult { content, .. } = &messages[0].parts[0] else {
        panic!()
    };
    let output_id = output_id(content);
    let store = reopened.output_store();
    let budget = ToolOutputBudget {
        max_lines: 100,
        max_bytes: 1024,
        max_line_bytes: 1024,
    };
    let mut offset = 0;
    let mut assembled = String::new();
    loop {
        let page = store.read_bytes(&output_id, offset, 1024, budget).unwrap();
        assembled.push_str(&page.content);
        if !page.has_more {
            break;
        }
        assert!(page.next_offset > offset);
        offset = page.next_offset;
    }
    assert_eq!(assembled, full);
    reopened.shutdown().await;
}
