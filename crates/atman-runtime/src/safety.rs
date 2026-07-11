use std::sync::Arc;

use crate::error::RuntimeError;
use crate::tool::BoxFut;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanVerdict {
    Pass,
    Warn(Vec<String>),
    Deny(Vec<String>),
}

impl ScanVerdict {
    pub fn is_pass(&self) -> bool {
        matches!(self, ScanVerdict::Pass)
    }

    pub fn is_deny(&self) -> bool {
        matches!(self, ScanVerdict::Deny(_))
    }

    pub fn categories(&self) -> &[String] {
        match self {
            ScanVerdict::Pass => &[],
            ScanVerdict::Warn(c) | ScanVerdict::Deny(c) => c,
        }
    }
}

pub trait SafetyClassifier: Send + Sync {
    fn scan<'a>(&'a self, text: &'a str) -> BoxFut<'a, Result<ScanVerdict, RuntimeError>>;
    fn kind(&self) -> &'static str;
}

pub struct NoopClassifier;

impl SafetyClassifier for NoopClassifier {
    fn scan<'a>(&'a self, _text: &'a str) -> BoxFut<'a, Result<ScanVerdict, RuntimeError>> {
        Box::pin(async { Ok(ScanVerdict::Pass) })
    }

    fn kind(&self) -> &'static str {
        "noop"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SafetyMode {
    #[default]
    Warn,
    Deny,
}

pub struct OpenAiModerationClassifier {
    api_key: String,
    base_url: String,
    client: reqwest::Client,
    model: String,
    timeout: std::time::Duration,
}

impl OpenAiModerationClassifier {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: "https://api.openai.com/v1".to_string(),
            client: reqwest::Client::new(),
            model: "omni-moderation-latest".to_string(),
            timeout: std::time::Duration::from_secs(5),
        }
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl SafetyClassifier for OpenAiModerationClassifier {
    fn scan<'a>(&'a self, text: &'a str) -> BoxFut<'a, Result<ScanVerdict, RuntimeError>> {
        Box::pin(async move {
            let body = serde_json::json!({ "input": text, "model": self.model });
            let call = self
                .client
                .post(format!("{}/moderations", self.base_url))
                .bearer_auth(&self.api_key)
                .json(&body)
                .send();
            let resp = tokio::time::timeout(self.timeout, call)
                .await
                .map_err(|_| {
                    RuntimeError::ToolFailed(format!(
                        "safety: openai moderation timed out after {}s",
                        self.timeout.as_secs()
                    ))
                })?;
            let resp = resp
                .map_err(|e| RuntimeError::ToolFailed(format!("safety: openai moderation: {e}")))?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(RuntimeError::ToolFailed(format!(
                    "safety: openai moderation http {status}: {body}"
                )));
            }
            let doc: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| RuntimeError::ToolFailed(format!("safety: parse moderation: {e}")))?;
            let Some(result) = doc.get("results").and_then(|r| r.get(0)) else {
                return Ok(ScanVerdict::Pass);
            };
            let flagged = result
                .get("flagged")
                .and_then(|f| f.as_bool())
                .unwrap_or(false);
            if !flagged {
                return Ok(ScanVerdict::Pass);
            }
            let categories = result
                .get("categories")
                .and_then(|c| c.as_object())
                .map(|obj| {
                    obj.iter()
                        .filter(|(_, v)| v.as_bool().unwrap_or(false))
                        .map(|(k, _)| k.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            Ok(ScanVerdict::Deny(categories))
        })
    }

    fn kind(&self) -> &'static str {
        "openai-moderation"
    }
}

pub struct SafetyConfig {
    pub enabled: bool,
    pub mode: SafetyMode,
    pub auto_rewrite: bool,
    pub classifier: Arc<dyn SafetyClassifier>,
}

impl Clone for SafetyConfig {
    fn clone(&self) -> Self {
        Self {
            enabled: self.enabled,
            mode: self.mode,
            auto_rewrite: self.auto_rewrite,
            classifier: self.classifier.clone(),
        }
    }
}

impl SafetyConfig {
    pub fn noop() -> Self {
        Self {
            enabled: false,
            mode: SafetyMode::Warn,
            auto_rewrite: false,
            classifier: Arc::new(NoopClassifier),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn noop_classifier_always_passes() {
        let c = NoopClassifier;
        assert_eq!(c.scan("anything").await.unwrap(), ScanVerdict::Pass);
        assert_eq!(c.kind(), "noop");
    }

    #[tokio::test]
    async fn openai_moderation_passes_when_flagged_is_false() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/moderations"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{
                    "flagged": false,
                    "categories": {"hate": false, "violence": false}
                }]
            })))
            .mount(&server)
            .await;
        let c = OpenAiModerationClassifier::new("test-key").with_base_url(server.uri());
        let out = c.scan("hello world").await.unwrap();
        assert_eq!(out, ScanVerdict::Pass);
    }

    #[tokio::test]
    async fn openai_moderation_returns_deny_with_flagged_categories() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/moderations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{
                    "flagged": true,
                    "categories": {"hate": true, "violence": false, "self-harm": true}
                }]
            })))
            .mount(&server)
            .await;
        let c = OpenAiModerationClassifier::new("k").with_base_url(server.uri());
        let out = c.scan("bad").await.unwrap();
        let mut cats: Vec<String> = match out {
            ScanVerdict::Deny(c) => c,
            other => panic!("expected Deny, got {other:?}"),
        };
        cats.sort();
        assert_eq!(cats, vec!["hate".to_string(), "self-harm".to_string()]);
    }

    #[tokio::test]
    async fn openai_moderation_bubbles_http_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/moderations"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
            .mount(&server)
            .await;
        let c = OpenAiModerationClassifier::new("k").with_base_url(server.uri());
        let err = c.scan("hi").await.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("401"), "err: {msg}");
    }
}
