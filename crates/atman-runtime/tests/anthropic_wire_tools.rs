use atman_runtime::event::TurnId;
use atman_runtime::message::{Message, MessageOrigin, MessagePart, MessageRole};
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
            origin: MessageOrigin::User,
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
                name: "bash.spawn".into(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
            },
        ],
        reasoning: atman_runtime::provider::ReasoningSelection::ProviderDefault,
        stall_timeout_secs: 0,
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
    assert_eq!(first["name"].as_str(), Some("fs_list"));
    assert_eq!(first["description"].as_str(), Some("list a directory"));
    assert!(first["input_schema"]["properties"]["path"].is_object());
    let second = &tools[1];
    assert_eq!(second["name"].as_str(), Some("bash_spawn"));
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

#[test]
fn anthropic_effort_uses_adaptive_thinking_and_output_config() {
    let p = provider();
    let mut req = request_with_tools();
    req.reasoning = atman_runtime::provider::ReasoningSelection::Effort {
        effort: atman_runtime::provider::ReasoningEffort::High,
        execution_mode: None,
    };
    let body: serde_json::Value = serde_json::from_slice(&p.wire_body_bytes(&req, false)).unwrap();
    assert_eq!(body["thinking"]["type"], "adaptive");
    assert_eq!(body["output_config"]["effort"], "high");
    assert!(body["thinking"].get("budget_tokens").is_none());
}

#[test]
fn anthropic_budget_uses_legacy_manual_thinking() {
    let p = provider();
    let mut req = request_with_tools();
    req.reasoning = atman_runtime::provider::ReasoningSelection::BudgetTokens { tokens: 8192 };
    let body: serde_json::Value = serde_json::from_slice(&p.wire_body_bytes(&req, false)).unwrap();
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["thinking"]["budget_tokens"], 8192);
    assert!(body.get("output_config").is_none());
}
