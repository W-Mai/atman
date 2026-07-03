use atman_runtime::event::NodeEvent;
use atman_runtime::provider::{LlmRequest, Provider};
use atman_runtime::providers::openai::OpenAiProvider;
use atman_runtime::value::Value;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SSE_STREAM: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n\
data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"}}]}\n\n\
data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\" openai\"}}]}\n\n\
data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":9,\"total_tokens\":14}}\n\n\
data: [DONE]\n\n";

#[tokio::test]
async fn openai_streaming_parses_delta_content() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(SSE_STREAM, "text/event-stream"),
        )
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new("openai", "test-key").with_base_url(server.uri());
    let mut obs = provider.call_streaming(LlmRequest {
        model: "gpt-test".into(),
        prompt: "hi".into(),
        input: Value::Unit,
        schema: None,
        cache_prompt: false,
        attachments: vec![],
    });

    let final_value = obs.output.await.unwrap();
    assert!(matches!(&final_value, Value::Str(s) if s == "hello openai"));

    let mut chunks = Vec::new();
    let mut total_tokens = 0u64;
    while let Ok(ev) = obs.events.try_recv() {
        match ev {
            NodeEvent::LlmChunk { text, .. } => chunks.push(text),
            NodeEvent::LlmDone { total_tokens: t } => total_tokens = t,
            _ => {}
        }
    }
    assert_eq!(chunks, vec!["hello", " openai"]);
    assert_eq!(total_tokens, 9);
}

#[tokio::test]
async fn openai_non_streaming_returns_message_content() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "cmpl-xyz",
            "object": "chat.completion",
            "model": "gpt-test",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hello world"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
        })))
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new("openai", "test-key").with_base_url(server.uri());
    let value = provider
        .call(LlmRequest {
            model: "gpt-test".into(),
            prompt: "hi".into(),
            input: Value::Unit,
            schema: None,
            cache_prompt: false,
            attachments: vec![],
        })
        .await
        .unwrap();
    assert!(matches!(value, Value::Str(s) if s == "hello world"));
}

#[tokio::test]
async fn openai_http_error_becomes_tool_failed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new("openai", "bad").with_base_url(server.uri());
    let err = provider
        .call(LlmRequest {
            model: "gpt-test".into(),
            prompt: "hi".into(),
            input: Value::Unit,
            schema: None,
            cache_prompt: false,
            attachments: vec![],
        })
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        atman_runtime::RuntimeError::ToolFailed(msg) if msg.contains("401")
    ));
}
