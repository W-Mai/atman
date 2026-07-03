use atman_runtime::event::NodeEvent;
use atman_runtime::provider::{LlmRequest, Provider};
use atman_runtime::providers::anthropic::AnthropicProvider;
use atman_runtime::value::Value;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SSE_STREAM: &str = "event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"m\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" atman\"}}\n\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":12}}\n\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\n";

#[tokio::test]
async fn anthropic_streaming_parses_content_block_delta() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "test-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(SSE_STREAM, "text/event-stream"),
        )
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new("anthropic", "test-key").with_base_url(server.uri());
    let mut obs = provider.call_streaming(LlmRequest {
        model: "claude-test".into(),
        prompt: "hi".into(),
        input: Value::Unit,
        schema: None,
    });

    let final_value = obs.output.await.unwrap();
    assert!(matches!(&final_value, Value::Str(s) if s == "hello atman"));

    let mut chunks = Vec::new();
    let mut total_tokens = 0u64;
    while let Ok(ev) = obs.events.try_recv() {
        match ev {
            NodeEvent::LlmChunk { text, .. } => chunks.push(text),
            NodeEvent::LlmDone { total_tokens: t } => total_tokens = t,
            _ => {}
        }
    }
    assert_eq!(chunks, vec!["hello", " atman"]);
    assert_eq!(total_tokens, 12);
}

#[tokio::test]
async fn anthropic_non_streaming_returns_concatenated_text() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "m",
            "type": "message",
            "role": "assistant",
            "model": "m",
            "content": [
                {"type": "text", "text": "hello "},
                {"type": "text", "text": "world"}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5, "output_tokens": 3}
        })))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new("anthropic", "test-key").with_base_url(server.uri());
    let value = provider
        .call(LlmRequest {
            model: "claude-test".into(),
            prompt: "hi".into(),
            input: Value::Unit,
            schema: None,
        })
        .await
        .unwrap();
    assert!(matches!(value, Value::Str(s) if s == "hello world"));
}

#[tokio::test]
async fn anthropic_http_error_becomes_tool_failed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new("anthropic", "bad").with_base_url(server.uri());
    let err = provider
        .call(LlmRequest {
            model: "claude-test".into(),
            prompt: "hi".into(),
            input: Value::Unit,
            schema: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        atman_runtime::RuntimeError::ToolFailed(msg) if msg.contains("401")
    ));
}

// Manual real-endpoint smoke test. Requires:
//   ATMAN_TEST_GLM_KEY=<key> cargo test --test anthropic_streaming --ignored anthropic_real
// The endpoint must be Anthropic-Messages-compatible; the test also honors
// ATMAN_TEST_GLM_BASE_URL (default: https://open.bigmodel.cn/api/anthropic)
// and ATMAN_TEST_GLM_MODEL (default: glm-4.6).
#[tokio::test]
#[ignore]
async fn anthropic_real() {
    let key = std::env::var("ATMAN_TEST_GLM_KEY").expect("set ATMAN_TEST_GLM_KEY to run");
    let base = std::env::var("ATMAN_TEST_GLM_BASE_URL")
        .unwrap_or_else(|_| "https://open.bigmodel.cn/api/anthropic".into());
    let model = std::env::var("ATMAN_TEST_GLM_MODEL").unwrap_or_else(|_| "glm-4.6".into());
    let provider = AnthropicProvider::new("glm", key).with_base_url(base);
    let obs = provider.call_streaming(LlmRequest {
        model,
        prompt: "Reply with exactly one short sentence.".into(),
        input: Value::Unit,
        schema: None,
    });
    let value = obs.output.await.unwrap();
    let text = match value {
        Value::Str(s) => s,
        other => panic!("expected string, got {other:?}"),
    };
    println!("[anthropic_real] {text}");
    assert!(!text.trim().is_empty());
}
