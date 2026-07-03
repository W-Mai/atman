use atman_dsl::parse::parse_file;
use atman_runtime::message::{ImageData, MessagePart, MessageRole};
use atman_runtime::{Executor, Value};

#[tokio::test]
async fn user_msg_positional_text_produces_message_value() {
    let src = r#"flow build() -> Message {
    return user_msg("hello")
}"#;
    let file = parse_file(src).unwrap();
    let ex = Executor::new();
    let out = ex.run(&file, "build", vec![]).await.unwrap();
    let Value::Message(msg) = out else {
        panic!("expected Value::Message, got {out:?}");
    };
    assert_eq!(msg.role, MessageRole::User);
    assert_eq!(msg.parts.len(), 1);
    assert!(matches!(&msg.parts[0], MessagePart::Text { text } if text == "hello"));
}

#[tokio::test]
async fn user_msg_with_attachments_prepends_image_parts() {
    let src = r#"flow build() -> Message {
    return user_msg("describe", attachments: [@"a.png"])
}"#;
    let file = parse_file(src).unwrap();
    let ex = Executor::new();
    let out = ex.run(&file, "build", vec![]).await.unwrap();
    let Value::Message(msg) = out else {
        panic!("expected message");
    };
    assert_eq!(msg.parts.len(), 2);
    let MessagePart::Image { source } = &msg.parts[0] else {
        panic!("first part should be image, got {:?}", msg.parts[0]);
    };
    assert_eq!(source.media_type, "image/png");
    assert!(
        matches!(&source.data, ImageData::Path { path } if path.to_string_lossy().ends_with("a.png"))
    );
    assert!(matches!(&msg.parts[1], MessagePart::Text { text } if text == "describe"));
}

#[tokio::test]
async fn tool_result_takes_id_content_and_optional_is_error() {
    let src = r#"flow build() -> Message {
    return tool_result("toolu_x", "output", is_error: true)
}"#;
    let file = parse_file(src).unwrap();
    let ex = Executor::new();
    let out = ex.run(&file, "build", vec![]).await.unwrap();
    let Value::Message(msg) = out else {
        panic!("expected message");
    };
    assert_eq!(msg.role, MessageRole::Tool);
    let MessagePart::ToolResult {
        tool_use_id,
        content,
        is_error,
    } = &msg.parts[0]
    else {
        panic!("expected tool_result part");
    };
    assert_eq!(tool_use_id, "toolu_x");
    assert_eq!(content, "output");
    assert!(*is_error);
}

#[tokio::test]
async fn system_msg_text_only() {
    let src = r#"flow build() -> Message {
    return system_msg("you are a reviewer")
}"#;
    let file = parse_file(src).unwrap();
    let ex = Executor::new();
    let out = ex.run(&file, "build", vec![]).await.unwrap();
    let Value::Message(msg) = out else {
        panic!("expected message");
    };
    assert_eq!(msg.role, MessageRole::System);
    assert!(matches!(&msg.parts[0], MessagePart::Text { text } if text == "you are a reviewer"));
}
