use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::error::RuntimeError;
use crate::event::{NodeEvent, Observable};
use crate::message::{Message, MessagePart, MessageRole};
use crate::tool::BoxFut;
use crate::value::Value;

#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub system: Option<String>,
    pub input: Value,
    pub schema: Option<String>,
    pub cache_prompt: bool,
    pub tools: Vec<crate::tool::ToolSpec>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct TokenUsage {
    pub input: u64,
    pub cached_input: u64,
    pub output: u64,
    pub cache_write: u64,
}

impl TokenUsage {
    pub fn total(&self) -> u64 {
        self.input
            .saturating_add(self.cached_input)
            .saturating_add(self.output)
            .saturating_add(self.cache_write)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    End,
    ToolUse,
    Length,
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct AssistantMessage {
    pub message: Message,
    pub stop_reason: StopReason,
    pub token_usage: TokenUsage,
}

impl AssistantMessage {
    pub fn text_only(msg: Message) -> Self {
        Self {
            message: msg,
            stop_reason: StopReason::End,
            token_usage: TokenUsage::default(),
        }
    }

    pub fn text_concat(&self) -> String {
        self.message.text_concat()
    }
}

pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn call<'a>(&'a self, req: LlmRequest) -> BoxFut<'a, Result<AssistantMessage, RuntimeError>>;
    fn call_streaming(&self, req: LlmRequest) -> Observable<AssistantMessage>;
}

pub const DEFAULT_STREAM_BUFFER: usize = 1024;

pub fn wrap_call_as_streaming(
    call_future: BoxFut<'static, Result<AssistantMessage, RuntimeError>>,
) -> Observable<AssistantMessage> {
    let (tx, events) = broadcast::channel(DEFAULT_STREAM_BUFFER);
    let cancel = CancellationToken::new();
    let cancel_for_task = cancel.clone();
    let output: BoxFut<'static, Result<AssistantMessage, RuntimeError>> = Box::pin(async move {
        tokio::select! {
            biased;
            _ = cancel_for_task.cancelled() => {
                let _ = tx.send(NodeEvent::LlmDone { total_tokens: 0 });
                Err(RuntimeError::Cancelled("call cancelled".into()))
            }
            result = call_future => {
                match &result {
                    Ok(am) => {
                        let text = am.text_concat();
                        if !text.is_empty() {
                            let _ = tx.send(NodeEvent::LlmChunk {
                                text: text.clone(),
                                cumulative_tokens: estimate_tokens(&text),
                            });
                        }
                        let _ = tx.send(NodeEvent::LlmDone { total_tokens: am.token_usage.output });
                    }
                    Err(_) => {
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

pub fn assistant_message_to_value(am: &AssistantMessage) -> Value {
    let has_structural_part = am
        .message
        .parts
        .iter()
        .any(|p| !matches!(p, MessagePart::Text { .. }));
    if has_structural_part {
        return Value::Message(am.message.clone());
    }
    let text = am.text_concat();
    if text.is_empty() {
        return Value::Message(am.message.clone());
    }
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(json) => Value::from_json(json),
        Err(_) => Value::Str(text),
    }
}

pub fn user_text_message(text: impl Into<String>) -> Message {
    Message {
        role: MessageRole::User,
        parts: vec![MessagePart::Text { text: text.into() }],
        turn_id: crate::event::TurnId::now(),
    }
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

    pub fn resolve(&self, model: &str) -> Option<Arc<dyn Provider>> {
        if let Some(p) = self.providers.get(model) {
            return Some(p.clone());
        }
        self.default
            .as_ref()
            .and_then(|n| self.providers.get(n).cloned())
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Provider>> {
        self.providers.get(name).cloned()
    }
}
