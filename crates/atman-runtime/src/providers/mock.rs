use std::collections::HashMap;

use crate::error::RuntimeError;
use crate::provider::{LlmRequest, Provider};
use crate::tool::BoxFut;
use crate::value::Value;

pub struct MockProvider {
    name: String,
    by_model: HashMap<String, Value>,
    by_prefix: Vec<(String, String, Value)>,
    fallback: Option<Value>,
}

impl MockProvider {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            by_model: HashMap::new(),
            by_prefix: Vec::new(),
            fallback: None,
        }
    }

    pub fn with_model(mut self, model: impl Into<String>, value: Value) -> Self {
        self.by_model.insert(model.into(), value);
        self
    }

    pub fn with_prefix(
        mut self,
        model: impl Into<String>,
        prompt_prefix: impl Into<String>,
        value: Value,
    ) -> Self {
        self.by_prefix
            .push((model.into(), prompt_prefix.into(), value));
        self
    }

    pub fn with_fallback(mut self, value: Value) -> Self {
        self.fallback = Some(value);
        self
    }
}

impl Provider for MockProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn call<'a>(&'a self, req: LlmRequest) -> BoxFut<'a, Result<Value, RuntimeError>> {
        Box::pin(async move {
            for (model, prefix, value) in &self.by_prefix {
                if req.model == *model && req.prompt.starts_with(prefix.as_str()) {
                    return Ok(value.clone());
                }
            }
            if let Some(v) = self.by_model.get(&req.model) {
                return Ok(v.clone());
            }
            if let Some(v) = &self.fallback {
                return Ok(v.clone());
            }
            Err(RuntimeError::ToolFailed(format!(
                "mock provider `{}` has no entry for model={} prompt.prefix={:?}",
                self.name,
                req.model,
                req.prompt.chars().take(40).collect::<String>()
            )))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolves_by_model_name() {
        let p = MockProvider::new("mock").with_model("gpt-4o-mini", Value::Str("hi".into()));
        let out = p
            .call(LlmRequest {
                model: "gpt-4o-mini".into(),
                prompt: "anything".into(),
                input: Value::Unit,
                schema: None,
            })
            .await
            .unwrap();
        assert!(matches!(out, Value::Str(s) if s == "hi"));
    }

    #[tokio::test]
    async fn prefix_wins_over_model() {
        let p = MockProvider::new("mock")
            .with_model("m", Value::Str("model-hit".into()))
            .with_prefix("m", "review", Value::Str("prefix-hit".into()));
        let out = p
            .call(LlmRequest {
                model: "m".into(),
                prompt: "review please".into(),
                input: Value::Unit,
                schema: None,
            })
            .await
            .unwrap();
        assert!(matches!(out, Value::Str(s) if s == "prefix-hit"));
    }

    #[tokio::test]
    async fn missing_entry_errors_with_hint() {
        let p = MockProvider::new("mock");
        let err = p
            .call(LlmRequest {
                model: "gpt".into(),
                prompt: "hello".into(),
                input: Value::Unit,
                schema: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, RuntimeError::ToolFailed(msg) if msg.contains("gpt")));
    }

    #[tokio::test]
    async fn fallback_captures_unmatched() {
        let p = MockProvider::new("mock").with_fallback(Value::Str("fb".into()));
        let out = p
            .call(LlmRequest {
                model: "anything".into(),
                prompt: "".into(),
                input: Value::Unit,
                schema: None,
            })
            .await
            .unwrap();
        assert!(matches!(out, Value::Str(s) if s == "fb"));
    }
}
