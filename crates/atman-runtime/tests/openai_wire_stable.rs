use atman_runtime::event::TurnId;
use atman_runtime::message::{Message, MessagePart, MessageRole};
use atman_runtime::provider::LlmRequest;
use atman_runtime::providers::openai::OpenAiProvider;
use atman_runtime::value::Value;
use uuid::Uuid;

fn provider() -> OpenAiProvider {
    OpenAiProvider::new("openai", "test-key").with_base_url("http://irrelevant".to_string())
}

fn tid() -> TurnId {
    TurnId(Uuid::nil())
}

fn user(text: &str) -> Message {
    Message {
        role: MessageRole::User,
        parts: vec![MessagePart::Text {
            text: text.to_string(),
        }],
        turn_id: tid(),
    }
}

fn tool_result(id: &str, content: &str) -> Message {
    Message {
        role: MessageRole::Tool,
        parts: vec![MessagePart::ToolResult {
            tool_use_id: id.to_string(),
            content: content.to_string(),
            is_error: false,
        }],
        turn_id: tid(),
    }
}

fn assistant_with_tool_use(text: &str, id: &str, name: &str, input: serde_json::Value) -> Message {
    Message {
        role: MessageRole::Assistant,
        parts: vec![
            MessagePart::Text {
                text: text.to_string(),
            },
            MessagePart::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input,
            },
        ],
        turn_id: tid(),
    }
}

fn fixed_request() -> LlmRequest {
    LlmRequest {
        model: "gpt-test".to_string(),
        messages: vec![
            user("what is the weather in tokyo?"),
            assistant_with_tool_use(
                "let me check",
                "call_1",
                "get_weather",
                serde_json::json!({"city": "tokyo", "unit": "c"}),
            ),
            tool_result("call_1", "{\"temp_c\":22}"),
            user("thanks — now paris?"),
        ],
        system: Some("you are a helpful assistant.".to_string()),
        input: Value::Unit,
        schema: None,
        cache_prompt: false,
        tools: Vec::new(),
        thinking_enabled: false,
        stall_timeout_secs: 0,
    }
}

#[test]
fn openai_wire_body_is_byte_stable_across_100_serializations() {
    let p = provider();
    let req = fixed_request();
    let first = p.wire_body_bytes(&req, false);
    for i in 0..100 {
        let again = p.wire_body_bytes(&req, false);
        assert_eq!(
            again, first,
            "wire body drift at iteration {i} — OpenAI prompt cache requires byte-stable serialization"
        );
    }
}

#[test]
fn openai_wire_body_tool_use_input_key_order_is_stable() {
    let p = provider();
    let req = LlmRequest {
        model: "m".to_string(),
        messages: vec![assistant_with_tool_use(
            "",
            "call_x",
            "f",
            serde_json::json!({"zeta": 1, "alpha": 2, "middle": 3}),
        )],
        system: None,
        input: Value::Unit,
        schema: None,
        cache_prompt: false,
        tools: Vec::new(),
        thinking_enabled: false,
        stall_timeout_secs: 0,
    };
    let body = p.wire_body_bytes(&req, false);
    let s = std::str::from_utf8(&body).unwrap();
    let z = s.find("zeta").expect("zeta present");
    let a = s.find("alpha").expect("alpha present");
    let m = s.find("middle").expect("middle present");
    assert!(
        z < a && a < m,
        "tool_use input JSON keys reordered — serde_json preserve_order broken? body={s}"
    );
}

#[test]
fn openai_wire_body_carries_tool_specs_as_function_calls() {
    let p = provider();
    let mut req = fixed_request();
    req.tools = vec![
        atman_runtime::tool::ToolSpec {
            name: "fs.read".to_string(),
            description: Some("read a file".into()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        },
        atman_runtime::tool::ToolSpec {
            name: "bash.exec".to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
        },
    ];
    let body: serde_json::Value = serde_json::from_slice(&p.wire_body_bytes(&req, false)).unwrap();
    let tools = body["tools"].as_array().expect("tools array present");
    assert_eq!(tools.len(), 2);
    let first = &tools[0];
    assert_eq!(first["type"].as_str(), Some("function"));
    assert_eq!(first["function"]["name"].as_str(), Some("fs.read"));
    assert_eq!(
        first["function"]["description"].as_str(),
        Some("read a file")
    );
    assert!(first["function"]["parameters"]["properties"]["path"].is_object());
    let second = &tools[1];
    assert_eq!(second["function"]["name"].as_str(), Some("bash.exec"));
    assert!(second["function"].get("description").is_none());
}

#[test]
fn openai_wire_body_omits_tools_field_when_list_empty() {
    let p = provider();
    let req = fixed_request();
    let body: serde_json::Value = serde_json::from_slice(&p.wire_body_bytes(&req, false)).unwrap();
    assert!(body.get("tools").is_none(), "body: {body}");
}

#[test]
fn openai_wire_body_prefix_stable_when_new_message_appended() {
    let p = provider();
    let base = fixed_request();
    let mut extended = fixed_request();
    extended.messages.push(user("and london?"));

    let base_body: serde_json::Value =
        serde_json::from_slice(&p.wire_body_bytes(&base, false)).unwrap();
    let ext_body: serde_json::Value =
        serde_json::from_slice(&p.wire_body_bytes(&extended, false)).unwrap();

    let base_msgs = base_body["messages"].as_array().unwrap();
    let ext_msgs = ext_body["messages"].as_array().unwrap();
    assert_eq!(ext_msgs.len(), base_msgs.len() + 1);
    for (i, base_msg) in base_msgs.iter().enumerate() {
        assert_eq!(
            &ext_msgs[i], base_msg,
            "message {i} drifted when we appended a new turn — breaks OpenAI prefix cache"
        );
    }
}
