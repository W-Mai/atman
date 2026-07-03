use std::collections::HashMap;

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::error::RuntimeError;
use crate::event::{NodeEvent, Observable};
use crate::provider::{DEFAULT_STREAM_BUFFER, LlmRequest, Provider, estimate_tokens};
use crate::tool::BoxFut;
use crate::value::Value;

pub struct MockProvider {
    name: String,
    by_model: HashMap<String, Value>,
    by_prefix: Vec<(String, String, Value)>,
    fallback: Option<Value>,
    chunk_delay: Option<std::time::Duration>,
}

impl MockProvider {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            by_model: HashMap::new(),
            by_prefix: Vec::new(),
            fallback: None,
            chunk_delay: None,
        }
    }

    pub fn with_chunk_delay(mut self, d: std::time::Duration) -> Self {
        self.chunk_delay = Some(d);
        self
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
        Box::pin(async move { self.lookup(&req) })
    }

    fn call_streaming(&self, req: LlmRequest) -> Observable<Value> {
        let (tx, events) = broadcast::channel(DEFAULT_STREAM_BUFFER);
        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();
        let looked_up = self.lookup(&req);
        let chunk_delay = self.chunk_delay;
        let output: BoxFut<'static, Result<Value, RuntimeError>> = Box::pin(async move {
            let value = match looked_up {
                Ok(v) => v,
                Err(e) => {
                    let _ = tx.send(NodeEvent::LlmDone { total_tokens: 0 });
                    return Err(e);
                }
            };
            let chunks = split_for_stream(&value);
            let mut running = 0u64;
            for chunk in chunks {
                if let Some(d) = chunk_delay {
                    tokio::select! {
                        biased;
                        _ = cancel_for_task.cancelled() => {
                            let _ = tx.send(NodeEvent::LlmDone { total_tokens: running });
                            return Err(RuntimeError::Cancelled("mock stream cancelled".into()));
                        }
                        _ = tokio::time::sleep(d) => {}
                    }
                }
                if cancel_for_task.is_cancelled() {
                    let _ = tx.send(NodeEvent::LlmDone {
                        total_tokens: running,
                    });
                    return Err(RuntimeError::Cancelled("mock stream cancelled".into()));
                }
                let inc = estimate_tokens(&chunk);
                running += inc;
                let _ = tx.send(NodeEvent::LlmChunk {
                    text: chunk,
                    cumulative_tokens: running,
                });
            }
            let _ = tx.send(NodeEvent::LlmDone {
                total_tokens: running,
            });
            Ok(value)
        });
        Observable {
            output,
            events,
            cancel,
        }
    }
}

impl MockProvider {
    fn lookup(&self, req: &LlmRequest) -> Result<Value, RuntimeError> {
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
    }
}

fn split_for_stream(value: &Value) -> Vec<String> {
    match value {
        Value::Str(s) if s.len() > 8 => s
            .as_bytes()
            .chunks(s.len().div_ceil(3))
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect(),
        Value::Str(s) => vec![s.clone()],
        other => vec![format!("{other:?}")],
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
                cache_prompt: false,
                attachments: vec![],
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
                cache_prompt: false,
                attachments: vec![],
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
                cache_prompt: false,
                attachments: vec![],
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
                cache_prompt: false,
                attachments: vec![],
            })
            .await
            .unwrap();
        assert!(matches!(out, Value::Str(s) if s == "fb"));
    }
}
