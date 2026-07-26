use atman_runtime::Session;
use atman_runtime::event::TurnId;
use atman_runtime::message::{Message, MessagePart, MessageRole};
use atman_runtime::tools::tool_output::MAX_TOOL_RESULT_CHARS;

#[tokio::test]
async fn append_tool_result_truncates_and_spills_to_file() {
    let tmp = tempfile::tempdir().unwrap();
    let session = Session::open(tmp.path()).unwrap();
    let sid = session.id().to_string();

    let big_content = "X".repeat(MAX_TOOL_RESULT_CHARS + 5000);
    let msg = Message {
        role: MessageRole::Tool,
        parts: vec![MessagePart::ToolResult {
            tool_use_id: "tu_overflow".into(),
            content: big_content,
            is_error: false,
        }],
        turn_id: TurnId::now(),
    };
    session.append_message(msg, None);
    session.shutdown().await;

    let reopened = Session::open_existing(tmp.path(), &sid).unwrap();
    let msgs = reopened.messages();
    assert_eq!(msgs.len(), 1, "one tool message");
    if let MessagePart::ToolResult { content, .. } = &msgs[0].parts[0] {
        assert!(
            content.len() < MAX_TOOL_RESULT_CHARS + 5000,
            "content should be truncated, got {} chars",
            content.len()
        );
        assert!(content.contains("[Output truncated at"));
        assert!(content.contains("spilled to:"));
        assert!(content.contains("fs.read"));
    } else {
        panic!("expected ToolResult part");
    }

    let session_dir = tmp.path().join("sessions").join(&sid);
    let spill_dir = session_dir.join("tool_outputs");
    let entries: Vec<_> = std::fs::read_dir(&spill_dir)
        .expect("tool_outputs dir should exist")
        .filter_map(|e| e.ok())
        .collect();
    assert!(
        entries.iter().any(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("tool_output_tu_overflow")
        }),
        "spill file should exist, found: {:?}",
        entries.iter().map(|e| e.file_name()).collect::<Vec<_>>()
    );

    reopened.shutdown().await;
}

#[tokio::test]
async fn small_tool_result_not_truncated() {
    let tmp = tempfile::tempdir().unwrap();
    let session = Session::open(tmp.path()).unwrap();
    let sid = session.id().to_string();

    let content = "small result".to_string();
    let msg = Message {
        role: MessageRole::Tool,
        parts: vec![MessagePart::ToolResult {
            tool_use_id: "tu_small".into(),
            content: content.clone(),
            is_error: false,
        }],
        turn_id: TurnId::now(),
    };
    session.append_message(msg, None);
    session.shutdown().await;

    let reopened = Session::open_existing(tmp.path(), &sid).unwrap();
    let msgs = reopened.messages();
    assert_eq!(msgs.len(), 1);
    if let MessagePart::ToolResult { content: c, .. } = &msgs[0].parts[0] {
        assert_eq!(c, &content, "small result should be unchanged");
        assert!(!c.contains("[Output truncated"));
    } else {
        panic!("expected ToolResult part");
    }

    let session_dir = tmp.path().join("sessions").join(&sid);
    let spill_dir = session_dir.join("tool_outputs");
    assert!(!spill_dir.exists());

    reopened.shutdown().await;
}

#[tokio::test]
async fn spill_file_contains_full_output() {
    let tmp = tempfile::tempdir().unwrap();
    let session = Session::open(tmp.path()).unwrap();
    let sid = session.id().to_string();

    let big_content = "Y".repeat(MAX_TOOL_RESULT_CHARS + 1000);
    let msg = Message {
        role: MessageRole::Tool,
        parts: vec![MessagePart::ToolResult {
            tool_use_id: "tu_spill_check".into(),
            content: big_content.clone(),
            is_error: false,
        }],
        turn_id: TurnId::now(),
    };
    session.append_message(msg, None);
    session.shutdown().await;

    let session_dir = tmp.path().join("sessions").join(&sid);
    let spill_path = session_dir
        .join("tool_outputs")
        .join("tool_output_tu_spill_check.txt");
    let spilled = std::fs::read_to_string(&spill_path).expect("spill file should exist");
    assert_eq!(spilled.len(), big_content.len());
    assert_eq!(spilled, big_content);
}
