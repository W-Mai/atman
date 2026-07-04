use atman_runtime::event::TurnId;
use atman_runtime::message::{Message, MessagePart, MessageRole};
use atman_runtime::provider::LlmRequest;
use atman_runtime::providers::anthropic::AnthropicProvider;
use atman_runtime::value::Value;
use uuid::Uuid;

fn provider() -> AnthropicProvider {
    AnthropicProvider::new("anthropic", "test-key").with_base_url("http://irrelevant".to_string())
}

fn request_with_tools() -> LlmRequest {
    LlmRequest {
        model: "claude-3-5-sonnet".to_string(),
        messages: vec![Message {
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                text: "list examples/".into(),
            }],
            turn_id: TurnId(Uuid::nil()),
        }],
        system: None,
        input: Value::Unit,
        schema: None,
        cache_prompt: false,
        tools: vec![
            atman_runtime::tool::ToolSpec {
                name: "fs.list".into(),
                description: Some("list a directory".into()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }),
            },
            atman_runtime::tool::ToolSpec {
                name: "bash.exec".into(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
            },
        ],
    }
}

#[test]
fn anthropic_wire_body_carries_tools_with_input_schema() {
    let p = provider();
    let req = request_with_tools();
    let body: serde_json::Value = serde_json::from_slice(&p.wire_body_bytes(&req, false)).unwrap();
    let tools = body["tools"].as_array().expect("tools array present");
    assert_eq!(tools.len(), 2);
    let first = &tools[0];
    assert_eq!(first["name"].as_str(), Some("fs.list"));
    assert_eq!(first["description"].as_str(), Some("list a directory"));
    assert!(first["input_schema"]["properties"]["path"].is_object());
    let second = &tools[1];
    assert_eq!(second["name"].as_str(), Some("bash.exec"));
    assert!(second.get("description").is_none(), "second: {second}");
}

#[test]
fn anthropic_wire_body_omits_tools_field_when_list_empty() {
    let p = provider();
    let mut req = request_with_tools();
    req.tools.clear();
    let body: serde_json::Value = serde_json::from_slice(&p.wire_body_bytes(&req, false)).unwrap();
    assert!(body.get("tools").is_none(), "body: {body}");
}
