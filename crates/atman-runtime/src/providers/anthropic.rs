use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::error::RuntimeError;
use crate::event::{NodeEvent, Observable};
use crate::message::{Message, MessageOrigin, MessagePart, MessageRole};
use crate::provider::{
    AssistantMessage, CallTiming, DEFAULT_STREAM_BUFFER, LlmRequest, Provider, ReasoningEffort,
    ReasoningSelection, ReasoningWireProfile, StopReason, TokenUsage, estimate_tokens,
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

    fn validate_reasoning(&self, selection: &ReasoningSelection) -> Result<(), RuntimeError> {
        ReasoningWireProfile::AnthropicMessages
            .validate(selection, Some(self.max_tokens))
            .map_err(|error| RuntimeError::ToolFailed(format!("invalid request: {error}")))
    }

    fn build_body(&self, req: &LlmRequest, stream: bool) -> Result<MessagesRequest, RuntimeError> {
        let raw_wire: Vec<WireMessage> = req
            .messages
            .iter()
            .map(|m| build_wire_message(m, false, &req.tools))
            .collect::<Result<_, _>>()?;
        let wire_messages = merge_consecutive_same_role(raw_wire);
        let tools: Vec<WireTool> = req
            .tools
            .iter()
            .map(|t| WireTool {
                name: crate::tool_naming::to_wire(&t.name),
                description: t.description.clone(),
                input_schema: t.input_schema.clone(),
            })
            .collect();
        let (thinking, output_config) = anthropic_reasoning(&req.reasoning);
        Ok(MessagesRequest {
            model: req.model.clone(),
            max_tokens: self.max_tokens,
            stream,
            system: req.system.clone(),
            messages: wire_messages,
            tools,
            thinking,
            output_config,
            cache_control: if req.cache_prompt {
                Some(CacheControl { kind: "ephemeral" })
            } else {
                None
            },
        })
    }

    fn build_request(
        &self,
        req: &LlmRequest,
        stream: bool,
    ) -> Result<reqwest::RequestBuilder, RuntimeError> {
        let body = self.build_body(req, stream)?;
        Ok(self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", &self.anthropic_version)
            .json(&body))
    }

    #[doc(hidden)]
    pub fn wire_body_bytes(&self, req: &LlmRequest, stream: bool) -> Vec<u8> {
        serde_json::to_vec(
            &self
                .build_body(req, stream)
                .expect("build Anthropic wire body"),
        )
        .expect("serialize wire body")
    }
}

fn anthropic_reasoning(
    selection: &ReasoningSelection,
) -> (Option<ThinkingConfig>, Option<OutputConfig>) {
    match selection {
        ReasoningSelection::ProviderDefault | ReasoningSelection::Disabled => (None, None),
        ReasoningSelection::Auto { .. } => (
            Some(ThinkingConfig {
                kind: "adaptive",
                budget_tokens: None,
            }),
            None,
        ),
        ReasoningSelection::Effort {
            effort: ReasoningEffort::None,
            ..
        } => (None, None),
        ReasoningSelection::Effort { effort, .. } => (
            Some(ThinkingConfig {
                kind: "adaptive",
                budget_tokens: None,
            }),
            Some(OutputConfig {
                effort: effort.to_string(),
            }),
        ),
        ReasoningSelection::BudgetTokens { tokens } => (
            Some(ThinkingConfig {
                kind: "enabled",
                budget_tokens: Some(*tokens),
            }),
            None,
        ),
    }
}

fn build_wire_message(
    m: &Message,
    apply_cache_control: bool,
    tools: &[crate::tool::ToolSpec],
) -> Result<WireMessage, RuntimeError> {
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
                let data = crate::attachment_store::image_base64(source)?;
                ContentPart::Image {
                    source: ImageSourceWire {
                        kind: "base64",
                        media_type: source.media_type.clone(),
                        data,
                    },
                }
            }
            MessagePart::ToolUse {
                id,
                name,
                input,
                intent,
            } => ContentPart::ToolUse {
                id: id.clone(),
                name: crate::tool_naming::to_wire(name),
                input: crate::message::encode_tool_call_input(input, intent.as_ref(), name, tools),
            },
            MessagePart::Thinking {
                thinking,
                signature,
            } => {
                if signature.is_none() {
                    continue;
                }
                ContentPart::Thinking {
                    thinking: thinking.clone(),
                    signature: signature.clone(),
                }
            }
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
    Ok(WireMessage {
        role,
        content: MessageContent::Blocks(blocks),
    })
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
        if let Err(error) = self.validate_reasoning(&req.reasoning) {
            return Box::pin(async move { Err(error) });
        }
        let request = match self.build_request(&req, false) {
            Ok(request) => request,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
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
        let preflight = self
            .validate_reasoning(&req.reasoning)
            .and_then(|()| self.build_request(&req, true));
        let turn_id = next_turn_id_from_req(&req);
        let tools: Vec<crate::tool::ToolSpec> = req.tools.clone();
        let (tx, events) = broadcast::channel(DEFAULT_STREAM_BUFFER);
        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();
        let output: BoxFut<'static, Result<AssistantMessage, RuntimeError>> = Box::pin(
            async move {
                let request = preflight?;
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
                            if let Some(block) = parsed.get("content_block") {
                                match block.get("type").and_then(|v| v.as_str()) {
                                    Some("tool_use") => {
                                        if let (Some(id), Some(name)) = (
                                            block.get("id").and_then(|v| v.as_str()),
                                            block.get("name").and_then(|v| v.as_str()),
                                        ) {
                                            tool_use_partial.push(PartialToolUse {
                                                id: id.to_string(),
                                                name: name.to_string(),
                                                input_json: String::new(),
                                            });
                                        }
                                    }
                                    Some("thinking") => {
                                        if let Some(sig) =
                                            block.get("signature").and_then(|v| v.as_str())
                                        {
                                            acc_signature = Some(sig.to_string());
                                        }
                                    }
                                    _ => {}
                                }
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
                    if req.reasoning.enabled() && acc_signature.is_none() {
                        return Err(RuntimeError::ThinkingSignatureMissing);
                    }
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
                    let name = crate::tool_naming::from_wire(&pu.name, &tools);
                    let (input, intent) =
                        crate::message::decode_tool_call_input(input, &name, &tools);
                    parts.push(MessagePart::ToolUse {
                        id: pu.id,
                        name,
                        input,
                        intent,
                    });
                }
                Ok(AssistantMessage {
                    message: Message {
                        role: MessageRole::Assistant,
                        parts,
                        turn_id,
                        origin: MessageOrigin::User,
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

    fn test_connection(&self) -> BoxFut<'_, Result<String, String>> {
        let base_url = self.base_url.clone();
        let api_key = self.api_key.clone();
        let name = self.name.clone();
        Box::pin(async move {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .map_err(|e| e.to_string())?;
            let resp = client
                .get(format!("{}/v1/models", base_url.trim_end_matches('/')))
                .header("x-api-key", &api_key)
                .header("anthropic-version", "2023-06-01")
                .send()
                .await
                .map_err(|e| format!("connection failed — {e}"))?;
            let status = resp.status();
            if status.is_success() {
                Ok(format!("\"{name}\" responded OK"))
            } else {
                let body = resp.text().await.unwrap_or_default();
                Err(format!(
                    "returned {status} — {}",
                    crate::provider::bounded_utf8_prefix(&body, 200)
                ))
            }
        })
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
            ContentBlock::ToolUse { id, name, input } => {
                let name = crate::tool_naming::from_wire(&name, tools);
                let (input, intent) = crate::message::decode_tool_call_input(input, &name, tools);
                parts.push(MessagePart::ToolUse {
                    id,
                    name,
                    input,
                    intent,
                });
            }
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
            origin: MessageOrigin::User,
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
    output_config: Option<OutputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Serialize, Clone)]
struct ThinkingConfig {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    budget_tokens: Option<u32>,
}

#[derive(Serialize, Clone)]
struct OutputConfig {
    effort: String,
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

#[cfg(test)]
mod tests {
    use super::*;

    struct IntentTool;

    impl crate::tool::Tool for IntentTool {
        fn name(&self) -> &str {
            "probe"
        }

        fn tier(&self) -> crate::tool::Tier {
            crate::tool::Tier::Zero
        }

        fn call<'a>(
            &'a self,
            _args: crate::tool::ToolArgs,
            _ctx: &'a crate::tool::ToolCtx,
        ) -> crate::tool::BoxFut<'a, crate::tool::ToolResult> {
            Box::pin(async { Ok(crate::Value::Unit) })
        }
    }

    #[test]
    fn tool_call_intent_round_trips_through_tool_use_input() {
        let tools = vec![crate::tool::tool_spec(&IntentTool)];
        let message = Message {
            role: MessageRole::Assistant,
            parts: vec![MessagePart::ToolUse {
                id: "call-1".into(),
                name: "probe".into(),
                input: serde_json::json!({"value": 1}),
                intent: crate::message::ToolCallIntent::new("Inspect provider state"),
            }],
            turn_id: crate::event::TurnId::now(),
            origin: crate::message::MessageOrigin::User,
        };
        let wire =
            serde_json::to_value(build_wire_message(&message, false, &tools).unwrap()).unwrap();
        assert_eq!(
            wire["content"][0]["input"]["_atman_intent"],
            "Inspect provider state"
        );

        let assistant = response_to_assistant(
            MessagesResponse {
                content: vec![ContentBlock::ToolUse {
                    id: "call-1".into(),
                    name: "probe".into(),
                    input: wire["content"][0]["input"].clone(),
                }],
                stop_reason: Some("tool_use".into()),
                usage: None,
                model: None,
                id: None,
            },
            crate::event::TurnId::now(),
            &tools,
        );
        assert!(matches!(
            assistant.message.parts.as_slice(),
            [MessagePart::ToolUse { input, intent: Some(intent), .. }]
                if input == &serde_json::json!({"value": 1})
                    && intent.as_str() == "Inspect provider state"
        ));
    }
}
