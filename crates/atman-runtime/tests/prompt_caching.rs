use std::sync::Arc;

use atman_runtime::provider::{LlmRequest, Provider};
use atman_runtime::providers::anthropic::AnthropicProvider;
use atman_runtime::value::Value;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use atman_dsl::parse::parse_file;
use atman_runtime::Executor;

#[tokio::test]
async fn cache_prompt_true_sends_ephemeral_cache_control() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(serde_json::json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "cache_control": {"type": "ephemeral"}
                }]
            }]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "m",
            "type": "message",
            "role": "assistant",
            "model": "test",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5, "output_tokens": 1}
        })))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new("anthropic", "test-key").with_base_url(server.uri());
    let value = provider
        .call(LlmRequest {
            model: "test".to_string(),
            messages: vec![atman_runtime::provider::user_text_message("cache me")],
            system: None,
            input: Value::Unit,
            schema: None,
            cache_prompt: true,
            tools: Vec::new(),
        })
        .await
        .unwrap();
    assert!(value.text_concat() == "ok");
}

#[tokio::test]
async fn cache_prompt_false_sends_plain_string_content() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(serde_json::json!({
            "messages": [{
                "role": "user",
                "content": [{"type": "text", "text": "no cache"}]
            }]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "m",
            "type": "message",
            "role": "assistant",
            "model": "test",
            "content": [{"type": "text", "text": "ok"}],
            "usage": {"input_tokens": 5, "output_tokens": 1}
        })))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new("anthropic", "test-key").with_base_url(server.uri());
    let value = provider
        .call(LlmRequest {
            model: "test".to_string(),
            messages: vec![atman_runtime::provider::user_text_message("no cache")],
            system: None,
            input: Value::Unit,
            schema: None,
            cache_prompt: false,
            tools: Vec::new(),
        })
        .await
        .unwrap();
    assert!(value.text_concat() == "ok");
}

#[tokio::test]
async fn cache_kwarg_flows_from_dsl_to_provider() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(serde_json::json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "cache_control": {"type": "ephemeral"}
                }]
            }]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "m",
            "type": "message",
            "role": "assistant",
            "model": "test",
            "content": [{"type": "text", "text": "hi"}],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .mount(&server)
        .await;

    let src = r#"flow t() -> string {
    return llm {
        model: "test"
        prompt: "cache me"
        cache: true
    }
}
"#;
    let file = parse_file(src).unwrap();
    let mut ex = Executor::new();
    ex.providers.register(Arc::new(
        AnthropicProvider::new("anthropic", "test-key").with_base_url(server.uri()),
    ));
    let out = ex.run(&file, "t", vec![]).await.unwrap();
    assert!(matches!(out, Value::Str(s) if s == "hi"));
}
