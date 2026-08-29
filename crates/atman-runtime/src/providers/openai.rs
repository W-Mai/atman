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

pub struct OpenAiProvider {
    name: String,
    api_key: String,
    base_url: String,
    client: reqwest::Client,
    max_tokens: Option<u32>,
    reasoning_format: OpenAiReasoningFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum OpenAiReasoningFormat {
    #[serde(rename = "reasoning-effort", alias = "official")]
    Official,
    #[default]
    #[serde(rename = "thinking-toggle", alias = "compatible-thinking")]
    CompatibleThinking,
}

impl OpenAiReasoningFormat {
    pub fn for_provider_kind(kind: &str) -> Self {
        if kind == "openai" {
            Self::Official
        } else {
            Self::CompatibleThinking
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Official => "reasoning-effort",
            Self::CompatibleThinking => "thinking-toggle",
        }
    }
}

impl std::fmt::Display for OpenAiReasoningFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for OpenAiReasoningFormat {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "reasoning-effort" | "official" => Ok(Self::Official),
            "thinking-toggle" | "compatible-thinking" => Ok(Self::CompatibleThinking),
            _ => Err(format!(
                "invalid reasoning format `{value}`; expected `reasoning-effort` or `thinking-toggle`"
            )),
        }
    }
}

impl OpenAiProvider {
    pub fn new(name: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            api_key: api_key.into(),
            base_url: "https://api.openai.com/v1".into(),
            client: reqwest::Client::new(),
            max_tokens: None,
            reasoning_format: OpenAiReasoningFormat::default(),
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

    pub fn with_reasoning_format(mut self, format: OpenAiReasoningFormat) -> Self {
        self.reasoning_format = format;
        self
    }

    fn validate_reasoning(&self, selection: &ReasoningSelection) -> Result<(), RuntimeError> {
        let profile = match self.reasoning_format {
            OpenAiReasoningFormat::Official => ReasoningWireProfile::OpenAiOfficial,
            OpenAiReasoningFormat::CompatibleThinking => ReasoningWireProfile::CompatibleThinking,
        };
        profile
            .validate(selection, self.max_tokens)
            .map_err(|error| RuntimeError::ToolFailed(format!("invalid request: {error}")))
    }

    fn build_body(
        &self,
        req: &LlmRequest,
        stream: bool,
    ) -> Result<ChatCompletionsRequest, RuntimeError> {
        let mut wire_messages: Vec<ChatMessage> = Vec::new();
        if let Some(sys) = &req.system {
            wire_messages.push(ChatMessage {
                role: "system",
                content: Some(ChatContent::Text(sys.clone())),
                tool_calls: None,
                tool_call_id: None,
            });
        }
        for m in &req.messages {
            wire_messages.push(build_wire_message(m)?);
        }
        let tools: Vec<WireToolSpec> = req
            .tools
            .iter()
            .map(|t| WireToolSpec {
                kind: "function",
                function: WireToolFunction {
                    name: crate::tool_naming::to_wire(&t.name),
                    description: t.description.clone(),
                    parameters: t.input_schema.clone(),
                },
            })
            .collect();
        let (reasoning_effort, thinking) = match self.reasoning_format {
            OpenAiReasoningFormat::Official => (official_reasoning_effort(&req.reasoning), None),
            OpenAiReasoningFormat::CompatibleThinking => {
                (None, compatible_thinking(&req.reasoning))
            }
        };
        Ok(ChatCompletionsRequest {
            model: req.model.clone(),
            stream,
            max_tokens: (self.reasoning_format == OpenAiReasoningFormat::CompatibleThinking)
                .then_some(self.max_tokens)
                .flatten(),
            max_completion_tokens: (self.reasoning_format == OpenAiReasoningFormat::Official)
                .then_some(self.max_tokens)
                .flatten(),
            messages: wire_messages,
            tools,
            stream_options: if stream {
                Some(StreamOptions {
                    include_usage: true,
                })
            } else {
                None
            },
            reasoning_effort,
            thinking,
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
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body))
    }

    #[doc(hidden)]
    pub fn wire_body_bytes(&self, req: &LlmRequest, stream: bool) -> Vec<u8> {
        serde_json::to_vec(
            &self
                .build_body(req, stream)
                .expect("build OpenAI wire body"),
        )
        .expect("serialize wire body")
    }
}

fn official_reasoning_effort(selection: &ReasoningSelection) -> Option<String> {
    match selection {
        ReasoningSelection::Disabled => Some(ReasoningEffort::None.to_string()),
        ReasoningSelection::Effort { effort, .. } => Some(effort.to_string()),
        ReasoningSelection::ProviderDefault
        | ReasoningSelection::Auto { .. }
        | ReasoningSelection::BudgetTokens { .. } => None,
    }
}

fn compatible_thinking(selection: &ReasoningSelection) -> Option<ThinkingConfig> {
    match selection {
        ReasoningSelection::ProviderDefault => None,
        ReasoningSelection::Disabled
        | ReasoningSelection::Effort {
            effort: ReasoningEffort::None,
            ..
        } => Some(ThinkingConfig { kind: "disabled" }),
        ReasoningSelection::Auto { .. }
        | ReasoningSelection::Effort { .. }
        | ReasoningSelection::BudgetTokens { .. } => Some(ThinkingConfig { kind: "enabled" }),
    }
}

fn build_wire_message(m: &Message) -> Result<ChatMessage, RuntimeError> {
    Ok(match m.role {
        MessageRole::System => ChatMessage {
            role: "system",
            content: Some(ChatContent::Text(m.text_concat())),
            tool_calls: None,
            tool_call_id: None,
        },
        MessageRole::Tool => {
            let (id, content) = extract_tool_result(m);
            ChatMessage {
                role: "tool",
                content: Some(ChatContent::Text(content)),
                tool_calls: None,
                tool_call_id: Some(id),
            }
        }
        MessageRole::Assistant => {
            let (text_parts, tool_uses) = split_assistant_parts(&m.parts);
            let content = if text_parts.is_empty() {
                None
            } else {
                Some(ChatContent::Text(text_parts.join("")))
            };
            let tool_calls = if tool_uses.is_empty() {
                None
            } else {
                Some(tool_uses)
            };
            ChatMessage {
                role: "assistant",
                content,
                tool_calls,
                tool_call_id: None,
            }
        }
        MessageRole::User => {
            let parts = build_user_parts(&m.parts)?;
            let content = if parts.iter().all(|p| matches!(p, ChatPart::Text { .. })) {
                let joined: String = parts
                    .iter()
                    .filter_map(|p| match p {
                        ChatPart::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                Some(ChatContent::Text(joined))
            } else {
                Some(ChatContent::Parts(parts))
            };
            ChatMessage {
                role: "user",
                content,
                tool_calls: None,
                tool_call_id: None,
            }
        }
    })
}

fn build_user_parts(parts: &[MessagePart]) -> Result<Vec<ChatPart>, RuntimeError> {
    let mut out = Vec::with_capacity(parts.len());
    for p in parts {
        match p {
            MessagePart::CompactSummary { summary, .. } => out.push(ChatPart::Text {
                text: summary.clone(),
            }),
            MessagePart::Text { text } => out.push(ChatPart::Text { text: text.clone() }),
            MessagePart::Image { source } => {
                let data = crate::attachment_store::image_base64(source)?;
                let url = format!("data:{};base64,{}", source.media_type, data);
                out.push(ChatPart::ImageUrl {
                    image_url: ImageUrl {
                        url,
                        detail: (!matches!(source.detail, crate::provider::ImageDetail::Auto))
                            .then(|| source.detail.as_str()),
                    },
                });
            }
            _ => {}
        }
    }
    Ok(out)
}

fn extract_tool_result(m: &Message) -> (String, String) {
    for p in &m.parts {
        if let MessagePart::ToolResult {
            tool_use_id,
            content,
            ..
        } = p
        {
            return (tool_use_id.clone(), content.clone());
        }
    }
    (String::new(), m.text_concat())
}

fn split_assistant_parts(parts: &[MessagePart]) -> (Vec<String>, Vec<WireToolCall>) {
    let mut text = Vec::new();
    let mut tools = Vec::new();
    for p in parts {
        match p {
            MessagePart::Text { text: t } => text.push(t.clone()),
            MessagePart::ToolUse { id, name, input } => tools.push(WireToolCall {
                id: id.clone(),
                kind: "function",
                function: WireFunctionCall {
                    name: crate::tool_naming::to_wire(name),
                    arguments: input.to_string(),
                },
            }),
            _ => {}
        }
    }
    (text, tools)
}

impl Provider for OpenAiProvider {
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
        let turn_id = next_turn_id_from_req(&req);
        Box::pin(async move {
            let resp = request.send().await.map_err(net_err)?;
            let status = resp.status();
            let body: ChatCompletionsResponse = if status.is_success() {
                resp.json().await.map_err(net_err)?
            } else {
                let body_text = resp.text().await.unwrap_or_default();
                if let Some(reason) = classify_attachment_error(status.as_u16(), &body_text) {
                    return Err(RuntimeError::AttachmentError { reason });
                }
                return Err(RuntimeError::ToolFailed(format!(
                    "openai http {status}: {body_text}"
                )));
            };
            Ok(response_to_assistant(body, turn_id, &req.tools))
        })
    }

    fn call_streaming(&self, req: LlmRequest) -> Observable<AssistantMessage> {
        let preflight = self
            .validate_reasoning(&req.reasoning)
            .and_then(|()| self.build_request(&req, true));
        let turn_id = next_turn_id_from_req(&req);
        let streaming_tools = req.tools.clone();
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
                    _ = cancel_for_task.cancelled() => return Err(RuntimeError::Cancelled("openai cancelled before send".into())),
                    r = request.send() => r.map_err(net_err)?,
                };
                let status = resp.status();
                if !status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    if let Some(reason) = classify_attachment_error(status.as_u16(), &body) {
                        return Err(RuntimeError::AttachmentError { reason });
                    }
                    return Err(RuntimeError::ToolFailed(format!(
                        "openai http {status}: {body}"
                    )));
                }

                let mut stream = resp.bytes_stream().eventsource();
                let mut acc_text = String::new();
                let mut acc_thinking = String::new();
                let mut cumulative = 0u64;
                let mut final_usage: Option<OpenAiUsage> = None;
                let mut resp_model: Option<String> = None;
                let mut resp_id: Option<String> = None;
                let mut partial_tool_calls: Vec<PartialToolCall> = Vec::new();
                let mut stop_reason = StopReason::End;
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
                        && !content.is_empty()
                    {
                        acc_text.push_str(content);
                        cumulative += estimate_tokens(content);
                        let _ = tx.send(NodeEvent::LlmChunk {
                            text: content.to_string(),
                            cumulative_tokens: cumulative,
                        });
                    }
                    if let Some(reasoning) = parsed
                        .pointer("/choices/0/delta/reasoning_content")
                        .and_then(|v| v.as_str())
                        && !reasoning.is_empty()
                    {
                        acc_thinking.push_str(reasoning);
                        let _ = tx.send(NodeEvent::ThinkingChunk {
                            text: reasoning.to_string(),
                        });
                    }
                    if let Some(m) = parsed.get("model").and_then(|v| v.as_str()) {
                        resp_model = Some(m.to_string());
                    }
                    if let Some(id) = parsed.get("id").and_then(|v| v.as_str()) {
                        resp_id = Some(id.to_string());
                    }
                    if let Some(tcs) = parsed
                        .pointer("/choices/0/delta/tool_calls")
                        .and_then(|v| v.as_array())
                    {
                        for tc in tcs {
                            let idx =
                                tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                            while partial_tool_calls.len() <= idx {
                                partial_tool_calls.push(PartialToolCall::default());
                            }
                            let slot = &mut partial_tool_calls[idx];
                            if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                                slot.id = id.to_string();
                            }
                            if let Some(name) = tc
                                .pointer("/function/name")
                                .and_then(|v| v.as_str())
                                .filter(|name| !name.is_empty())
                            {
                                slot.name = name.to_string();
                            }
                            if let Some(args) =
                                tc.pointer("/function/arguments").and_then(|v| v.as_str())
                            {
                                slot.arguments.push_str(args);
                            }
                        }
                    }
                    if let Some(reason) = parsed
                        .pointer("/choices/0/finish_reason")
                        .and_then(|v| v.as_str())
                    {
                        stop_reason = parse_stop_reason(reason);
                    }
                    if let Some(usage_obj) = parsed.get("usage") {
                        if !usage_obj.is_null() {
                            final_usage =
                                serde_json::from_value::<OpenAiUsage>(usage_obj.clone()).ok();
                        }
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
                let total = final_usage
                    .as_ref()
                    .and_then(|u| u.completion_tokens)
                    .unwrap_or(cumulative);
                let _ = tx.send(NodeEvent::LlmDone {
                    total_tokens: total,
                });

                let mut parts: Vec<MessagePart> = Vec::new();
                if !acc_thinking.is_empty() {
                    parts.push(MessagePart::Thinking {
                        thinking: acc_thinking,
                        signature: None,
                    });
                }
                if !acc_text.is_empty() {
                    parts.push(MessagePart::Text { text: acc_text });
                }
                for tc in partial_tool_calls {
                    if tc.id.is_empty() && tc.name.is_empty() && tc.arguments.is_empty() {
                        continue;
                    }
                    let input: serde_json::Value = if tc.arguments.is_empty() {
                        serde_json::Value::Object(Default::default())
                    } else {
                        serde_json::from_str(&tc.arguments).unwrap_or(serde_json::Value::Null)
                    };
                    parts.push(MessagePart::ToolUse {
                        id: tc.id,
                        name: crate::tool_naming::from_wire(&tc.name, &streaming_tools),
                        input,
                    });
                }

                let token_usage = if let Some(u) = &final_usage {
                    let cached = u
                        .prompt_tokens_details
                        .as_ref()
                        .and_then(|d| d.cached_tokens)
                        .unwrap_or(0);
                    let cache_write = u
                        .prompt_tokens_details
                        .as_ref()
                        .and_then(|d| d.cache_write_tokens)
                        .unwrap_or(0);
                    let cache_read = cached.max(u.prompt_cache_hit_tokens.unwrap_or(0));
                    let reasoning = u
                        .completion_tokens_details
                        .as_ref()
                        .and_then(|d| d.reasoning_tokens)
                        .unwrap_or(0);
                    TokenUsage {
                        input: u.prompt_tokens.unwrap_or(0).saturating_sub(cache_read),
                        cached_input: cache_read,
                        output: u.completion_tokens.unwrap_or(0),
                        cache_write,
                        reasoning_tokens: reasoning,
                    }
                } else {
                    TokenUsage {
                        output: total,
                        ..Default::default()
                    }
                };

                Ok(AssistantMessage {
                    message: Message {
                        role: MessageRole::Assistant,
                        parts,
                        turn_id,
                        origin: MessageOrigin::User,
                    },
                    stop_reason,
                    token_usage,
                    timing: CallTiming::default(),
                    model: resp_model.unwrap_or_default(),
                    response_id: resp_id,
                })
            },
        );
        Observable {
            output,
            events,
            cancel,
        }
    }

    fn discover_models(
        &self,
    ) -> crate::tool::BoxFut<
        'static,
        Result<Vec<crate::provider::DiscoveredModel>, crate::provider::ModelDiscoveryError>,
    > {
        let base_url = self.base_url.clone();
        let api_key = self.api_key.clone();
        Box::pin(async move {
            #[derive(serde::Deserialize)]
            struct ModelsResponse {
                data: Vec<ModelEntry>,
            }
            #[derive(serde::Deserialize)]
            struct ModelEntry {
                id: String,
            }

            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .map_err(|error| {
                    crate::provider::ModelDiscoveryError::Transport(error.to_string())
                })?;
            let resp = client
                .get(format!("{base_url}/models"))
                .bearer_auth(&api_key)
                .send()
                .await
                .map_err(|error| {
                    crate::provider::ModelDiscoveryError::Transport(error.to_string())
                })?;
            let status = resp.status();
            let bytes = resp.bytes().await.map_err(|error| {
                crate::provider::ModelDiscoveryError::Transport(error.to_string())
            })?;
            if !status.is_success() {
                return Err(crate::provider::ModelDiscoveryError::Http {
                    status: status.as_u16(),
                    body: String::from_utf8_lossy(&bytes).chars().take(512).collect(),
                });
            }
            let body: ModelsResponse = serde_json::from_slice(&bytes).map_err(|error| {
                crate::provider::ModelDiscoveryError::InvalidResponse(error.to_string())
            })?;
            Ok(body
                .data
                .into_iter()
                .filter(|m| !m.id.starts_with("ft:"))
                .map(|m| {
                    let (budget, thinking) =
                        crate::model_registry::lookup_known_model(&m.id).unwrap_or((32_768, false));
                    crate::provider::DiscoveredModel {
                        slug: m.id,
                        context_budget: Some(budget),
                        capability_knowledge: crate::provider::CapabilityKnowledge::Legacy {
                            thinking,
                        },
                    }
                })
                .collect())
        })
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
                .get(format!("{}/models", base_url.trim_end_matches('/')))
                .bearer_auth(&api_key)
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
                    &body[..body.len().min(200)]
                ))
            }
        })
    }
}

#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

fn response_to_assistant(
    body: ChatCompletionsResponse,
    turn_id: crate::event::TurnId,
    tools: &[crate::tool::ToolSpec],
) -> AssistantMessage {
    let mut parts: Vec<MessagePart> = Vec::new();
    let mut stop_reason = StopReason::End;
    if let Some(choice) = body.choices.into_iter().next() {
        if let Some(msg) = choice.message {
            if let Some(content) = msg.content {
                parts.push(MessagePart::Text { text: content });
            }
            if let Some(tool_calls) = msg.tool_calls {
                for tc in tool_calls {
                    let input: serde_json::Value = if tc.function.arguments.is_empty() {
                        serde_json::Value::Object(Default::default())
                    } else {
                        serde_json::from_str(&tc.function.arguments)
                            .unwrap_or(serde_json::Value::Null)
                    };
                    parts.push(MessagePart::ToolUse {
                        id: tc.id,
                        name: crate::tool_naming::from_wire(&tc.function.name, tools),
                        input,
                    });
                }
            }
        }
        if let Some(reason) = choice.finish_reason {
            stop_reason = parse_stop_reason(&reason);
        }
    }
    let usage = body.usage.map(|u| {
        let cached = u
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens)
            .unwrap_or(0);
        let cache_write = u
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cache_write_tokens)
            .unwrap_or(0);
        let cache_read = cached.max(u.prompt_cache_hit_tokens.unwrap_or(0));
        let reasoning = u
            .completion_tokens_details
            .as_ref()
            .and_then(|d| d.reasoning_tokens)
            .unwrap_or(0);
        TokenUsage {
            input: u.prompt_tokens.unwrap_or(0).saturating_sub(cache_read),
            cached_input: cache_read,
            output: u.completion_tokens.unwrap_or(0),
            cache_write,
            reasoning_tokens: reasoning,
        }
    });
    AssistantMessage {
        message: Message {
            role: MessageRole::Assistant,
            parts,
            turn_id,
            origin: MessageOrigin::User,
        },
        stop_reason,
        token_usage: usage.unwrap_or_default(),
        timing: CallTiming::default(),
        model: body.model.unwrap_or_default(),
        response_id: body.id,
    }
}

fn parse_stop_reason(s: &str) -> StopReason {
    match s {
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" => StopReason::Length,
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
    RuntimeError::ToolFailed(format!("openai net: {e}"))
}

#[derive(Serialize)]
struct ChatCompletionsRequest {
    model: String,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireToolSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<ThinkingConfig>,
}

#[derive(Serialize)]
struct ThinkingConfig {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
struct WireToolSpec {
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireToolFunction,
}

#[derive(Serialize)]
struct WireToolFunction {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    parameters: serde_json::Value,
}

#[derive(Serialize)]
struct ChatMessage {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<ChatContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<WireToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ChatContent {
    Text(String),
    Parts(Vec<ChatPart>),
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ChatPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

#[derive(Serialize)]
struct ImageUrl {
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<&'static str>,
}

#[derive(Serialize)]
struct WireToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireFunctionCall,
}

#[derive(Serialize)]
struct WireFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Deserialize)]
struct ChatCompletionsResponse {
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    id: Option<String>,
}

#[derive(Deserialize, Default)]
struct OpenAiUsage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetails>,
    #[serde(default)]
    prompt_cache_hit_tokens: Option<u64>,
    #[serde(default)]
    #[allow(dead_code)]
    prompt_cache_miss_tokens: Option<u64>,
}

#[derive(Deserialize, Default)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
    #[serde(default)]
    cache_write_tokens: Option<u64>,
}

#[derive(Deserialize, Default)]
struct CompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct ChatChoice {
    #[serde(default)]
    message: Option<ChatChoiceMessage>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChatChoiceMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<RespToolCall>>,
}

#[derive(Deserialize)]
struct RespToolCall {
    id: String,
    function: RespFunctionCall,
}

#[derive(Deserialize)]
struct RespFunctionCall {
    name: String,
    #[serde(default)]
    arguments: String,
}
