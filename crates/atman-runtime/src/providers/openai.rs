use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::error::RuntimeError;
use crate::event::{NodeEvent, Observable};
use crate::provider::{DEFAULT_STREAM_BUFFER, LlmRequest, Provider, estimate_tokens};
use crate::tool::BoxFut;
use crate::value::Value;

pub struct OpenAiProvider {
    name: String,
    api_key: String,
    base_url: String,
    client: reqwest::Client,
    max_tokens: Option<u32>,
}

impl OpenAiProvider {
    pub fn new(name: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            api_key: api_key.into(),
            base_url: "https://api.openai.com/v1".into(),
            client: reqwest::Client::new(),
            max_tokens: None,
        }
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    pub fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = Some(n);
        self
    }

    fn build_request(&self, req: &LlmRequest, stream: bool) -> reqwest::RequestBuilder {
        let body = ChatCompletionsRequest {
            model: req.model.clone(),
            stream,
            max_tokens: self.max_tokens,
            messages: vec![ChatMessage {
                role: "user",
                content: req.prompt.clone(),
            }],
        };
        self.client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
    }
}

impl Provider for OpenAiProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn call<'a>(&'a self, req: LlmRequest) -> BoxFut<'a, Result<Value, RuntimeError>> {
        let request = self.build_request(&req, false);
        Box::pin(async move {
            let resp = request.send().await.map_err(net_err)?;
            let status = resp.status();
            let body: ChatCompletionsResponse = if status.is_success() {
                resp.json().await.map_err(net_err)?
            } else {
                return Err(RuntimeError::ToolFailed(format!(
                    "openai http {}: {}",
                    status,
                    resp.text().await.unwrap_or_default()
                )));
            };
            let text = body
                .choices
                .into_iter()
                .next()
                .and_then(|c| c.message.content)
                .unwrap_or_default();
            Ok(Value::Str(text))
        })
    }

    fn call_streaming(&self, req: LlmRequest) -> Observable<Value> {
        let request = self.build_request(&req, true);
        let (tx, events) = broadcast::channel(DEFAULT_STREAM_BUFFER);
        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();
        let output: BoxFut<'static, Result<Value, RuntimeError>> = Box::pin(async move {
            use eventsource_stream::Eventsource;
            use futures::StreamExt;

            let resp = tokio::select! {
                biased;
                _ = cancel_for_task.cancelled() => return Err(RuntimeError::Cancelled("openai cancelled before send".into())),
                r = request.send() => r.map_err(net_err)?,
            };
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(RuntimeError::ToolFailed(format!(
                    "openai http {status}: {body}"
                )));
            }

            let mut stream = resp.bytes_stream().eventsource();
            let mut acc = String::new();
            let mut cumulative = 0u64;
            let mut final_completion_tokens: Option<u64> = None;
            while let Some(event) = tokio::select! {
                biased;
                _ = cancel_for_task.cancelled() => None,
                next = stream.next() => next,
            } {
                let event = event.map_err(|e| RuntimeError::ToolFailed(format!("sse: {e}")))?;
                if event.data == "[DONE]" {
                    break;
                }
                if event.data.is_empty() {
                    continue;
                }
                let parsed: serde_json::Value = match serde_json::from_str(&event.data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(content) = parsed
                    .pointer("/choices/0/delta/content")
                    .and_then(|v| v.as_str())
                {
                    if !content.is_empty() {
                        acc.push_str(content);
                        cumulative += estimate_tokens(content);
                        let _ = tx.send(NodeEvent::LlmChunk {
                            text: content.to_string(),
                            cumulative_tokens: cumulative,
                        });
                    }
                }
                if let Some(usage) = parsed
                    .pointer("/usage/completion_tokens")
                    .and_then(|v| v.as_u64())
                {
                    final_completion_tokens = Some(usage);
                }
            }
            if cancel_for_task.is_cancelled() {
                let _ = tx.send(NodeEvent::LlmDone {
                    total_tokens: cumulative,
                });
                return Err(RuntimeError::Cancelled(
                    "openai cancelled mid-stream".into(),
                ));
            }
            let total = final_completion_tokens.unwrap_or(cumulative);
            let _ = tx.send(NodeEvent::LlmDone {
                total_tokens: total,
            });
            Ok(Value::Str(acc))
        });
        Observable {
            output,
            events,
            cancel,
        }
    }
}

fn net_err(e: reqwest::Error) -> RuntimeError {
    RuntimeError::ToolFailed(format!("openai net: {e}"))
}

#[derive(Serialize)]
struct ChatCompletionsRequest {
    model: String,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    messages: Vec<ChatMessage>,
}

#[derive(Serialize)]
struct ChatMessage {
    role: &'static str,
    content: String,
}

#[derive(Deserialize)]
struct ChatCompletionsResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
}

#[derive(Deserialize)]
struct ChatChoiceMessage {
    content: Option<String>,
}
