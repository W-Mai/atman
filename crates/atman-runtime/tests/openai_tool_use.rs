use atman_runtime::message::MessagePart;
use atman_runtime::provider::{LlmRequest, Provider};
use atman_runtime::providers::openai::OpenAiProvider;
use atman_runtime::value::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn tool_call_response() -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-tooluse-1",
        "object": "chat.completion",
        "model": "gpt-test",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_abc",
                    "type": "function",
                    "function": {
                        "name": "fs.read",
                        "arguments": "{\"path\": \"foo.txt\"}"
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 3, "total_tokens": 13}
    })
}

#[tokio::test]
async fn openai_non_streaming_returns_tool_use_parts_from_tool_calls() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(tool_call_response()))
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new("openai", "test-key").with_base_url(server.uri());
    let am = provider
        .call(LlmRequest {
            model: "gpt-test".to_string(),
            messages: vec![atman_runtime::provider::user_text_message("edit foo.txt")],
            system: None,
            input: Value::Unit,
            schema: None,
            cache_prompt: false,
        })
        .await
        .expect("call ok");

    let tool_use = am
        .message
        .parts
        .iter()
        .find_map(|p| match p {
            MessagePart::ToolUse { id, name, input } => {
                Some((id.clone(), name.clone(), input.clone()))
            }
            _ => None,
        })
        .expect("expected a ToolUse part parsed from tool_calls");
    assert_eq!(tool_use.0, "call_abc");
    assert_eq!(tool_use.1, "fs.read");
    assert_eq!(tool_use.2["path"], serde_json::json!("foo.txt"));
    assert_eq!(am.token_usage.input, 10);
    assert_eq!(am.token_usage.output, 3);
}

const STREAM_TOOL_CALLS: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":null,\"tool_calls\":[{\"index\":0,\"id\":\"call_zz\",\"type\":\"function\",\"function\":{\"name\":\"fs.write\",\"arguments\":\"\"}}]}}]}\n\n\
data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\": \\\"a.txt\\\", \\\"content\\\": \\\"hi\\\"}\"}}]}}]}\n\n\
data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":8,\"total_tokens\":28}}\n\n\
data: [DONE]\n\n";

#[tokio::test]
async fn openai_streaming_accumulates_tool_calls_across_chunks() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(STREAM_TOOL_CALLS, "text/event-stream"),
        )
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new("openai", "test-key").with_base_url(server.uri());
    let obs = provider.call_streaming(LlmRequest {
        model: "gpt-test".to_string(),
        messages: vec![atman_runtime::provider::user_text_message("write a.txt")],
        system: None,
        input: Value::Unit,
        schema: None,
        cache_prompt: false,
    });
    let am = obs.output.await.expect("streaming call ok");
    let tool_use = am
        .message
        .parts
        .iter()
        .find_map(|p| match p {
            MessagePart::ToolUse { id, name, input } => {
                Some((id.clone(), name.clone(), input.clone()))
            }
            _ => None,
        })
        .expect("expected accumulated ToolUse across streamed chunks");
    assert_eq!(tool_use.0, "call_zz");
    assert_eq!(tool_use.1, "fs.write");
    assert_eq!(tool_use.2["path"], serde_json::json!("a.txt"));
    assert_eq!(tool_use.2["content"], serde_json::json!("hi"));
}
