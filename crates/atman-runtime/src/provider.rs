use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::error::RuntimeError;
use crate::event::{NodeEvent, Observable};
use crate::tool::BoxFut;
use crate::value::Value;

#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub model: String,
    pub prompt: String,
    pub input: Value,
    pub schema: Option<String>,
}

pub trait Provider {
    fn name(&self) -> &str;
    fn call<'a>(&'a self, req: LlmRequest) -> BoxFut<'a, Result<Value, RuntimeError>>;

    fn call_streaming(&self, req: LlmRequest) -> Observable<Value>;
}

pub const DEFAULT_STREAM_BUFFER: usize = 1024;

pub fn wrap_call_as_streaming(
    call_future: BoxFut<'static, Result<Value, RuntimeError>>,
) -> Observable<Value> {
    let (tx, events) = broadcast::channel(DEFAULT_STREAM_BUFFER);
    let cancel = CancellationToken::new();
    let cancel_for_task = cancel.clone();
    let output: BoxFut<'static, Result<Value, RuntimeError>> = Box::pin(async move {
        tokio::select! {
            biased;
            _ = cancel_for_task.cancelled() => {
                let _ = tx.send(NodeEvent::LlmDone { total_tokens: 0 });
                Err(RuntimeError::Cancelled("call cancelled".into()))
            }
            result = call_future => {
                match &result {
                    Ok(Value::Str(s)) => {
                        let _ = tx.send(NodeEvent::LlmChunk {
                            text: s.clone(),
                            cumulative_tokens: estimate_tokens(s),
                        });
                        let _ = tx.send(NodeEvent::LlmDone { total_tokens: estimate_tokens(s) });
                    }
                    _ => {
                        let _ = tx.send(NodeEvent::LlmDone { total_tokens: 0 });
                    }
                }
                result
            }
        }
    });
    Observable {
        output,
        events,
        cancel,
    }
}

pub fn estimate_tokens(text: &str) -> u64 {
    ((text.len() as f64) / 3.5).ceil() as u64
}

#[derive(Default, Clone)]
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn Provider>>,
    default: Option<String>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, provider: Arc<dyn Provider>) {
        let name = provider.name().to_string();
        if self.default.is_none() {
            self.default = Some(name.clone());
        }
        self.providers.insert(name, provider);
    }

    pub fn set_default(&mut self, name: &str) {
        if self.providers.contains_key(name) {
            self.default = Some(name.to_string());
        }
    }

    pub fn resolve(&self, _model: &str) -> Option<Arc<dyn Provider>> {
        self.default
            .as_ref()
            .and_then(|n| self.providers.get(n).cloned())
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Provider>> {
        self.providers.get(name).cloned()
    }
}
