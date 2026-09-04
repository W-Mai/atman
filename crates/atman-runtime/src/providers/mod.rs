pub mod anthropic;
pub mod codex;
pub mod mock;
pub mod openai;

pub(crate) fn classify_attachment_error(status: u16, body: &str) -> Option<String> {
    if !matches!(status, 400 | 413) {
        return None;
    }
    let lower = body.to_ascii_lowercase();
    let attachment_markers = [
        "image",
        "attachment",
        "images",
        "attachments",
        "invalid_image",
        "image_parse_error",
        "invalid_image_url",
    ];
    let has_marker = lower
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|word| attachment_markers.contains(&word));
    if !has_marker {
        return None;
    }
    Some(if status == 413 {
        "payload_too_large".into()
    } else {
        pick_reason(&lower)
    })
}

fn pick_reason(lower: &str) -> String {
    if lower.contains("invalid_image_url") {
        "invalid_image_url".into()
    } else if lower.contains("image_parse_error") {
        "image_parse_error".into()
    } else if lower.contains("unsupported") {
        "unsupported_media_type".into()
    } else if lower.contains("too large") || lower.contains("payload_too_large") {
        "payload_too_large".into()
    } else {
        "invalid_image".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CASES: &[(u16, &str, Option<&str>)] = &[
        (400, "Unsupported parameter: reasoning_effort", None),
        (400, "Unsupported parameter: image_detail", None),
        (400, "unsupported media_type: application/json", None),
        (400, "request_too_large", None),
        (413, "", None),
        (413, "request payload too large", None),
        (401, "image payload too large", None),
        (403, "image access denied", None),
        (429, "image request too large", None),
        (500, "image payload too large", None),
        (
            400,
            r#"{"error":{"code":"invalid_image_url","message":"..."}}"#,
            Some("invalid_image_url"),
        ),
        (
            400,
            r#"{"error":{"code":"image_parse_error"}}"#,
            Some("image_parse_error"),
        ),
        (
            400,
            "unsupported media_type for image",
            Some("unsupported_media_type"),
        ),
        (400, "IMAGE payload_too_large", Some("payload_too_large")),
        (400, "invalid images", Some("invalid_image")),
        (
            413,
            "image data exceeds the limit",
            Some("payload_too_large"),
        ),
    ];

    #[test]
    fn attachment_classification_requires_validation_status_and_image_evidence() {
        for &(status, body, reason) in CASES {
            assert_eq!(
                classify_attachment_error(status, body).as_deref(),
                reason,
                "{status}: {body}"
            );
        }
    }

    #[tokio::test]
    async fn providers_share_attachment_error_classification_across_call_modes() {
        use crate::provider::{LlmRequest, Provider, ReasoningSelection};
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

        let server = MockServer::start().await;
        let providers: [Box<dyn Provider>; 3] = [
            Box::new(openai::OpenAiProvider::new("openai", "test-key").with_base_url(server.uri())),
            Box::new(
                anthropic::AnthropicProvider::new("anthropic", "test-key")
                    .with_base_url(server.uri()),
            ),
            Box::new(
                codex::CodexProvider::new("codex", "test-key", "account").with_endpoints(
                    format!("{}/responses", server.uri()),
                    format!("{}/models", server.uri()),
                ),
            ),
        ];
        let request = LlmRequest {
            model: "test-model".into(),
            messages: vec![crate::provider::user_text_message("hello")],
            system: None,
            input: crate::Value::Unit,
            schema: None,
            cache_prompt: false,
            prompt_cache_key: None,
            tools: Vec::new(),
            reasoning: ReasoningSelection::ProviderDefault,
            stall_timeout_secs: 0,
        };
        for &(status, body, reason) in CASES {
            let mock = Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(status).set_body_string(body))
                .expect(6)
                .mount_as_scoped(&server)
                .await;
            for provider in &providers {
                for streaming in [false, true] {
                    let result = if streaming {
                        provider.call_streaming(request.clone()).output.await
                    } else {
                        provider.call(request.clone()).await
                    };
                    let error = result.unwrap_err();
                    match (reason, &error) {
                        (Some(expected), crate::RuntimeError::AttachmentError { reason }) => {
                            assert_eq!(reason, expected)
                        }
                        (None, crate::RuntimeError::ToolFailed(message)) => {
                            assert!(message.contains(&status.to_string()))
                        }
                        _ => panic!(
                            "{} streaming={streaming} status={status}: {error:?}",
                            provider.name()
                        ),
                    }
                }
            }
            drop(mock);
        }
    }
}
