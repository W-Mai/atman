use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::error::RuntimeError;
use crate::event::{NodeEvent, Observable};
use crate::provider::{DEFAULT_STREAM_BUFFER, LlmRequest, Provider, estimate_tokens};
use crate::tool::BoxFut;
use crate::value::Value;

pub struct AnthropicProvider {
    name: String,
    api_key: String,
    base_url: String,
    client: reqwest::Client,
    max_tokens: u32,
    anthropic_version: String,
}

impl AnthropicProvider {
    pub fn new(name: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            api_key: api_key.into(),
            base_url: "https://api.anthropic.com".into(),
            client: reqwest::Client::new(),
            max_tokens: 4096,
            anthropic_version: "2023-06-01".into(),
        }
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    pub fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n;
        self
    }

    pub fn with_anthropic_version(mut self, v: impl Into<String>) -> Self {
        self.anthropic_version = v.into();
        self
    }

    fn build_request(&self, req: &LlmRequest, stream: bool) -> reqwest::RequestBuilder {
        let content = build_content(req);
        let body = MessagesRequest {
            model: req.model.clone(),
            max_tokens: self.max_tokens,
            stream,
            messages: vec![Message {
                role: "user",
                content,
            }],
        };
        self.client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", &self.anthropic_version)
            .json(&body)
    }
}

fn build_content(req: &LlmRequest) -> MessageContent {
    let cache = if req.cache_prompt {
        Some(CacheControl { kind: "ephemeral" })
    } else {
        None
    };
    if req.attachments.is_empty() && !req.cache_prompt {
        return MessageContent::Text(req.prompt.clone());
    }
    let mut blocks: Vec<ContentPart> = req
        .attachments
        .iter()
        .filter_map(|a| match a.kind {
            crate::provider::AttachmentKind::Image => {
                let bytes = std::fs::read(&a.path).ok()?;
                use base64::Engine;
                let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
                let media_type = a
                    .mime
                    .clone()
                    .or_else(|| guess_image_mime(&a.path))
                    .unwrap_or_else(|| "image/png".to_string());
                Some(ContentPart::Image {
                    source: ImageSource {
                        kind: "base64",
                        media_type,
                        data,
                    },
                })
            }
            crate::provider::AttachmentKind::File => None,
        })
        .collect();
    blocks.push(ContentPart::Text {
        text: req.prompt.clone(),
        cache_control: cache,
    });
    MessageContent::Blocks(blocks)
}

fn guess_image_mime(path: &std::path::Path) -> Option<String> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())?
        .to_ascii_lowercase();
    Some(
        match ext.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            _ => return None,
        }
        .to_string(),
    )
}

impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn call<'a>(&'a self, req: LlmRequest) -> BoxFut<'a, Result<Value, RuntimeError>> {
        let request = self.build_request(&req, false);
        Box::pin(async move {
            let resp = request.send().await.map_err(net_err)?;
            let status = resp.status();
            let body: MessagesResponse = if status.is_success() {
                resp.json().await.map_err(net_err)?
            } else {
                return Err(RuntimeError::ToolFailed(format!(
                    "anthropic http {}: {}",
                    status,
                    resp.text().await.unwrap_or_default()
                )));
            };
            let text = body.content.into_iter().fold(String::new(), |mut acc, b| {
                if let ContentBlock::Text { text } = b {
                    acc.push_str(&text);
                }
                acc
            });
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
                _ = cancel_for_task.cancelled() => return Err(RuntimeError::Cancelled("anthropic cancelled before send".into())),
                r = request.send() => r.map_err(net_err)?,
            };
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(RuntimeError::ToolFailed(format!(
                    "anthropic http {status}: {body}"
                )));
            }

            let mut stream = resp.bytes_stream().eventsource();
            let mut acc = String::new();
            let mut cumulative = 0u64;
            while let Some(event) = tokio::select! {
                biased;
                _ = cancel_for_task.cancelled() => None,
                next = stream.next() => next,
            } {
                let event = event.map_err(|e| RuntimeError::ToolFailed(format!("sse: {e}")))?;
                if event.data.is_empty() {
                    continue;
                }
                let parsed: serde_json::Value = match serde_json::from_str(&event.data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let ty = parsed.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match ty {
                    "content_block_delta" => {
                        if let Some(text) = parsed.pointer("/delta/text").and_then(|v| v.as_str()) {
                            acc.push_str(text);
                            cumulative += estimate_tokens(text);
                            let _ = tx.send(NodeEvent::LlmChunk {
                                text: text.to_string(),
                                cumulative_tokens: cumulative,
                            });
                        }
                    }
                    "message_delta" => {
                        if let Some(out) = parsed
                            .pointer("/usage/output_tokens")
                            .and_then(|v| v.as_u64())
                        {
                            cumulative = out;
                        }
                    }
                    "message_stop" => break,
                    _ => {}
                }
            }
            if cancel_for_task.is_cancelled() {
                let _ = tx.send(NodeEvent::LlmDone {
                    total_tokens: cumulative,
                });
                return Err(RuntimeError::Cancelled(
                    "anthropic cancelled mid-stream".into(),
                ));
            }
            let _ = tx.send(NodeEvent::LlmDone {
                total_tokens: cumulative,
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
    RuntimeError::ToolFailed(format!("anthropic net: {e}"))
}

#[derive(Serialize)]
struct MessagesRequest {
    model: String,
    max_tokens: u32,
    stream: bool,
    messages: Vec<Message>,
}

#[derive(Serialize)]
struct Message {
    role: &'static str,
    content: MessageContent,
}

#[derive(Serialize)]
#[serde(untagged)]
enum MessageContent {
    Text(String),
    Blocks(Vec<ContentPart>),
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentPart {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    Image {
        source: ImageSource,
    },
}

#[derive(Serialize)]
struct ImageSource {
    #[serde(rename = "type")]
    kind: &'static str,
    media_type: String,
    data: String,
}

#[derive(Serialize)]
struct CacheControl {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text {
        text: String,
    },
    #[serde(other)]
    Other,
}
