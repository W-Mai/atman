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
        model: "gpt-test".to_string(),
        messages: vec![atman_runtime::provider::user_text_message("hi")],
        system: None,
        input: Value::Unit,
        schema: None,
        cache_prompt: false,
    });

    let final_value = obs.output.await.unwrap();
    assert!(final_value.text_concat() == "hello openai");

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
            model: "gpt-test".to_string(),
            messages: vec![atman_runtime::provider::user_text_message("hi")],
            system: None,
            input: Value::Unit,
            schema: None,
            cache_prompt: false,
        })
        .await
        .unwrap();
    assert!(value.text_concat() == "hello world");
}

#[tokio::test]
async fn openai_multimodal_request_uses_image_url_parts() {
    let dir = tempfile::tempdir().unwrap();
    let img_path = dir.path().join("pic.jpg");
    let jpg_bytes: [u8; 4] = [0xFF, 0xD8, 0xFF, 0xE0];
    std::fs::write(&img_path, jpg_bytes).unwrap();
    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD.encode(jpg_bytes);
    let expected_url = format!("data:image/jpeg;base64,{data}");

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(wiremock::matchers::body_partial_json(serde_json::json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "image_url", "image_url": {"url": expected_url}},
                    {"type": "text", "text": "describe"}
                ]
            }]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "cmpl-x",
            "object": "chat.completion",
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new("openai", "k").with_base_url(server.uri());
    let user_msg = atman_runtime::message::Message {
        role: atman_runtime::message::MessageRole::User,
        parts: vec![
            atman_runtime::message::MessagePart::Image {
                source: atman_runtime::message::ImageSource {
                    media_type: "image/jpeg".into(),
                    data: atman_runtime::message::ImageData::Path {
                        path: img_path.clone(),
                    },
                },
            },
            atman_runtime::message::MessagePart::Text {
                text: "describe".into(),
            },
        ],
        turn_id: atman_runtime::event::TurnId::now(),
    };
    let v = provider
        .call(LlmRequest {
            model: "gpt-4o".to_string(),
            messages: vec![user_msg],
            system: None,
            input: Value::Unit,
            schema: None,
            cache_prompt: false,
        })
        .await
        .unwrap();
    assert!(v.text_concat() == "ok");
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
            model: "gpt-test".to_string(),
            messages: vec![atman_runtime::provider::user_text_message("hi")],
            system: None,
            input: Value::Unit,
            schema: None,
            cache_prompt: false,
        })
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        atman_runtime::RuntimeError::ToolFailed(msg) if msg.contains("401")
    ));
}

// Manual OpenAI-compat smoke test — needs an Ollama-like server on the wire.
// Run: `cargo test --test openai_streaming --ignored openai_real`.
// Env: ATMAN_TEST_OLLAMA_{BASE_URL,MODEL,KEY} — key can be any string, Ollama ignores it.
#[tokio::test]
#[ignore]
async fn openai_real() {
    let base = std::env::var("ATMAN_TEST_OLLAMA_BASE_URL")
        .unwrap_or_else(|_| "http://localhost:11434/v1".into());
    let model = std::env::var("ATMAN_TEST_OLLAMA_MODEL").unwrap_or_else(|_| "llama3.2".into());
    let key = std::env::var("ATMAN_TEST_OLLAMA_KEY").unwrap_or_else(|_| "sk-anything".into());

    let provider = OpenAiProvider::new("openai-compat", key).with_base_url(base);
    let obs = provider.call_streaming(LlmRequest {
        model,
        messages: vec![atman_runtime::provider::user_text_message(
            "Reply with exactly one short sentence.",
        )],
        system: None,
        input: Value::Unit,
        schema: None,
        cache_prompt: false,
    });
    let value = obs.output.await.unwrap();
    let text = value.text_concat();
    println!("[openai_real] {text}");
    assert!(!text.trim().is_empty());
}
