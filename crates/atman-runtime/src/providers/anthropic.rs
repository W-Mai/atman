use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::error::RuntimeError;
use crate::event::{NodeEvent, Observable};
use crate::message::{ImageData, Message, MessagePart, MessageRole};
use crate::provider::{
    AssistantMessage, CallTiming, DEFAULT_STREAM_BUFFER, LlmRequest, Provider, StopReason,
    TokenUsage, estimate_tokens,
};
use crate::providers::classify_attachment_error;
use crate::tool::BoxFut;

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
            max_tokens: 16384,
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

    fn build_body(&self, req: &LlmRequest, stream: bool) -> MessagesRequest {
        let raw_wire: Vec<WireMessage> = req
            .messages
            .iter()
            .map(|m| build_wire_message(m, false))
            .collect();
        let wire_messages = merge_consecutive_same_role(raw_wire);
        let tools: Vec<WireTool> = req
            .tools
            .iter()
            .map(|t| WireTool {
                name: name_to_provider(&t.name),
                description: t.description.clone(),
                input_schema: t.input_schema.clone(),
            })
            .collect();
        MessagesRequest {
            model: req.model.clone(),
            max_tokens: self.max_tokens,
            stream,
            system: req.system.clone(),
            messages: wire_messages,
            tools,
            thinking: if req.thinking_enabled {
                Some(ThinkingConfig {
                    kind: "enabled",
                    budget_tokens: self.max_tokens.saturating_sub(4096).max(1024),
                })
            } else {
                None
            },
            cache_control: if req.cache_prompt {
                Some(CacheControl { kind: "ephemeral" })
            } else {
                None
            },
        }
    }

    fn build_request(&self, req: &LlmRequest, stream: bool) -> reqwest::RequestBuilder {
        let body = self.build_body(req, stream);
        self.client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", &self.anthropic_version)
            .json(&body)
    }

    #[doc(hidden)]
    pub fn wire_body_bytes(&self, req: &LlmRequest, stream: bool) -> Vec<u8> {
        serde_json::to_vec(&self.build_body(req, stream)).expect("serialize wire body")
    }
}

fn name_to_provider(flow_name: &str) -> String {
    flow_name.replace('.', "_")
}

fn name_from_provider(native: &str, tools: &[crate::tool::ToolSpec]) -> String {
    for t in tools {
        if name_to_provider(&t.name) == native {
            return t.name.clone();
        }
    }
    native.to_string()
}

fn build_wire_message(m: &Message, apply_cache_control: bool) -> WireMessage {
    let role = match m.role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::System => "user",
        MessageRole::Tool => "user",
    };
    let mut blocks: Vec<ContentPart> = Vec::with_capacity(m.parts.len());
    let last_idx = m.parts.len().saturating_sub(1);
    for (i, part) in m.parts.iter().enumerate() {
        blocks.push(match part {
            MessagePart::CompactSummary { summary, .. } => ContentPart::Text {
                text: summary.clone(),
                cache_control: if apply_cache_control && i == last_idx {
                    Some(CacheControl { kind: "ephemeral" })
                } else {
                    None
                },
            },
            MessagePart::Text { text } => ContentPart::Text {
                text: text.clone(),
                cache_control: if apply_cache_control && i == last_idx {
                    Some(CacheControl { kind: "ephemeral" })
                } else {
                    None
                },
            },
            MessagePart::Image { source } => {
                let data = match &source.data {
                    ImageData::Base64 { data } => data.clone(),
                    ImageData::Path { path } => {
                        let bytes = std::fs::read(path).unwrap_or_default();
                        use base64::Engine;
                        base64::engine::general_purpose::STANDARD.encode(&bytes)
                    }
                };
                ContentPart::Image {
                    source: ImageSourceWire {
                        kind: "base64",
                        media_type: source.media_type.clone(),
                        data,
                    },
                }
            }
            MessagePart::ToolUse { id, name, input } => ContentPart::ToolUse {
                id: id.clone(),
                name: name_to_provider(name),
                input: input.clone(),
            },
            MessagePart::Thinking {
                thinking,
                signature,
            } => ContentPart::Thinking {
                thinking: thinking.clone(),
                signature: signature.clone(),
            },
            MessagePart::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => ContentPart::ToolResult {
                tool_use_id: tool_use_id.clone(),
                content: content.clone(),
                is_error: *is_error,
            },
        });
    }
    WireMessage {
        role,
        content: MessageContent::Blocks(blocks),
    }
}

fn merge_consecutive_same_role(wire: Vec<WireMessage>) -> Vec<WireMessage> {
    let mut out: Vec<WireMessage> = Vec::with_capacity(wire.len());
    for msg in wire {
        let WireMessage { role, content } = msg;
        let mut content = Some(content);
        if let Some(last) = out.last_mut()
            && last.role == role
            && let Some(msg_content) = content.take()
        {
            let MessageContent::Blocks(last_blocks) = &mut last.content;
            let MessageContent::Blocks(msg_blocks) = msg_content;
            last_blocks.extend(msg_blocks);
        }
        if let Some(content) = content {
            out.push(WireMessage { role, content });
        }
    }
    out
}

impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn call<'a>(&'a self, req: LlmRequest) -> BoxFut<'a, Result<AssistantMessage, RuntimeError>> {
        let request = self.build_request(&req, false);
        Box::pin(async move {
            let resp = request.send().await.map_err(net_err)?;
            let status = resp.status();
            let body: MessagesResponse = if status.is_success() {
                resp.json().await.map_err(net_err)?
            } else {
                let body_text = resp.text().await.unwrap_or_default();
                if let Some(reason) = classify_attachment_error(status.as_u16(), &body_text) {
                    return Err(RuntimeError::AttachmentError { reason });
                }
                return Err(RuntimeError::ToolFailed(format!(
                    "anthropic http {status}: {body_text}"
                )));
            };
            Ok(response_to_assistant(
                body,
                next_turn_id_from_req(&req),
                &req.tools,
            ))
        })
    }

    fn call_streaming(&self, req: LlmRequest) -> Observable<AssistantMessage> {
        let request = self.build_request(&req, true);
        let turn_id = next_turn_id_from_req(&req);
        let tools: Vec<crate::tool::ToolSpec> = req.tools.clone();
        let (tx, events) = broadcast::channel(DEFAULT_STREAM_BUFFER);
        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();
        let output: BoxFut<'static, Result<AssistantMessage, RuntimeError>> = Box::pin(
            async move {
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
                    if let Some(reason) = classify_attachment_error(status.as_u16(), &body) {
                        return Err(RuntimeError::AttachmentError { reason });
                    }
                    return Err(RuntimeError::ToolFailed(format!(
                        "anthropic http {status}: {body}"
                    )));
                }

                let mut stream = resp.bytes_stream().eventsource();
                let mut acc_text = String::new();
                let mut acc_thinking = String::new();
                let mut acc_signature: Option<String> = None;
                let mut cumulative = 0u64;
                let mut input_tokens: u64 = 0;
                let mut cache_read_tokens: u64 = 0;
                let mut cache_write_tokens: u64 = 0;
                let mut tool_use_partial: Vec<PartialToolUse> = Vec::new();
                let mut stop_reason = StopReason::End;
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
                        "message_start" => {
                            if let Some(usage) = parsed.pointer("/message/usage") {
                                input_tokens = usage
                                    .get("input_tokens")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                cache_read_tokens = usage
                                    .get("cache_read_input_tokens")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                cache_write_tokens = usage
                                    .get("cache_creation_input_tokens")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                            }
                        }
                        "content_block_start" => {
                            if let Some(block) = parsed.get("content_block")
                                && block.get("type").and_then(|v| v.as_str()) == Some("tool_use")
                                && let (Some(id), Some(name)) = (
                                    block.get("id").and_then(|v| v.as_str()),
                                    block.get("name").and_then(|v| v.as_str()),
                                )
                            {
                                tool_use_partial.push(PartialToolUse {
                                    id: id.to_string(),
                                    name: name.to_string(),
                                    input_json: String::new(),
                                });
                            }
                        }
                        "content_block_delta" => {
                            if let Some(delta) = parsed.get("delta") {
                                let delta_ty =
                                    delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
                                if delta_ty == "text_delta" {
                                    if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                                        acc_text.push_str(text);
                                        cumulative += estimate_tokens(text);
                                        let _ = tx.send(NodeEvent::LlmChunk {
                                            text: text.to_string(),
                                            cumulative_tokens: cumulative,
                                        });
                                    }
                                } else if delta_ty == "thinking_delta" {
                                    if let Some(text) =
                                        delta.get("thinking").and_then(|v| v.as_str())
                                    {
                                        acc_thinking.push_str(text);
                                        let _ = tx.send(NodeEvent::ThinkingChunk {
                                            text: text.to_string(),
                                        });
                                    }
                                } else if let Some(text) =
                                    delta.get("reasoning_content").and_then(|v| v.as_str())
                                {
                                    acc_thinking.push_str(text);
                                    let _ = tx.send(NodeEvent::ThinkingChunk {
                                        text: text.to_string(),
                                    });
                                } else if delta_ty == "signature_delta" {
                                    if let Some(sig) =
                                        delta.get("signature").and_then(|v| v.as_str())
                                    {
                                        acc_signature = Some(sig.to_string());
                                    }
                                } else if delta_ty == "input_json_delta"
                                    && let Some(partial) =
                                        delta.get("partial_json").and_then(|v| v.as_str())
                                    && let Some(last) = tool_use_partial.last_mut()
                                {
                                    last.input_json.push_str(partial);
                                }
                            }
                        }
                        "message_delta" => {
                            if let Some(out) = parsed
                                .pointer("/usage/output_tokens")
                                .and_then(|v| v.as_u64())
                            {
                                cumulative = out;
                            }
                            if let Some(inp) = parsed
                                .pointer("/usage/input_tokens")
                                .and_then(|v| v.as_u64())
                            {
                                input_tokens = inp;
                            }
                            if let Some(cr) = parsed
                                .pointer("/usage/cache_read_input_tokens")
                                .and_then(|v| v.as_u64())
                            {
                                cache_read_tokens = cr;
                            }
                            if let Some(cw) = parsed
                                .pointer("/usage/cache_creation_input_tokens")
                                .and_then(|v| v.as_u64())
                            {
                                cache_write_tokens = cw;
                            }
                            if let Some(reason) = parsed
                                .pointer("/delta/stop_reason")
                                .and_then(|v| v.as_str())
                            {
                                stop_reason = parse_stop_reason(reason);
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

                let mut parts: Vec<MessagePart> = Vec::new();
                if !acc_thinking.is_empty() {
                    parts.push(MessagePart::Thinking {
                        thinking: acc_thinking,
                        signature: acc_signature,
                    });
                }
                if !acc_text.is_empty() {
                    parts.push(MessagePart::Text { text: acc_text });
                }
                for pu in tool_use_partial {
                    let input: serde_json::Value = if pu.input_json.is_empty() {
                        serde_json::Value::Object(Default::default())
                    } else {
                        serde_json::from_str(&pu.input_json).unwrap_or(serde_json::Value::Null)
                    };
                    parts.push(MessagePart::ToolUse {
                        id: pu.id,
                        name: name_from_provider(&pu.name, &tools),
                        input,
                    });
                }
                Ok(AssistantMessage {
                    message: Message {
                        role: MessageRole::Assistant,
                        parts,
                        turn_id,
                    },
                    stop_reason,
                    token_usage: TokenUsage {
                        input: input_tokens,
                        cached_input: cache_read_tokens,
                        output: cumulative,
                        cache_write: cache_write_tokens,
                        ..Default::default()
                    },
                    timing: CallTiming::default(),
                    model: String::new(),
                    response_id: None,
                })
            },
        );
        Observable {
            output,
            events,
            cancel,
        }
    }
}

struct PartialToolUse {
    id: String,
    name: String,
    input_json: String,
}

fn response_to_assistant(
    body: MessagesResponse,
    turn_id: crate::event::TurnId,
    tools: &[crate::tool::ToolSpec],
) -> AssistantMessage {
    let mut parts: Vec<MessagePart> = Vec::new();
    for block in body.content {
        match block {
            ContentBlock::Text { text } => parts.push(MessagePart::Text { text }),
            ContentBlock::Thinking {
                thinking,
                signature,
            } => parts.push(MessagePart::Thinking {
                thinking,
                signature,
            }),
            ContentBlock::ToolUse { id, name, input } => parts.push(MessagePart::ToolUse {
                id,
                name: name_from_provider(&name, tools),
                input,
            }),
            ContentBlock::Other => {}
        }
    }
    let stop_reason = body
        .stop_reason
        .as_deref()
        .map(parse_stop_reason)
        .unwrap_or(StopReason::End);
    let usage = body
        .usage
        .map(|u| TokenUsage {
            input: u.input_tokens.unwrap_or(0),
            cached_input: u.cache_read_input_tokens.unwrap_or(0),
            output: u.output_tokens.unwrap_or(0),
            cache_write: u.cache_creation_input_tokens.unwrap_or(0),
            ..Default::default()
        })
        .unwrap_or_default();
    AssistantMessage {
        message: Message {
            role: MessageRole::Assistant,
            parts,
            turn_id,
        },
        stop_reason,
        token_usage: usage,
        timing: CallTiming::default(),
        model: body.model.unwrap_or_default(),
        response_id: body.id,
    }
}

fn parse_stop_reason(s: &str) -> StopReason {
    match s {
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::Length,
        _ => StopReason::End,
    }
}

fn next_turn_id_from_req(req: &LlmRequest) -> crate::event::TurnId {
    req.messages
        .first()
        .map(|m| m.turn_id.clone())
        .unwrap_or_else(crate::event::TurnId::now)
}

fn net_err(e: reqwest::Error) -> RuntimeError {
    RuntimeError::ToolFailed(format!("anthropic net: {e}"))
}

#[derive(Serialize, Clone)]
struct MessagesRequest {
    model: String,
    max_tokens: u32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<ThinkingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Serialize, Clone)]
struct ThinkingConfig {
    #[serde(rename = "type")]
    kind: &'static str,
    budget_tokens: u32,
}

#[derive(Serialize, Clone)]
struct WireTool {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    input_schema: serde_json::Value,
}

#[derive(Serialize, Clone)]
struct WireMessage {
    role: &'static str,
    content: MessageContent,
}

#[derive(Serialize, Clone)]
#[serde(untagged)]
enum MessageContent {
    Blocks(Vec<ContentPart>),
}

#[derive(Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentPart {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    Thinking {
        thinking: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    Image {
        source: ImageSourceWire,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "core::ops::Not::not")]
        is_error: bool,
    },
}

#[derive(Serialize, Clone)]
struct ImageSourceWire {
    #[serde(rename = "type")]
    kind: &'static str,
    media_type: String,
    data: String,
}

#[derive(Serialize, Clone)]
struct CacheControl {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    id: Option<String>,
}

#[derive(Deserialize, Default)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        #[serde(default)]
        signature: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(other)]
    Other,
}
